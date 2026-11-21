//! La CLI contra un `fleetd` de verdad en un puerto efímero de loopback, con repos de git de
//! verdad y un `claude` de mentira.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command as Process;
use std::time::Duration;

use fleet::args::{Command, RunArgs};
use fleet::client::{Client, ClientError};
use fleet::{CliError, execute};
use fleet_core::{AgentProvider, TaskId, TaskState};
use fleetd::api::{self, AppState};
use fleetd::{Engine, EngineConfig};
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn git(cwd: &Path, args: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    let output = Process::new("git")
        .current_dir(cwd)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Pregunta si el encargo dice `ask-first` y aún no se ha retomado; si no, escribe y commitea.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
input=$(cat)
echo '{"type":"system","subtype":"init","session_id":"s-1","apiKeySource":"none"}'
case "$input" in
  *ask-first*)
    case "$*" in
      *"--resume s-1"*) ;;
      *) echo '{"type":"result","subtype":"success","result":"QUESTION: which file?","session_id":"s-1"}'; exit 0 ;;
    esac ;;
esac
echo "agent says hi"
echo "$input" > work.txt
git add work.txt
git -c user.name=t -c user.email=t@example.invalid -c commit.gpgsign=false commit --quiet -m work
echo '{"type":"result","subtype":"success","result":"done","session_id":"s-1"}'
"#;

struct Daemon {
    dir: TempDir,
    url: String,
    runtime: tokio::runtime::Runtime,
}

impl Daemon {
    fn checkout(&self) -> std::path::PathBuf {
        self.dir.path().join("app")
    }
}

fn daemon() -> Result<Daemon, Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    git(dir.path(), &["init", "--quiet", "--bare", "origin.git"])?;
    git(dir.path(), &["clone", "--quiet", "origin.git", "app"])?;
    let checkout = dir.path().join("app");
    git(&checkout, &["config", "user.name", "t"])?;
    git(&checkout, &["config", "user.email", "t@example.invalid"])?;
    std::fs::write(
        checkout.join("fleet.toml"),
        "[preflight]\ncommand = [\"true\"]\n",
    )?;
    git(&checkout, &["add", "."])?;
    git(&checkout, &["commit", "--quiet", "-m", "init"])?;
    git(&checkout, &["push", "--quiet", "origin", "HEAD:main"])?;

    let claude = dir.path().join("claude");
    std::fs::write(&claude, FAKE_CLAUDE)?;
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755))?;
    let mut config = EngineConfig::new(dir.path().join("state"));
    config.programs.claude = claude;
    let repos_root = dir.path().to_path_buf();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let url = runtime.block_on(async move {
        let engine = Engine::start(config).await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let (stopping, shutdown) = tokio::sync::watch::channel(false);
        let router = api::router(AppState {
            engine,
            node: "test".to_owned(),
            shutdown,
            hosts: fleetd::guard::HostPolicy::default(),
            nodes: fleetd::nodes::Nodes::local_only("test")?,
            repos_root: repos_root.clone(),
        });
        tokio::spawn(async move {
            // El emisor vive lo que el servidor: soltarlo no cierra los flujos de eventos
            let _stopping = stopping;
            if let Err(err) = axum::serve(listener, router).await {
                eprintln!("test fleetd stopped: {err}");
            }
        });
        Ok::<_, Box<dyn std::error::Error>>(format!("http://{addr}"))
    })?;
    Ok(Daemon { dir, url, runtime })
}

