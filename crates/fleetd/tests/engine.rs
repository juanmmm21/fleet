//! El motor de punta a punta: repos de git de verdad, un `claude` de mentira y SQLite.

use std::time::Duration;

use fleet_core::{AgentProvider, TaskId, TaskSpec, TaskState};
use fleetd::{Engine, EngineError, Event};

mod common;
use common::{TestResult, fixture, git, wait_for};

#[tokio::test(flavor = "multi_thread")]
async fn a_task_goes_from_prompt_to_merged() -> TestResult {
    let fx = fixture()?;
    let engine = Engine::start(fx.config()?).await?;
    let mut events = engine.subscribe();

    let task = engine.create(fx.spec("write the work file")).await?;
    assert_eq!(task.state, TaskState::Provisioning);
    let ready = wait_for(&mut events, &task.id, TaskState::Ready).await?;
    let worktree = ready.worktree.clone().ok_or("no worktree")?;
    assert!(worktree.path.join("work.txt").is_file());
    assert_eq!(ready.session_id.as_deref(), Some("s-1"));
    assert!(ready.preflight.as_ref().is_some_and(|p| p.passed()));
    let diff = ready.diff.clone().ok_or("no diff")?;
    assert_eq!(diff.commits, 1);

    let log = std::fs::read_to_string(engine.config().log_path(&task.id))?;
    assert!(log.contains("== preflight"), "{log}");

    engine.merge(&task.id).await?;
    wait_for(&mut events, &task.id, TaskState::Merged).await?;
    assert!(fx.origin_has("work.txt")?);
    // Integrada, su carpeta sobra
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while worktree.path.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "worktree left behind"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_question_waits_for_the_answer() -> TestResult {
    let fx = fixture()?;
    let engine = Engine::start(fx.config()?).await?;
    let mut events = engine.subscribe();

    let task = engine.create(fx.spec("ask-first, then write")).await?;
    let waiting = wait_for(&mut events, &task.id, TaskState::Waiting).await?;
    assert_eq!(waiting.question.as_deref(), Some("which file?"));
    assert!(matches!(
        engine.merge(&task.id).await,
        Err(EngineError::Transition(_))
    ));

    engine.answer(&task.id, "work.txt".to_owned()).await?;
    let ready = wait_for(&mut events, &task.id, TaskState::Ready).await?;
    let worktree = ready.worktree.ok_or("no worktree")?;
    assert_eq!(
        std::fs::read_to_string(worktree.path.join("work.txt"))?,
        "work.txt\n"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_fails_the_task_and_keeps_its_branch() -> TestResult {
    let fx = fixture()?;
    let engine = Engine::start(fx.config()?).await?;
    let mut events = engine.subscribe();

    let task = engine.create(fx.spec("hang forever")).await?;
    // La primera línea del agente dice que ya está trabajando
    loop {
        if let Event::Output { id, line, .. } = events.recv().await?
            && id == task.id
            && line == "working"
        {
            break;
        }
    }
    engine.cancel(&task.id).await?;
    let failed = wait_for(&mut events, &task.id, TaskState::Failed).await?;
    assert_eq!(failed.failure.as_deref(), Some("the agent was cancelled"));
    assert!(failed.worktree.is_some_and(|wt| wt.path.exists()));
    git(&fx.checkout, &["rev-parse", "--verify", &task.id.branch()])?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_a_running_task_removes_its_worktree_and_branch() -> TestResult {
    let fx = fixture()?;
    let engine = Engine::start(fx.config()?).await?;
    let mut events = engine.subscribe();

    let task = engine.create(fx.spec("hang forever")).await?;
    let working = wait_for(&mut events, &task.id, TaskState::Agent).await?;
    let worktree = working.worktree.ok_or("no worktree")?;
    let dropped = engine.drop_task(&task.id).await?;
    assert_eq!(dropped.state, TaskState::Dropped);
    assert!(!worktree.path.exists());
    assert!(git(&fx.checkout, &["rev-parse", "--verify", &task.id.branch()]).is_err());
    assert!(matches!(
        engine.cancel(&task.id).await,
        Err(EngineError::NotRunning(_))
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn tasks_survive_a_restart() -> TestResult {
    let fx = fixture()?;
    let id = {
        let engine = Engine::start(fx.config()?).await?;
        let mut events = engine.subscribe();
        let task = engine.create(fx.spec("write the work file")).await?;
        wait_for(&mut events, &task.id, TaskState::Ready).await?;
        task.id
    };
    let engine = Engine::start(fx.config()?).await?;
    let task = engine.get(&id).await?;
    assert_eq!(task.state, TaskState::Ready);
    assert_eq!(engine.list().await?.len(), 1);
    assert!(matches!(
        engine.get(&TaskId::parse("nope")?).await,
        Err(EngineError::NotFound(_))
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_specs_are_rejected_before_anything_runs() -> TestResult {
    let fx = fixture()?;
    let engine = Engine::start(fx.config()?).await?;
    let cloud = TaskSpec {
        client: None,
        agent: AgentProvider::Ollama {
            model: "big:cloud".to_owned(),
        },
        ..fx.spec("x")
    };
    assert!(matches!(
        engine.create(cloud).await,
        Err(EngineError::Invalid(_))
    ));
    assert!(matches!(
        engine.create(fx.spec("  ")).await,
        Err(EngineError::Invalid(_))
    ));
    assert!(engine.list().await?.is_empty());
    Ok(())
}

/// Espera el siguiente evento de choques que cumpla `wanted`.
async fn wait_for_overlaps(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    wanted: impl Fn(usize) -> bool,
) -> TestResult {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .map_err(|_| "the expected overlaps never came")??;
        if let Event::Overlaps { overlaps } = event
            && wanted(overlaps.len())
        {
            return Ok(());
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn overlaps_are_announced_again_when_they_end() -> TestResult {
    let fx = fixture()?;
    let engine = Engine::start(fx.config()?).await?;
    let mut events = engine.subscribe();

    // Las dos escriben `work.txt`
    let first = engine.create(fx.spec("first")).await?;
    let second = engine.create(fx.spec("second")).await?;
    wait_for_overlaps(&mut events, |count| count == 1).await?;
    wait_for(&mut events, &second.id, TaskState::Ready).await?;
    assert_eq!(engine.overlaps().await?.len(), 1);

    // Sin la segunda ya no hay choque, y los clientes se enteran con una lista vacía
    engine.drop_task(&second.id).await?;
    wait_for_overlaps(&mut events, |count| count == 0).await?;
    assert!(engine.overlaps().await?.is_empty());
    assert!(engine.get(&first.id).await.is_ok());
    Ok(())
}

fn external_spec(fx: &common::Fixture) -> TaskSpec {
    TaskSpec {
        agent: AgentProvider::External,
        client: Some("palo:e-1".to_owned()),
        ..fx.spec("palo runs its own agent here")
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_external_agent_works_in_the_worktree_and_reports_done() -> TestResult {
    let fx = fixture()?;
    let engine = Engine::start(fx.config()?).await?;
    let mut events = engine.subscribe();

    let task = engine.create(external_spec(&fx)).await?;
    let working = wait_for(&mut events, &task.id, TaskState::Agent).await?;
    assert_eq!(working.client.as_deref(), Some("palo:e-1"));
    let worktree = working.worktree.ok_or("no worktree")?;
    // fleet no lanza nada: la tarea se queda esperando al cliente
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(engine.get(&task.id).await?.state, TaskState::Agent);
    assert!(!worktree.path.join("work.txt").exists());

    // Primera vuelta en rojo: el pre-flight falla y la tarea se puede reintentar
    std::fs::write(worktree.path.join(".fleet-needs-work"), "")?;
    engine.done(&task.id).await?;
    let red = wait_for(&mut events, &task.id, TaskState::Failed).await?;
    assert!(red.preflight.as_ref().is_some_and(|p| !p.passed()));

    std::fs::remove_file(worktree.path.join(".fleet-needs-work"))?;
    std::fs::write(worktree.path.join("work.txt"), "by palo\n")?;
    git(&worktree.path, &["add", "work.txt"])?;
    git(&worktree.path, &["commit", "--quiet", "-m", "work"])?;
    engine.done(&task.id).await?;
    let ready = wait_for(&mut events, &task.id, TaskState::Ready).await?;
    assert_eq!(ready.diff.as_ref().map(|diff| diff.commits), Some(1));
    assert!(matches!(
        engine.done(&task.id).await,
        Err(EngineError::Transition(_))
    ));

    engine.merge(&task.id).await?;
    wait_for(&mut events, &task.id, TaskState::Merged).await?;
    assert!(fx.origin_has("work.txt")?);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn external_tasks_survive_a_restart_and_can_be_cancelled() -> TestResult {
    let fx = fixture()?;
    let config = fx.config()?;
    let id = {
        let engine = Engine::start(config.clone()).await?;
        let mut events = engine.subscribe();
        let task = engine.create(external_spec(&fx)).await?;
        wait_for(&mut events, &task.id, TaskState::Agent).await?;
        task.id
    };
    let engine = Engine::start(config).await?;
    let task = engine.get(&id).await?;
    assert_eq!(task.state, TaskState::Agent, "{:?}", task.failure);

    engine.cancel(&id).await?;
    let cancelled = engine.get(&id).await?;
    assert_eq!(cancelled.state, TaskState::Failed);
    let worktree = cancelled.worktree.ok_or("no worktree")?;
    assert!(worktree.path.is_dir());
    assert!(!git(&fx.checkout, &["branch", "--list", &cancelled.branch])?.is_empty());

    // Un agente propio de fleet no avisa con `done`
    let own = engine.create(fx.spec("hang forever")).await?;
    assert!(matches!(
        engine.done(&own.id).await,
        Err(EngineError::Invalid(_))
    ));
    engine.drop_task(&own.id).await?;
    Ok(())
}
