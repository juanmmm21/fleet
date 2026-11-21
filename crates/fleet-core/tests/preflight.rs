//! Pre-flight dentro de worktrees de verdad.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use fleet_core::TaskId;
use fleet_core::preflight::{self, ActPlan, PreflightConfig, PreflightError, PreflightPlan};
use fleet_core::worktree::{self, Worktree, WorktreeConfig};
use tempfile::TempDir;

mod common;
use common::{commit_file, git};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Repo con los ficheros dados ya en `origin/main` y el worktree de una tarea sobre él.
async fn worktree_with(
    files: &[(&str, &str)],
) -> Result<(TempDir, Worktree), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    git(dir.path(), &["init", "--quiet", "--bare", "origin.git"])?;
    git(dir.path(), &["clone", "--quiet", "origin.git", "app"])?;
    let checkout = dir.path().join("app");
    commit_file(&checkout, "README.md", "app\n")?;
    for (name, content) in files {
        commit_file(&checkout, name, content)?;
    }
    git(&checkout, &["push", "--quiet", "origin", "HEAD:main"])?;
    let config = WorktreeConfig::new(dir.path().join("worktrees"));
    let id = TaskId::generate(SystemTime::now());
    let wt = worktree::create(&config, &checkout, &id, "main").await?;
    Ok((dir, wt))
}

fn config() -> PreflightConfig {
    PreflightConfig {
        default_timeout: Duration::from_secs(30),
        grace: Duration::from_secs(1),
        tail_bytes: 4096,
        ..PreflightConfig::default()
    }
}

fn log(dir: &TempDir) -> PathBuf {
    dir.path().join("logs").join("preflight.log")
}

const SCRIPT_CONFIG: &str = "[preflight]\ncommand = [\"sh\", \"check.sh\"]\n";

#[tokio::test]
async fn green_tests_pass_and_see_the_worktree_and_ci() -> TestResult {
    let (dir, wt) = worktree_with(&[
        ("fleet.toml", SCRIPT_CONFIG),
        (
            "check.sh",
            "test -f README.md && echo \"ci=$CI\" && echo all good",
        ),
    ])
    .await?;
    let plan = preflight::plan(&wt.path, &config())?;
    let result = preflight::run(
        &wt.path,
        &plan,
        &config(),
        &log(&dir),
        std::future::pending(),
    )
    .await?;
    assert!(result.passed(), "{result:?}");
    assert_eq!(result.command, ["sh", "check.sh"]);
    assert!(result.output_tail.contains("ci=true\nall good\n"));
    let log = std::fs::read_to_string(log(&dir))?;
    assert!(log.contains("== preflight `sh check.sh` (fleet.toml) =="));
    assert!(log.trim_end().ends_with("== `sh check.sh` passed =="));
    Ok(())
}

#[tokio::test]
async fn red_tests_fail_with_their_output() -> TestResult {
    let (dir, wt) = worktree_with(&[
        ("fleet.toml", SCRIPT_CONFIG),
        ("check.sh", "echo 'assertion failed: 1 == 2' >&2; exit 101"),
    ])
    .await?;
    let plan = preflight::plan(&wt.path, &config())?;
    let result = preflight::run(
        &wt.path,
        &plan,
        &config(),
        &log(&dir),
        std::future::pending(),
    )
    .await?;
    assert!(!result.passed());
    assert_eq!(result.exit_code, Some(101));
    assert_eq!(result.summary(), "`sh check.sh` exited with code 101");
    assert!(result.output_tail.contains("[stderr] assertion failed"));
    Ok(())
}

#[tokio::test]
async fn hanging_tests_are_cut_at_the_deadline() -> TestResult {
    let (dir, wt) = worktree_with(&[
        ("fleet.toml", SCRIPT_CONFIG),
        ("check.sh", "echo started; sleep 30"),
    ])
    .await?;
    let plan = PreflightPlan {
        timeout: Duration::from_millis(400),
        ..preflight::plan(&wt.path, &config())?
    };
    let started = Instant::now();
    let result = preflight::run(
        &wt.path,
        &plan,
        &config(),
        &log(&dir),
        std::future::pending(),
    )
    .await?;
    assert!(result.timed_out);
    assert!(!result.passed());
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(result.output_tail.contains("started"));
    Ok(())
}

