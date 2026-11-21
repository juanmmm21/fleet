//! Integración contra repos de git de verdad: un `origin` desnudo, el checkout y otro clon
//! que hace de compañero empujando a `origin` por su cuenta.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use fleet_core::TaskId;
use fleet_core::integrate::{self, IntegrationError, IntegrationOutcome};
use fleet_core::preflight::{self, PreflightConfig};
use fleet_core::worktree::{self, Worktree, WorktreeConfig};
use tempfile::TempDir;

mod common;
use common::{commit_file, git};

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Fixture {
    dir: TempDir,
    checkout: PathBuf,
    /// Otro clon que sube a `origin` mientras la tarea trabaja.
    teammate: PathBuf,
    config: WorktreeConfig,
}

impl Fixture {
    fn log(&self) -> PathBuf {
        self.dir.path().join("logs").join("integrate.log")
    }

    fn origin_main(&self) -> Result<String, Box<dyn std::error::Error>> {
        git(&self.dir.path().join("origin.git"), &["rev-parse", "main"])
    }

    fn teammate_pushes(&self, name: &str, content: &str) -> TestResult {
        git(
            &self.teammate,
            &["pull", "--quiet", "--rebase", "origin", "main"],
        )?;
        commit_file(&self.teammate, name, content)?;
        git(&self.teammate, &["push", "--quiet", "origin", "HEAD:main"])?;
        Ok(())
    }

    async fn task(&self) -> Result<Worktree, Box<dyn std::error::Error>> {
        let id = TaskId::generate(SystemTime::now());
        Ok(worktree::create(&self.config, &self.checkout, &id, "main").await?)
    }
}

/// `check.sh` es el pre-flight del repo: falla si existe `BROKEN`.
fn fixture() -> Result<Fixture, Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    git(dir.path(), &["init", "--quiet", "--bare", "origin.git"])?;
    git(dir.path(), &["clone", "--quiet", "origin.git", "app"])?;
    let checkout = dir.path().join("app");
    // El rebase de fleet necesita una identidad: va en la config de este repo de usar y tirar
    git(&checkout, &["config", "user.name", "fleet-test"])?;
    git(
        &checkout,
        &["config", "user.email", "fleet-test@example.invalid"],
    )?;
    commit_file(&checkout, "README.md", "line one\n")?;
    commit_file(
        &checkout,
        "fleet.toml",
        "[preflight]\ncommand = [\"sh\", \"check.sh\"]\n",
    )?;
    commit_file(&checkout, "check.sh", "test ! -e BROKEN && echo green\n")?;
    git(&checkout, &["push", "--quiet", "origin", "HEAD:main"])?;
    git(dir.path(), &["clone", "--quiet", "origin.git", "teammate"])?;
    let teammate = dir.path().join("teammate");
    let config = WorktreeConfig::new(dir.path().join("worktrees"));
    Ok(Fixture {
        dir,
        checkout,
        teammate,
        config,
    })
}

fn preflight_config() -> PreflightConfig {
    PreflightConfig {
        default_timeout: Duration::from_secs(30),
        grace: Duration::from_secs(1),
        ..PreflightConfig::default()
    }
}

async fn run(fx: &Fixture, wt: &mut Worktree) -> Result<IntegrationOutcome, IntegrationError> {
    let plan = preflight::plan(&wt.path, &preflight_config())?;
    integrate::integrate(
        &fx.config,
        wt,
        &plan,
        &preflight_config(),
        &fx.log(),
        std::future::pending(),
    )
    .await
}

fn head(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    git(path, &["rev-parse", "HEAD"])
}

