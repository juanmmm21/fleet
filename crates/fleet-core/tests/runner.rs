//! El agente en su worktree, con un script de shell haciendo de agente y repos de verdad.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use fleet_core::runner::{self, RunConfig};
use fleet_core::worktree::{self, ChangeKind, Worktree, WorktreeConfig};
use fleet_core::{
    AgentOutcome, AgentPrograms, AgentProvider, Invocation, OutputLine, Stream, StreamEnd, TaskId,
    Transcript,
};
use tempfile::TempDir;
use tokio::process::Command;
use tokio::sync::mpsc;

mod common;
use common::{commit_file, git};

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Fixture {
    dir: TempDir,
    config: WorktreeConfig,
    worktree: Worktree,
}

impl Fixture {
    fn log(&self) -> PathBuf {
        self.dir.path().join("logs").join("task.log")
    }
}

async fn fixture() -> Result<Fixture, Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    git(dir.path(), &["init", "--quiet", "--bare", "origin.git"])?;
    git(dir.path(), &["clone", "--quiet", "origin.git", "app"])?;
    let checkout = dir.path().join("app");
    commit_file(&checkout, "README.md", "app\n")?;
    git(&checkout, &["push", "--quiet", "origin", "HEAD:main"])?;
    let config = WorktreeConfig::new(dir.path().join("worktrees"));
    let id = TaskId::generate(SystemTime::now());
    let worktree = worktree::create(&config, &checkout, &id, "main").await?;
    Ok(Fixture {
        dir,
        config,
        worktree,
    })
}

/// El «agente» commitea con una identidad de prueba, sin tocar la configuración de nadie.
fn test_identity(command: &mut Command) {
    command
        .env("GIT_AUTHOR_NAME", "fleet-test")
        .env("GIT_AUTHOR_EMAIL", "fleet-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "fleet-test")
        .env("GIT_COMMITTER_EMAIL", "fleet-test@example.invalid");
}

fn agent(script: &str) -> Invocation {
    let mut command = Command::new("sh");
    command.arg("-c").arg(script);
    test_identity(&mut command);
    Invocation {
        command,
        stdin: Vec::new(),
        transcript: Transcript::for_provider(&AgentProvider::Claude { model: None }),
    }
}

/// Un ejecutable de mentira que imita a una CLI de agente.
fn fake_cli(dir: &Path, name: &str, body: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n"))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(path)
}

fn quick() -> RunConfig {
    RunConfig {
        timeout: Duration::from_secs(30),
        grace: Duration::from_secs(2),
        tail_bytes: 1024,
    }
}

fn read(path: &Path) -> std::io::Result<String> {
    std::fs::read_to_string(path)
}

#[tokio::test]
async fn runs_in_the_worktree_logs_everything_and_reports_the_diff() -> TestResult {
    let fx = fixture().await?;
    let script = "pwd; echo 'fn main() {}' > main.rs; git add main.rs; \
                  git -c commit.gpgsign=false commit --quiet -m 'add main'; \
                  echo notes > notes.txt; echo careful >&2; exit 0";
    let (tx, mut rx) = mpsc::channel(16);
    let report = runner::run_agent(
        &fx.config,
        &fx.worktree,
        agent(script),
        &quick(),
        &fx.log(),
        std::future::pending(),
        Some(tx),
    )
    .await?;

    assert_eq!(report.end, StreamEnd::Exited(Some(0)));
    assert_eq!(report.diff.commits, 1);
    assert!(report.diff.uncommitted);
    let files: Vec<(&str, ChangeKind)> = report
        .diff
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.kind))
        .collect();
    assert_eq!(
        files,
        [
            ("main.rs", ChangeKind::Added),
            ("notes.txt", ChangeKind::Added)
        ]
    );

    let log = read(&fx.log())?;
    let pwd = fx.worktree.path.canonicalize()?;
    assert!(log.contains(&format!("{}\n", pwd.display())), "{log}");
    assert!(log.contains("[stderr] careful\n"), "{log}");
    assert!(
        log.lines()
            .next()
            .is_some_and(|l| l.ends_with("sh started =="))
    );
    assert!(
        log.trim_end().ends_with("exited with code 0 after 0s =="),
        "{log}"
    );
    assert!(report.output_tail.contains("[stderr] careful"));

    let mut live = Vec::new();
    while let Ok(line) = rx.try_recv() {
        live.push(line);
    }
    assert!(live.contains(&OutputLine {
        stream: Stream::Stderr,
        line: "careful".to_owned()
    }));
    Ok(())
}

#[tokio::test]
async fn a_cancelled_agent_still_reports_what_it_left() -> TestResult {
    let fx = fixture().await?;
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
    let (tx, mut rx) = mpsc::channel(16);
    let config = fx.config.clone();
    let worktree = fx.worktree.clone();
    let log = fx.log();
    let run = tokio::spawn(async move {
        runner::run_agent(
            &config,
            &worktree,
            agent("echo half > half.txt; echo working; sleep 30 & wait"),
            &quick(),
            &log,
            async move {
                // Soltar el emisor sin avisar también cancela
                let _ = cancel_rx.await;
            },
            Some(tx),
        )
        .await
    });
    let first = rx.recv().await.ok_or("no output")?;
    assert_eq!(first.line, "working");
    cancel_tx.send(()).map_err(|()| "agent already finished")?;
    let report = run.await??;

    assert_eq!(report.end, StreamEnd::Cancelled);
    assert!(report.duration_ms < 10_000);
    let paths: Vec<&str> = report.diff.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["half.txt"]);
    assert!(read(&fx.log())?.contains("was cancelled"));
    Ok(())
}

