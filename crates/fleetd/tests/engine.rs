//! El motor de punta a punta: repos de git de verdad, un `claude` de mentira y SQLite.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use fleet_core::{AgentProvider, Task, TaskId, TaskSpec, TaskState};
use fleetd::{Engine, EngineConfig, EngineError, Event};
use tempfile::TempDir;
use tokio::sync::broadcast;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn git(cwd: &Path, args: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args([
            "-c",
            "user.name=fleet-test",
            "-c",
            "user.email=fleet-test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// El «agente»: según el encargo, pregunta, se queda colgado o escribe y commitea `work.txt`.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
input=$(cat)
echo '{"type":"system","subtype":"init","session_id":"s-1","apiKeySource":"none"}'
case "$input" in
  *ask-first*)
    case "$*" in
      *"--resume s-1"*) ;;
      *) echo '{"type":"result","subtype":"success","result":"QUESTION: which file?","session_id":"s-1"}'; exit 0 ;;
    esac ;;
  *hang*) echo working; sleep 30 & wait; exit 0 ;;
esac
echo "$input" > work.txt
git add work.txt
git -c user.name=fleet-test -c user.email=fleet-test@example.invalid -c commit.gpgsign=false commit --quiet -m work
echo '{"type":"result","subtype":"success","result":"done","num_turns":1,"session_id":"s-1"}'
"#;

struct Fixture {
    dir: TempDir,
    checkout: PathBuf,
}

impl Fixture {
    fn config(&self) -> Result<EngineConfig, Box<dyn std::error::Error>> {
        let mut config = EngineConfig::new(self.dir.path().join("state"));
        let claude = self.dir.path().join("claude");
        std::fs::write(&claude, FAKE_CLAUDE)?;
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755))?;
        config.programs.claude = claude;
        config.run.grace = Duration::from_secs(1);
        config.preflight.grace = Duration::from_secs(1);
        Ok(config)
    }

    fn spec(&self, prompt: &str) -> TaskSpec {
        TaskSpec {
            repo: self.checkout.clone(),
            prompt: prompt.to_owned(),
            agent: AgentProvider::Claude { model: None },
            base: "main".to_owned(),
        }
    }

    fn origin_has(&self, file: &str) -> Result<bool, Box<dyn std::error::Error>> {
        let files = git(
            &self.dir.path().join("origin.git"),
            &["ls-tree", "--name-only", "main"],
        )?;
        Ok(files.lines().any(|line| line == file))
    }
}

fn fixture() -> Result<Fixture, Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    git(dir.path(), &["init", "--quiet", "--bare", "origin.git"])?;
    git(dir.path(), &["clone", "--quiet", "origin.git", "app"])?;
    let checkout = dir.path().join("app");
    git(&checkout, &["config", "user.name", "fleet-test"])?;
    git(
        &checkout,
        &["config", "user.email", "fleet-test@example.invalid"],
    )?;
    std::fs::write(
        checkout.join("fleet.toml"),
        "[preflight]\ncommand = [\"sh\", \"-c\", \"test -f work.txt || test ! -e .fleet-needs-work\"]\n",
    )?;
    std::fs::write(checkout.join("README.md"), "app\n")?;
    git(&checkout, &["add", "."])?;
    git(&checkout, &["commit", "--quiet", "-m", "init"])?;
    git(&checkout, &["push", "--quiet", "origin", "HEAD:main"])?;
    Ok(Fixture { dir, checkout })
}

/// Espera a que la tarea llegue a `state`, mirando los eventos.
async fn wait_for(
    events: &mut broadcast::Receiver<Event>,
    id: &TaskId,
    state: TaskState,
) -> Result<Task, Box<dyn std::error::Error>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .map_err(|_| format!("task {id} never reached {state}"))??;
        if let Event::Task { task } = event
            && &task.id == id
        {
            if task.state == state {
                return Ok(*task);
            }
            if task.state == TaskState::Failed && state != TaskState::Failed {
                return Err(format!("task {id} failed: {:?}", task.failure).into());
            }
        }
    }
}

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
    engine.cancel(&task.id)?;
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
        engine.cancel(&task.id),
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