fn run(client: &Client, command: Command) -> Result<String, CliError> {
    let mut out = Vec::new();
    execute(command, client, &mut out)?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn wait_state(client: &Client, id: &TaskId, state: TaskState) -> TestResult {
    for _ in 0..300 {
        if client.get(id)?.state == state {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(format!("task {id} never reached {state}").into())
}

#[test]
fn a_missing_daemon_says_how_to_start_it() {
    let client = Client::new("http://127.0.0.1:1");
    let err = client
        .list()
        .err()
        .map(|err| err.to_string())
        .unwrap_or_default();
    assert!(
        err.contains("cannot reach fleetd at http://127.0.0.1:1"),
        "{err}"
    );
    assert!(err.contains("start it with `fleetd`"), "{err}");
}

#[test]
fn run_follow_answer_and_merge_from_the_cli() -> TestResult {
    let daemon = daemon()?;
    let client = Client::new(&daemon.url);

    let created = run(
        &client,
        Command::Run(RunArgs {
            prompt: "ask-first, then write".to_owned(),
            repo: daemon.checkout(),
            agent: AgentProvider::Claude { model: None },
            base: None,
            json: true,
        }),
    )?;
    let task: serde_json::Value = serde_json::from_str(&created)?;
    let id = TaskId::parse(task["id"].as_str().ok_or("no id")?)?;

    // Seguir hasta que se queda quieta: pregunta
    let followed = run(
        &client,
        Command::Log {
            id: id.clone(),
            follow: true,
        },
    )?;
    assert!(
        followed.contains(&format!("== task {id} is waiting: which file?")),
        "{followed}"
    );

    let answered = run(
        &client,
        Command::Answer {
            id: id.clone(),
            answer: "work.txt".to_owned(),
        },
    )?;
    assert!(answered.contains("back with the agent"));
    let followed = run(
        &client,
        Command::Log {
            id: id.clone(),
            follow: true,
        },
    )?;
    assert!(
        followed.contains(&format!("is ready (integrate with `fleet merge {id}`)")),
        "{followed}"
    );
    let log = run(
        &client,
        Command::Log {
            id: id.clone(),
            follow: false,
        },
    )?;
    assert!(log.contains("agent says hi"), "{log}");

    let listed = run(&client, Command::List { json: false })?;
    assert!(
        listed.starts_with(id.as_str()) && listed.contains("ready"),
        "{listed}"
    );
    let shown = run(
        &client,
        Command::Show {
            id: id.clone(),
            json: false,
        },
    )?;
    assert!(
        shown.contains("changes   1 commit(s)") && shown.contains("work.txt"),
        "{shown}"
    );
    let diff = run(&client, Command::Diff { id: id.clone() })?;
    assert!(diff.contains("+++ b/work.txt"), "{diff}");

    let merging = run(&client, Command::Merge { id: id.clone() })?;
    assert!(merging.starts_with(&format!("integrating {id}")));
    wait_state(&client, &id, TaskState::Merged)?;
    let files = git(
        &daemon.dir.path().join("origin.git"),
        &["ls-tree", "--name-only", "main"],
    )?;
    assert!(files.lines().any(|f| f == "work.txt"), "{files}");
    drop(daemon.runtime);
    Ok(())
}

#[test]
fn api_errors_reach_the_user_with_their_code() -> TestResult {
    let daemon = daemon()?;
    let client = Client::new(&daemon.url);
    let missing = TaskId::parse("nope")?;
    match run(
        &client,
        Command::Show {
            id: missing.clone(),
            json: false,
        },
    ) {
        Err(CliError::Client(ClientError::Api { status, code, .. })) => {
            assert_eq!((status, code.as_str()), (404, "not_found"));
        }
        other => return Err(format!("expected not_found, got {other:?}").into()),
    }
    let err = run(&client, Command::Cancel { id: missing })
        .err()
        .map(|e| e.to_string());
    assert!(err.is_some_and(|e| e.ends_with("(not_running)")));
    let err = run(
        &client,
        Command::Run(RunArgs {
            prompt: "x".to_owned(),
            repo: daemon.dir.path().join("does-not-exist"),
            agent: AgentProvider::Claude { model: None },
            base: None,
            json: false,
        }),
    );
    assert!(matches!(err, Err(CliError::Repo { .. })));
    Ok(())
}
