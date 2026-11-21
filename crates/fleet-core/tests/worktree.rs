//! Ciclo de vida del worktree contra repos de git de verdad en carpetas temporales:
//! un `origin` desnudo, el checkout «de Juan» y otro clon para mover `origin` por detrás.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use fleet_core::TaskId;
use fleet_core::worktree::{self, Removal, WorktreeConfig, WorktreeError};
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Fixture {
    dir: TempDir,
    origin: PathBuf,
    checkout: PathBuf,
    config: WorktreeConfig,
}

fn git(cwd: &Path, args: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    let output = Command::new("git")
        .current_dir(cwd)
        // Identidad de prueba solo para estos repos temporales
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

fn commit_file(
    repo: &Path,
    name: &str,
    content: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    std::fs::write(repo.join(name), content)?;
    git(repo, &["add", name])?;
    git(repo, &["commit", "--quiet", "-m", &format!("add {name}")])?;
    git(repo, &["rev-parse", "HEAD"])
}

fn fixture() -> Result<Fixture, Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let origin = dir.path().join("origin.git");
    let checkout = dir.path().join("app");
    git(dir.path(), &["init", "--quiet", "--bare", "origin.git"])?;
    git(dir.path(), &["clone", "--quiet", "origin.git", "app"])?;
    std::fs::write(checkout.join(".gitignore"), "node_modules/\n.venv/\n.env\n")?;
    git(&checkout, &["add", ".gitignore"])?;
    commit_file(&checkout, "README.md", "app\n")?;
    git(&checkout, &["push", "--quiet", "origin", "HEAD:main"])?;
    let config = WorktreeConfig::new(dir.path().join("state").join("worktrees"));
    Ok(Fixture {
        dir,
        origin,
        checkout,
        config,
    })
}

fn new_id() -> TaskId {
    TaskId::generate(SystemTime::now())
}

#[tokio::test]
async fn creates_the_branch_from_fresh_origin_not_from_the_checkout() -> TestResult {
    let fx = fixture()?;
    // Otro clon sube un commit: el checkout de Juan se queda por detrás sin saberlo
    let other = fx.dir.path().join("other");
    git(fx.dir.path(), &["clone", "--quiet", "origin.git", "other"])?;
    let pushed = commit_file(&other, "feature.txt", "new\n")?;
    git(&other, &["push", "--quiet", "origin", "HEAD:main"])?;
    // Y el checkout tiene un commit local sin subir que no debe colarse en la tarea
    commit_file(&fx.checkout, "local.txt", "unpushed\n")?;

    let id = new_id();
    let wt = worktree::create(&fx.config, &fx.checkout, &id, "main").await?;

    assert_eq!(wt.branch, format!("fleet/{}", id.as_str()));
    assert_eq!(wt.base_commit, pushed);
    assert_eq!(wt.path, fx.config.root.join(format!("app-{}", id.as_str())));
    assert_eq!(git(&wt.path, &["rev-parse", "HEAD"])?, pushed);
    assert_eq!(git(&wt.path, &["branch", "--show-current"])?, wt.branch);
    assert!(wt.path.join("feature.txt").exists());
    assert!(!wt.path.join("local.txt").exists());
    // Sin upstream: un `git push` suelto no puede caer en main
    assert!(git(&wt.path, &["rev-parse", "--abbrev-ref", "@{upstream}"]).is_err());
    assert!(fx.origin.exists());
    Ok(())
}

#[tokio::test]
async fn copies_env_links_dependencies_and_keeps_status_clean() -> TestResult {
    let fx = fixture()?;
    std::fs::write(fx.checkout.join(".env"), "TOKEN=secret\n")?;
    std::fs::create_dir_all(fx.checkout.join("node_modules").join("left-pad"))?;
    std::fs::write(fx.checkout.join("node_modules/left-pad/index.js"), "x\n")?;
    std::fs::write(fx.checkout.join("AGENTS.md"), "rules\n")?;

    let wt = worktree::create(&fx.config, &fx.checkout, &new_id(), "main").await?;

    let env = wt.path.join(".env");
    assert!(!env.is_symlink());
    assert_eq!(std::fs::read_to_string(&env)?, "TOKEN=secret\n");
    for linked in ["node_modules", "AGENTS.md"] {
        let link = wt.path.join(linked);
        assert!(link.is_symlink(), "{linked} should be a link");
        assert_eq!(std::fs::read_link(&link)?, fx.checkout.join(linked));
    }
    assert!(!wt.path.join(".venv").exists());
    assert_eq!(wt.copied, vec![PathBuf::from(".env")]);
    assert_eq!(
        wt.linked,
        vec![PathBuf::from("node_modules"), PathBuf::from("AGENTS.md")]
    );
    // `node_modules/` con barra no casa con un enlace: sin el exclude saldría aquí
    assert_eq!(
        git(
            &wt.path,
            &["status", "--porcelain", "--untracked-files=all"]
        )?,
        ""
    );
    Ok(())
}

#[tokio::test]
async fn tracked_files_are_never_replaced_by_the_checkout_copy() -> TestResult {
    let fx = fixture()?;
    commit_file(&fx.checkout, "CLAUDE.md", "tracked\n")?;
    git(&fx.checkout, &["push", "--quiet", "origin", "HEAD:main"])?;
    std::fs::write(fx.checkout.join("CLAUDE.md"), "edited locally\n")?;

    let wt = worktree::create(&fx.config, &fx.checkout, &new_id(), "main").await?;

    assert!(!wt.path.join("CLAUDE.md").is_symlink());
    assert_eq!(
        std::fs::read_to_string(wt.path.join("CLAUDE.md"))?,
        "tracked\n"
    );
    assert!(wt.linked.is_empty());
    Ok(())
}