#[tokio::test]
async fn each_run_appends_to_the_same_log() -> TestResult {
    let fx = fixture().await?;
    let config = RunConfig {
        timeout: Duration::from_millis(300),
        ..quick()
    };
    for script in ["echo first", "echo second; sleep 30"] {
        runner::run_agent(
            &fx.config,
            &fx.worktree,
            agent(script),
            &config,
            &fx.log(),
            std::future::pending(),
            None,
        )
        .await?;
    }
    let log = read(&fx.log())?;
    assert!(log.contains("first\n") && log.contains("second\n"), "{log}");
    assert!(log.contains("timed out"), "{log}");
    assert_eq!(log.matches("started ==").count(), 2);
    Ok(())
}

#[tokio::test]
async fn a_missing_agent_program_is_an_error_not_a_hang() -> TestResult {
    let fx = fixture().await?;
    let result = runner::run_agent(
        &fx.config,
        &fx.worktree,
        Invocation {
            command: Command::new("fleet-no-such-agent"),
            stdin: Vec::new(),
            transcript: Transcript::for_provider(&AgentProvider::Agy { model: None }),
        },
        &quick(),
        &fx.log(),
        std::future::pending(),
        None,
    )
    .await;
    assert!(matches!(result, Err(runner::RunError::Process(_))));
    Ok(())
}

#[tokio::test]
async fn a_fake_claude_gets_the_prompt_on_stdin_and_reports_its_result() -> TestResult {
    let fx = fixture().await?;
    let bin = fx.dir.path().join("bin");
    std::fs::create_dir_all(&bin)?;
    // Comprueba el modo sin permisos, lee el encargo, commitea y responde como Claude Code
    let claude = fake_cli(
        &bin,
        "claude",
        r#"case "$*" in *"--permission-mode bypassPermissions"*) ;; *) exit 9 ;; esac
prompt=$(cat)
echo '{"type":"system","subtype":"init","session_id":"s-42","apiKeySource":"none"}'
echo "$prompt" > prompt.txt
git add prompt.txt && git -c commit.gpgsign=false commit --quiet -m prompt
echo '{"type":"result","subtype":"success","is_error":false,"result":"wrote it","num_turns":3,"session_id":"s-42"}'"#,
    )?;
    let programs = AgentPrograms {
        claude,
        ..AgentPrograms::default()
    };
    let mut invocation = AgentProvider::Claude { model: None }.invocation(
        &programs,
        "write the prompt down",
        None,
    )?;
    test_identity(&mut invocation.command);

    let report = runner::run_agent(
        &fx.config,
        &fx.worktree,
        invocation,
        &quick(),
        &fx.log(),
        std::future::pending(),
        None,
    )
    .await?;
    assert_eq!(report.end, StreamEnd::Exited(Some(0)));
    let prompt = read(&fx.worktree.path.join("prompt.txt"))?;
    assert!(prompt.starts_with("You are running unattended"), "{prompt}");
    assert!(
        prompt.ends_with("Task:\nwrite the prompt down\n"),
        "{prompt}"
    );
    assert_eq!(report.diff.commits, 1);
    assert_eq!(report.summary.session_id.as_deref(), Some("s-42"));
    assert_eq!(report.summary.turns, Some(3));
    assert_eq!(
        report.summary.outcome,
        Some(AgentOutcome::Success {
            text: "wrote it".to_owned()
        })
    );
    Ok(())
}

#[tokio::test]
async fn a_claude_on_a_paid_key_is_stopped_at_once() -> TestResult {
    let fx = fixture().await?;
    let bin = fx.dir.path().join("bin");
    std::fs::create_dir_all(&bin)?;
    let claude = fake_cli(
        &bin,
        "claude",
        r#"cat > /dev/null
echo '{"type":"system","subtype":"init","apiKeySource":"ANTHROPIC_API_KEY"}'
sleep 30 & wait"#,
    )?;
    let programs = AgentPrograms {
        claude,
        ..AgentPrograms::default()
    };
    let invocation =
        AgentProvider::Claude { model: None }.invocation(&programs, "anything", None)?;
    let report = runner::run_agent(
        &fx.config,
        &fx.worktree,
        invocation,
        &quick(),
        &fx.log(),
        std::future::pending(),
        None,
    )
    .await?;
    assert_eq!(report.end, StreamEnd::Cancelled);
    assert!(report.duration_ms < 10_000);
    assert!(
        report
            .summary
            .refused
            .as_deref()
            .is_some_and(|r| r.contains("billed per use"))
    );
    assert!(read(&fx.log())?.contains("fleet stopped the agent"));
    Ok(())
}

#[tokio::test]
async fn a_fake_agy_reports_quota_exhaustion() -> TestResult {
    let fx = fixture().await?;
    let bin = fx.dir.path().join("bin");
    std::fs::create_dir_all(&bin)?;
    let agy = fake_cli(
        &bin,
        "agy",
        r#"cat > /dev/null
echo '{"event":"result","conversation_id":"c-1","result":{"status":"ERROR","error":"RESOURCE_EXHAUSTED"}}'
exit 1"#,
    )?;
    let programs = AgentPrograms {
        agy,
        ..AgentPrograms::default()
    };
    let invocation = AgentProvider::Agy { model: None }.invocation(&programs, "anything", None)?;
    let report = runner::run_agent(
        &fx.config,
        &fx.worktree,
        invocation,
        &quick(),
        &fx.log(),
        std::future::pending(),
        None,
    )
    .await?;
    assert_eq!(report.end, StreamEnd::Exited(Some(1)));
    assert!(report.summary.quota_exhausted);
    assert!(report.diff.is_empty());
    Ok(())
}