#[tokio::test]
async fn a_relative_program_runs_from_the_worktree() -> TestResult {
    let (dir, wt) = worktree_with(&[
        ("fleet.toml", "[preflight]\ncommand = [\"scripts/check\"]\n"),
        ("scripts/check", "#!/bin/sh\npwd\n"),
    ])
    .await?;
    git(&wt.path, &["update-index", "--chmod=+x", "scripts/check"])?;
    std::fs::set_permissions(
        wt.path.join("scripts/check"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )?;
    let plan = preflight::plan(&wt.path, &config())?;
    // Lanzado desde otra carpeta: el programa se resuelve igualmente dentro del worktree
    let result = preflight::run(
        &wt.path,
        &plan,
        &config(),
        &log(&dir),
        std::future::pending(),
    )
    .await?;
    assert!(result.passed(), "{result:?}");
    let pwd = wt.path.canonicalize()?;
    assert!(result.output_tail.contains(&pwd.display().to_string()));
    Ok(())
}

#[tokio::test]
async fn cancelling_the_preflight_is_not_a_result() -> TestResult {
    let (dir, wt) =
        worktree_with(&[("fleet.toml", SCRIPT_CONFIG), ("check.sh", "sleep 30")]).await?;
    let plan = preflight::plan(&wt.path, &config())?;
    let cancel = tokio::time::sleep(Duration::from_millis(200));
    let result = preflight::run(&wt.path, &plan, &config(), &log(&dir), cancel).await;
    assert!(matches!(result, Err(PreflightError::Cancelled)));
    Ok(())
}

#[tokio::test]
async fn a_repo_without_tests_has_no_preflight() -> TestResult {
    let (_dir, wt) = worktree_with(&[]).await?;
    assert!(matches!(
        preflight::plan(&wt.path, &config()),
        Err(PreflightError::NoCommand(_))
    ));
    Ok(())
}

/// Un `act` de mentira: comprueba que está en un repo con `.git` de verdad y en qué rama.
fn fake_act(dir: &std::path::Path, exit_code: i32) -> Result<PathBuf, Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("act");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\ntest -d .git || {{ echo '.git is not a folder' >&2; exit 3; }}\n\
             echo \"args=$*\"\necho \"branch=$(git rev-parse --abbrev-ref HEAD)\"\n\
             ls\nexit {exit_code}\n"
        ),
    )?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(path)
}

#[tokio::test]
async fn act_runs_on_a_clean_clone_of_the_branch_and_cleans_up() -> TestResult {
    let (dir, wt) = worktree_with(&[]).await?;
    commit_file(&wt.path, "committed.txt", "in the branch\n")?;
    std::fs::write(wt.path.join("pending.txt"), "not committed\n")?;
    let config = PreflightConfig {
        act_program: fake_act(dir.path(), 0)?,
        ..config()
    };
    let act = ActPlan {
        job: "test".to_owned(),
        args: vec![
            "--container-architecture".to_owned(),
            "linux/amd64".to_owned(),
        ],
    };
    let result = preflight::run_act(
        &wt,
        &act,
        Duration::from_secs(30),
        &config,
        &log(&dir),
        std::future::pending(),
    )
    .await?;
    assert!(result.passed(), "{result:?}");
    assert_eq!(
        result.command[1..],
        ["-j", "test", "--container-architecture", "linux/amd64"]
    );
    let tail = &result.output_tail;
    assert!(tail.contains(&format!("branch={}", wt.branch)), "{tail}");
    assert!(tail.contains("committed.txt"), "{tail}");
    assert!(!tail.contains("pending.txt"), "{tail}");

    let clone = wt.path.with_file_name(format!(
        "{}.act",
        wt.path.file_name().ok_or("no name")?.to_string_lossy()
    ));
    assert!(!clone.exists());
    assert!(wt.path.join("pending.txt").exists());
    Ok(())
}

#[tokio::test]
async fn a_failing_act_job_is_a_red_preflight() -> TestResult {
    let (dir, wt) = worktree_with(&[]).await?;
    let config = PreflightConfig {
        act_program: fake_act(dir.path(), 1)?,
        ..config()
    };
    let act = ActPlan {
        job: "lint".to_owned(),
        args: Vec::new(),
    };
    let result = preflight::run_act(
        &wt,
        &act,
        Duration::from_secs(30),
        &config,
        &log(&dir),
        std::future::pending(),
    )
    .await?;
    assert!(!result.passed());
    assert_eq!(result.exit_code, Some(1));
    Ok(())
}
