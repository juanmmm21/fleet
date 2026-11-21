//! Estado en SQLite con ficheros de verdad en carpetas temporales.

use std::path::PathBuf;

use fleet_core::{AgentProvider, Store, StoreError, Task, TaskId, TaskSpec, TaskState, Worktree};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn worktree(path: &str) -> Worktree {
    Worktree {
        repo: PathBuf::from("/home/juan/Desarrollo/fleet"),
        path: PathBuf::from(path),
        branch: "fleet/b-working".to_owned(),
        base: "main".to_owned(),
        base_commit: "0123abcd".to_owned(),
        copied: vec![PathBuf::from(".env")],
        linked: vec![PathBuf::from("target")],
    }
}

fn task(id: &str, created_at_ms: u64) -> Result<Task, Box<dyn std::error::Error>> {
    let spec = TaskSpec {
        client: None,
        repo: PathBuf::from("/home/juan/Desarrollo/fleet"),
        prompt: format!("task {id}"),
        agent: AgentProvider::Claude { model: None },
        base: "main".to_owned(),
    };
    Ok(Task::new(TaskId::parse(id)?, spec, created_at_ms)?)
}

#[tokio::test]
async fn live_tasks_survive_reopening_the_file() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("fleet.sqlite3");

    {
        let store = Store::open(&path).await?;
        let mut working = task("b-working", 2)?;
        working.transition(TaskState::Agent, 3)?;
        working.worktree = Some(worktree("/tmp/fleet/fleet-b-working"));
        store.insert(&task("a-provisioning", 1)?).await?;
        store.insert(&working).await?;

        let mut merged = task("c-merged", 4)?;
        store.insert(&merged).await?;
        merged.transition(TaskState::Agent, 5)?;
        store.update(&merged).await?;
        merged.transition(TaskState::Dropped, 6)?;
        store.update(&merged).await?;
    }

    let store = Store::open(&path).await?;
    let live: Vec<String> = store
        .live()
        .await?
        .into_iter()
        .map(|t| t.id.to_string())
        .collect();
    assert_eq!(live, ["a-provisioning", "b-working"]);
    assert_eq!(store.list().await?.len(), 3);

    let working = store.get(&TaskId::parse("b-working")?).await?;
    let working = working.ok_or("b-working is missing")?;
    assert_eq!(working.state, TaskState::Agent);
    assert_eq!(
        working.worktree.map(|wt| wt.path),
        Some(PathBuf::from("/tmp/fleet/fleet-b-working"))
    );
    assert_eq!(store.get(&TaskId::parse("nope")?).await?, None);
    Ok(())
}

#[tokio::test]
async fn every_field_round_trips() -> TestResult {
    let dir = tempfile::tempdir()?;
    let store = Store::open(dir.path().join("fleet.sqlite3")).await?;
    let mut original = task("full", 10)?;
    store.insert(&original).await?;
    original.transition(TaskState::Agent, 11)?;
    store.update(&original).await?;
    original.ask("which crate?", 12)?;
    original.agent = AgentProvider::Ollama {
        model: "qwen3-coder".to_owned(),
    };
    store.update(&original).await?;

    let stored = store.get(&original.id).await?;
    assert_eq!(stored.as_ref(), Some(&original));
    Ok(())
}

#[tokio::test]
async fn duplicates_and_missing_tasks_are_rejected() -> TestResult {
    let dir = tempfile::tempdir()?;
    let store = Store::open(dir.path().join("fleet.sqlite3")).await?;
    let first = task("same", 1)?;
    store.insert(&first).await?;
    assert!(matches!(
        store.insert(&first).await,
        Err(StoreError::Duplicate(_))
    ));
    assert!(matches!(
        store.update(&task("ghost", 1)?).await,
        Err(StoreError::NotFound(_))
    ));
    Ok(())
}

#[tokio::test]
async fn a_stale_copy_cannot_overwrite_newer_state() -> TestResult {
    let dir = tempfile::tempdir()?;
    let store = Store::open(dir.path().join("fleet.sqlite3")).await?;
    let mut fresh = task("raced", 1)?;
    store.insert(&fresh).await?;
    let mut stale = fresh.clone();

    fresh.transition(TaskState::Agent, 2)?;
    store.update(&fresh).await?;
    fresh.transition(TaskState::Preflight, 3)?;
    store.update(&fresh).await?;

    // La copia vieja cree que la tarea sigue aprovisionándose y la manda al agente
    stale.transition(TaskState::Agent, 4)?;
    let err = store.update(&stale).await;
    assert!(matches!(
        err,
        Err(StoreError::StaleUpdate {
            stored: TaskState::Preflight,
            to: TaskState::Agent,
            ..
        })
    ));
    let stored = store.get(&fresh.id).await?.ok_or("raced is missing")?;
    assert_eq!(stored.state, TaskState::Preflight);
    Ok(())
}

#[tokio::test]
async fn skipping_a_saved_step_is_refused() -> TestResult {
    let dir = tempfile::tempdir()?;
    let store = Store::open(dir.path().join("fleet.sqlite3")).await?;
    let mut task = task("hasty", 1)?;
    store.insert(&task).await?;
    task.transition(TaskState::Agent, 2)?;
    task.transition(TaskState::Preflight, 3)?;
    assert!(matches!(
        store.update(&task).await,
        Err(StoreError::StaleUpdate {
            stored: TaskState::Provisioning,
            to: TaskState::Preflight,
            ..
        })
    ));
    Ok(())
}

#[tokio::test]
async fn finished_tasks_are_never_rewritten() -> TestResult {
    let dir = tempfile::tempdir()?;
    let store = Store::open(dir.path().join("fleet.sqlite3")).await?;
    let mut dropped = task("gone", 1)?;
    store.insert(&dropped).await?;
    dropped.transition(TaskState::Dropped, 2)?;
    store.update(&dropped).await?;

    dropped.prompt = "rewritten".to_owned();
    assert!(matches!(
        store.update(&dropped).await,
        Err(StoreError::StaleUpdate {
            stored: TaskState::Dropped,
            ..
        })
    ));
    Ok(())
}

#[tokio::test]
async fn a_newer_schema_is_refused_without_touching_it() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("fleet.sqlite3");
    {
        let conn = rusqlite::Connection::open(&path)?;
        conn.pragma_update(None, "user_version", 99)?;
    }
    let err = Store::open(&path).await;
    assert!(matches!(
        err,
        Err(StoreError::SchemaTooNew { found: 99, .. })
    ));
    let conn = rusqlite::Connection::open(&path)?;
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    assert_eq!(version, 99);
    let tables: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE name = 'tasks'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(tables, 0);
    Ok(())
}

#[tokio::test]
async fn opening_twice_does_not_migrate_twice() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("fleet.sqlite3");
    let first = Store::open(&path).await?;
    first.insert(&task("kept", 1)?).await?;
    drop(first);
    let second = Store::open(&path).await?;
    assert_eq!(second.list().await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn a_folder_that_does_not_exist_is_an_error() -> TestResult {
    let dir = tempfile::tempdir()?;
    let err = Store::open(dir.path().join("missing").join("fleet.sqlite3")).await;
    assert!(matches!(err, Err(StoreError::Sqlite(_))));
    Ok(())
}