#[tokio::test]
async fn refuses_to_remove_unintegrated_work_and_keeps_everything() -> TestResult {
    let fx = fixture()?;
    let wt = worktree::create(&fx.config, &fx.checkout, &new_id(), "main").await?;
    commit_file(&wt.path, "work.txt", "agent output\n")?;

    let result = worktree::remove(&fx.config, &wt, Removal::IfIntegrated).await;

    assert!(matches!(result, Err(WorktreeError::NotIntegrated { .. })));
    assert!(wt.path.join("work.txt").exists());
    git(&fx.checkout, &["rev-parse", "--verify", &wt.branch])?;
    Ok(())
}

#[tokio::test]
async fn refuses_to_remove_a_dirty_worktree() -> TestResult {
    let fx = fixture()?;
    let wt = worktree::create(&fx.config, &fx.checkout, &new_id(), "main").await?;
    std::fs::write(wt.path.join("half-done.txt"), "not committed\n")?;

    let result = worktree::remove(&fx.config, &wt, Removal::IfIntegrated).await;

    assert!(matches!(result, Err(WorktreeError::Dirty(_))));
    assert!(wt.path.join("half-done.txt").exists());
    Ok(())
}

#[tokio::test]
async fn removes_an_integrated_worktree_without_touching_linked_targets() -> TestResult {
    let fx = fixture()?;
    std::fs::create_dir_all(fx.checkout.join(".venv").join("bin"))?;
    std::fs::write(fx.checkout.join(".venv/bin/python"), "py\n")?;
    let wt = worktree::create(&fx.config, &fx.checkout, &new_id(), "main").await?;
    commit_file(&wt.path, "work.txt", "agent output\n")?;
    git(&wt.path, &["push", "--quiet", "origin", "HEAD:main"])?;

    assert!(worktree::is_integrated(&fx.config, &wt).await?);
    worktree::remove(&fx.config, &wt, Removal::IfIntegrated).await?;

    assert!(!wt.path.exists());
    assert!(git(&fx.checkout, &["rev-parse", "--verify", &wt.branch]).is_err());
    // Borrar el worktree no puede seguir el enlace y llevarse el .venv de Juan
    assert_eq!(
        std::fs::read_to_string(fx.checkout.join(".venv/bin/python"))?,
        "py\n"
    );
    Ok(())
}

#[tokio::test]
async fn discard_removes_dirty_unintegrated_work_on_purpose() -> TestResult {
    let fx = fixture()?;
    let wt = worktree::create(&fx.config, &fx.checkout, &new_id(), "main").await?;
    commit_file(&wt.path, "work.txt", "agent output\n")?;
    std::fs::write(wt.path.join("half-done.txt"), "not committed\n")?;

    worktree::remove(&fx.config, &wt, Removal::Discard).await?;

    assert!(!wt.path.exists());
    assert!(git(&fx.checkout, &["rev-parse", "--verify", &wt.branch]).is_err());
    assert_eq!(
        git(&fx.checkout, &["worktree", "list", "--porcelain"])?
            .matches("worktree ")
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn a_folder_deleted_by_hand_is_pruned_on_removal() -> TestResult {
    let fx = fixture()?;
    let wt = worktree::create(&fx.config, &fx.checkout, &new_id(), "main").await?;
    std::fs::remove_dir_all(&wt.path)?;

    worktree::remove(&fx.config, &wt, Removal::Discard).await?;

    assert_eq!(
        git(&fx.checkout, &["worktree", "list", "--porcelain"])?
            .matches("worktree ")
            .count(),
        1
    );
    assert!(git(&fx.checkout, &["rev-parse", "--verify", &wt.branch]).is_err());
    Ok(())
}

#[tokio::test]
async fn rejects_bad_bases_existing_folders_and_relative_repos() -> TestResult {
    let fx = fixture()?;
    let id = new_id();

    for base in ["--upload-pack=evil", "bad..name", ""] {
        let result = worktree::create(&fx.config, &fx.checkout, &id, base).await;
        assert!(
            matches!(result, Err(WorktreeError::InvalidBase(_))),
            "{base}"
        );
    }
    let missing = worktree::create(&fx.config, &fx.checkout, &id, "no-such-branch").await;
    assert!(matches!(missing, Err(WorktreeError::Git { .. })));

    let relative = worktree::create(&fx.config, Path::new("app"), &id, "main").await;
    assert!(matches!(relative, Err(WorktreeError::InvalidRepo(_))));

    std::fs::create_dir_all(fx.config.path_for(&fx.checkout, &id)?)?;
    let taken = worktree::create(&fx.config, &fx.checkout, &id, "main").await;
    assert!(matches!(taken, Err(WorktreeError::AlreadyExists(_))));

    let mut config = fx.config.clone();
    config.linked.push(PathBuf::from("../outside"));
    let escaping = worktree::create(&config, &fx.checkout, &new_id(), "main").await;
    assert!(matches!(escaping, Err(WorktreeError::InvalidEntry(_))));
    Ok(())
}
