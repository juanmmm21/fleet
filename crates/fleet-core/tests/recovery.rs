//! Reinicio del demonio: estado en SQLite y worktrees de verdad en carpetas temporales.

use std::path::PathBuf;

use fleet_core::recovery::{self, Recovery};
use fleet_core::worktree::{self, WorktreeConfig};
use fleet_core::{AgentProvider, Store, Task, TaskId, TaskSpec, TaskState};

mod common;
use common::{commit_file, git};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn task(repo: &std::path::Path, id: &str) -> Result<Task, Box<dyn std::error::Error>> {
    let spec = TaskSpec {
        client: None,
        repo: repo.to_path_buf(),
        prompt: format!("task {id}"),
        agent: AgentProvider::Claude { model: None },
        base: "main".to_owned(),
    };
    Ok(Task::new(TaskId::parse(id)?, spec, 1)?)
}

/// Guarda cada paso, como hará el demonio.
async fn advance(store: &Store, task: &mut Task, states: &[TaskState]) -> TestResult {
    for (step, state) in states.iter().enumerate() {
        task.transition(*state, 2 + u64::try_from(step)?)?;
        store.update(task).await?;
    }
    Ok(())
}

#[tokio::test]
async fn restart_fails_interrupted_work_and_keeps_its_worktree() -> TestResult {
    let dir = tempfile::tempdir()?;
    git(dir.path(), &["init", "--quiet", "--bare", "origin.git"])?;
    git(dir.path(), &["clone", "--quiet", "origin.git", "app"])?;
    let checkout = dir.path().join("app");
    commit_file(&checkout, "README.md", "app\n")?;
    git(&checkout, &["push", "--quiet", "origin", "HEAD:main"])?;
    let config = WorktreeConfig::new(dir.path().join("worktrees"));
    let db = dir.path().join("fleet.sqlite3");

    let mut gone_path = PathBuf::new();
    let mut agent_path = PathBuf::new();
    {
        let store = Store::open(&db).await?;

        let mut agent = task(&checkout, "a-agent")?;
        store.insert(&agent).await?;
        let created = worktree::create(&config, &checkout, &agent.id, "main").await?;
        agent_path.clone_from(&created.path);
        agent.worktree = Some(created);
        advance(&store, &mut agent, &[TaskState::Agent]).await?;
        commit_file(&agent_path, "work.txt", "half done\n")?;

        let mut waiting = task(&checkout, "b-waiting")?;
        store.insert(&waiting).await?;
        let created = worktree::create(&config, &checkout, &waiting.id, "main").await?;
        waiting.worktree = Some(created);
        advance(&store, &mut waiting, &[TaskState::Agent]).await?;
        waiting.ask("which crate?", 9)?;
        store.update(&waiting).await?;

        let mut gone = task(&checkout, "c-gone")?;
        store.insert(&gone).await?;
        let created = worktree::create(&config, &checkout, &gone.id, "main").await?;
        gone_path.clone_from(&created.path);
        gone.worktree = Some(created);
        advance(&store, &mut gone, &[TaskState::Agent, TaskState::Preflight]).await?;

        let provisioning = task(&checkout, "d-provisioning")?;
        store.insert(&provisioning).await?;
    }
    // Alguien borra a mano una carpeta mientras el demonio está parado
    std::fs::remove_dir_all(&gone_path)?;

    let store = Store::open(&db).await?;
    let recovered = recovery::recover(&store, 100).await?;
    let actions: Vec<(String, Recovery)> = recovered
        .iter()
        .map(|r| (r.task.id.to_string(), r.action.clone()))
        .collect();
    assert_eq!(
        actions,
        [
            (
                "a-agent".to_owned(),
                Recovery::Interrupted {
                    was: TaskState::Agent
                }
            ),
            ("b-waiting".to_owned(), Recovery::Kept),
            (
                "c-gone".to_owned(),
                Recovery::WorktreeMissing {
                    path: gone_path.clone()
                }
            ),
            (
                "d-provisioning".to_owned(),
                Recovery::Interrupted {
                    was: TaskState::Provisioning
                }
            ),
        ]
    );

    // Lo cambiado queda guardado, no solo devuelto
    let agent = store
        .get(&TaskId::parse("a-agent")?)
        .await?
        .ok_or("a-agent is missing")?;
    assert_eq!(agent.state, TaskState::Failed);
    assert_eq!(
        agent.failure.as_deref(),
        Some("interrupted: fleetd restarted during agent")
    );
    let waiting = store
        .get(&TaskId::parse("b-waiting")?)
        .await?
        .ok_or("b-waiting is missing")?;
    assert_eq!(waiting.state, TaskState::Waiting);
    assert_eq!(waiting.question.as_deref(), Some("which crate?"));

    // El trabajo a medias del agente sigue en su rama y su carpeta
    assert!(agent_path.join("work.txt").is_file());
    let log = git(&checkout, &["log", "--format=%s", "fleet/a-agent", "-1"])?;
    assert_eq!(log, "add work.txt");

    // Un segundo arranque no cambia nada más: todo está ya en un estado estable
    let again = recovery::recover(&store, 200).await?;
    assert!(again.iter().all(|r| r.action == Recovery::Kept));
    assert_eq!(store.live().await?.len(), 4);
    Ok(())
}
