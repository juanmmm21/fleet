//! Ayudantes de los tests de `fleetd`: repos de git de verdad y un `claude` de mentira.
#![allow(dead_code)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use fleet_core::{AgentProvider, Task, TaskId, TaskSpec, TaskState};
use fleetd::{EngineConfig, Event};
use tempfile::TempDir;
use tokio::sync::broadcast;

pub type TestResult = Result<(), Box<dyn std::error::Error>>;

pub fn git(cwd: &Path, args: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
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
pub const FAKE_CLAUDE: &str = r#"#!/bin/sh
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

pub struct Fixture {
    pub dir: TempDir,
    pub checkout: PathBuf,
}

impl Fixture {
    pub fn config(&self) -> Result<EngineConfig, Box<dyn std::error::Error>> {
        let mut config = EngineConfig::new(self.dir.path().join("state"));
        let claude = self.dir.path().join("claude");
        std::fs::write(&claude, FAKE_CLAUDE)?;
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755))?;
        config.programs.claude = claude;
        config.run.grace = Duration::from_secs(1);
        config.preflight.grace = Duration::from_secs(1);
        Ok(config)
    }

    pub fn spec(&self, prompt: &str) -> TaskSpec {
        TaskSpec {
            client: None,
            repo: self.checkout.clone(),
            prompt: prompt.to_owned(),
            agent: AgentProvider::Claude { model: None },
            base: "main".to_owned(),
        }
    }

    pub fn origin_has(&self, file: &str) -> Result<bool, Box<dyn std::error::Error>> {
        let files = git(
            &self.dir.path().join("origin.git"),
            &["ls-tree", "--name-only", "main"],
        )?;
        Ok(files.lines().any(|line| line == file))
    }
}

pub fn fixture() -> Result<Fixture, Box<dyn std::error::Error>> {
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
pub async fn wait_for(
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