#[tokio::test]
async fn rebases_onto_what_moved_tests_again_and_pushes() -> TestResult {
    let fx = fixture()?;
    let mut wt = fx.task().await?;
    commit_file(&wt.path, "feature.txt", "new feature\n")?;
    fx.teammate_pushes("other.txt", "someone else\n")?;

    let outcome = run(&fx, &mut wt).await?;
    let IntegrationOutcome::Merged { commit } = outcome else {
        return Err(format!("expected a merge, got {outcome:?}").into());
    };
    assert_eq!(fx.origin_main()?, commit);
    let files = git(
        &fx.dir.path().join("origin.git"),
        &["ls-tree", "--name-only", "main"],
    )?;
    assert!(
        files.contains("feature.txt") && files.contains("other.txt"),
        "{files}"
    );
    // La rama quedó encima de lo del compañero: un merge limpio, sin commits de merge
    let parents = git(&wt.path, &["rev-list", "--parents", "-1", "HEAD"])?;
    assert_eq!(parents.split(' ').count(), 2);
    assert_eq!(wt.base_commit, git(&wt.path, &["rev-parse", "HEAD~1"])?);
    assert!(worktree::is_integrated(&fx.config, &wt).await?);
    Ok(())
}

#[tokio::test]
async fn a_conflict_is_undone_and_the_branch_stays() -> TestResult {
    let fx = fixture()?;
    let mut wt = fx.task().await?;
    commit_file(&wt.path, "README.md", "line one, by the task\n")?;
    let before = head(&wt.path)?;
    let base_before = wt.base_commit.clone();
    let origin_before = {
        fx.teammate_pushes("README.md", "line one, by a teammate\n")?;
        fx.origin_main()?
    };

    let outcome = run(&fx, &mut wt).await?;
    assert_eq!(
        outcome,
        IntegrationOutcome::Conflict {
            files: vec!["README.md".to_owned()]
        }
    );
    assert_eq!(head(&wt.path)?, before);
    assert_eq!(wt.base_commit, base_before);
    assert_eq!(git(&wt.path, &["status", "--porcelain"])?, "");
    assert_eq!(fx.origin_main()?, origin_before);
    Ok(())
}

#[tokio::test]
async fn red_after_the_rebase_pushes_nothing() -> TestResult {
    let fx = fixture()?;
    let mut wt = fx.task().await?;
    commit_file(&wt.path, "feature.txt", "fine on its own\n")?;
    fx.teammate_pushes("BROKEN", "breaks the tests\n")?;
    let origin_before = fx.origin_main()?;

    let outcome = run(&fx, &mut wt).await?;
    let IntegrationOutcome::Red(result) = outcome else {
        return Err(format!("expected red, got {outcome:?}").into());
    };
    assert_eq!(result.exit_code, Some(1));
    assert_eq!(fx.origin_main()?, origin_before);
    // Se queda rebasada (y su base al día) para que el agente arregle sobre lo nuevo
    assert_eq!(wt.base_commit, origin_before);
    Ok(())
}

#[tokio::test]
async fn origin_moving_during_the_tests_is_reported_not_forced() -> TestResult {
    let fx = fixture()?;
    let mut wt = fx.task().await?;
    // El pre-flight de esta tarea hace que el compañero suba algo mientras se ejecuta
    let teammate = fx.teammate.display().to_string();
    let racing = format!(
        "git -C '{teammate}' -c user.name=t -c user.email=t@example.invalid \
         commit --quiet --allow-empty -m race && git -C '{teammate}' push --quiet origin HEAD:main\n"
    );
    commit_file(&wt.path, "check.sh", &racing)?;
    let origin_before_push = fx.origin_main()?;

    let outcome = run(&fx, &mut wt).await?;
    assert_eq!(outcome, IntegrationOutcome::OriginMoved);
    let origin_now = fx.origin_main()?;
    assert_ne!(origin_now, origin_before_push);
    assert_eq!(origin_now, head(&fx.teammate)?);
    Ok(())
}

#[tokio::test]
async fn uncommitted_work_or_no_commits_is_refused() -> TestResult {
    let fx = fixture()?;
    let mut empty = fx.task().await?;
    assert!(matches!(
        run(&fx, &mut empty).await,
        Err(IntegrationError::NothingToIntegrate(_))
    ));

    let mut dirty = fx.task().await?;
    commit_file(&dirty.path, "feature.txt", "committed\n")?;
    std::fs::write(dirty.path.join("forgotten.txt"), "not committed\n")?;
    let origin_before = fx.origin_main()?;
    assert!(matches!(
        run(&fx, &mut dirty).await,
        Err(IntegrationError::Uncommitted(_))
    ));
    assert_eq!(fx.origin_main()?, origin_before);
    Ok(())
}
