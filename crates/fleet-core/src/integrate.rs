//! Integración de una tarea lista: rebase sobre `origin/<base>`, pruebas otra vez sobre lo
//! rebasado y `push` sin forzar. Si algo sale mal, la rama se queda como estaba para mirarla.

use std::future::Future;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::preflight::{self, PreflightConfig, PreflightError, PreflightPlan, PreflightResult};
use crate::process;
use crate::task::{Task, TaskState, TransitionError};
use crate::worktree::{self, Worktree, WorktreeConfig, WorktreeError};

/// Cómo acabó un intento de integrar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum IntegrationOutcome {
    /// En `origin/<base>`; `commit` es la nueva punta.
    Merged { commit: String },
    /// El rebase choca con lo que entró en `origin` entretanto; se deshizo.
    Conflict { files: Vec<String> },
    /// Rebasada, pero las pruebas fallan sobre lo nuevo de `origin`. No se ha subido nada.
    Red(PreflightResult),
    /// Alguien subió a `origin/<base>` mientras se pasaban las pruebas: hay que repetir.
    OriginMoved,
}

#[derive(Debug, thiserror::Error)]
pub enum IntegrationError {
    #[error("{0} has uncommitted changes; the agent must commit its work")]
    Uncommitted(PathBuf),
    #[error("branch `{0}` has no commits to integrate")]
    NothingToIntegrate(String),
    #[error(transparent)]
    Worktree(#[from] WorktreeError),
    #[error(transparent)]
    Preflight(#[from] PreflightError),
}

/// Integra la rama del worktree en `origin/<base>`. Tras un rebase que sale bien,
/// `worktree.base_commit` pasa a ser la punta de `origin` sobre la que quedó la rama.
pub async fn integrate<C>(
    worktree_config: &WorktreeConfig,
    worktree: &mut Worktree,
    plan: &PreflightPlan,
    preflight_config: &PreflightConfig,
    log: &Path,
    cancel: C,
) -> Result<IntegrationOutcome, IntegrationError>
where
    C: Future<Output = ()>,
{
    let diff = worktree::diff(worktree_config, worktree).await?;
    if diff.uncommitted {
        return Err(IntegrationError::Uncommitted(worktree.path.clone()));
    }
    if diff.commits == 0 {
        return Err(IntegrationError::NothingToIntegrate(
            worktree.branch.clone(),
        ));
    }

    let path = worktree.path.clone();
    let remote_base = format!("refs/remotes/origin/{}", worktree.base);
    let refspec = format!("+refs/heads/{}:{remote_base}", worktree.base);
    worktree::git(
        worktree_config,
        &path,
        &["fetch", "--quiet", "origin", &refspec],
    )
    .await?;
    if let Some(files) = rebase(worktree_config, &path, &remote_base).await? {
        return Ok(IntegrationOutcome::Conflict { files });
    }
    let onto = worktree::git(worktree_config, &path, &["rev-parse", &remote_base]).await?;
    worktree.base_commit = onto.stdout.trim().to_owned();

    tokio::pin!(cancel);
    let native = preflight::run(&path, plan, preflight_config, log, cancel.as_mut()).await?;
    if !native.passed() {
        return Ok(IntegrationOutcome::Red(native));
    }
    if let Some(act) = &plan.act {
        let replayed = preflight::run_act(
            worktree,
            act,
            plan.timeout,
            preflight_config,
            log,
            cancel.as_mut(),
        )
        .await?;
        if !replayed.passed() {
            return Ok(IntegrationOutcome::Red(replayed));
        }
    }

    let target = format!("HEAD:refs/heads/{}", worktree.base);
    let mut push = worktree::git_command(&path);
    push.args(["push", "--quiet", "origin", &target]);
    let pushed = process::run(push, worktree_config.git_timeout)
        .await
        .map_err(WorktreeError::from)?;
    if !pushed.success() {
        if is_rejected(&pushed.stderr) {
            return Ok(IntegrationOutcome::OriginMoved);
        }
        return Err(worktree::git_failure(&path, &format!("push origin {target}"), &pushed).into());
    }
    let head = worktree::git(worktree_config, &path, &["rev-parse", "HEAD"]).await?;
    Ok(IntegrationOutcome::Merged {
        commit: head.stdout.trim().to_owned(),
    })
}

/// Lleva a una tarea `ready` al estado que toca tras intentar integrarla: `merged`, `failed`
/// con el motivo (conflicto o pruebas en rojo: la rama se queda para arreglarla) o sigue en
/// `ready` si `origin` se movió y basta con repetir.
pub fn settle(
    task: &mut Task,
    outcome: &IntegrationOutcome,
    now_ms: u64,
) -> Result<(), TransitionError> {
    if task.state != TaskState::Ready {
        return Err(TransitionError {
            from: task.state,
            to: TaskState::Merged,
        });
    }
    match outcome {
        IntegrationOutcome::Merged { .. } => task.transition(TaskState::Merged, now_ms),
        IntegrationOutcome::Conflict { files } => task.fail(
            format!("conflict with origin/{} in {}", task.base, files.join(", ")),
            now_ms,
        ),
        IntegrationOutcome::Red(result) => {
            task.preflight = Some(result.clone());
            task.fail(
                format!(
                    "tests fail on top of origin/{}: {}",
                    task.base,
                    result.summary()
                ),
                now_ms,
            )
        }
        IntegrationOutcome::OriginMoved => Ok(()),
    }
}

/// Rebasa sobre `onto`. Con conflictos deshace el rebase y devuelve los ficheros en conflicto;
/// cualquier otro fallo (sin identidad de git, por ejemplo) también se deshace y es un error.
async fn rebase(
    config: &WorktreeConfig,
    path: &Path,
    onto: &str,
) -> Result<Option<Vec<String>>, WorktreeError> {
    let mut command = worktree::git_command(path);
    command.args(["rebase", "--quiet", onto]);
    let output = process::run(command, config.git_timeout).await?;
    if output.success() {
        return Ok(None);
    }
    let conflicted = worktree::git(
        config,
        path,
        &["diff", "--name-only", "--diff-filter=U", "-z"],
    )
    .await;
    // Se deshace antes de mirar nada más: la rama tiene que quedar como estaba
    abort_rebase(config, path).await?;
    let files: Vec<String> = conflicted?
        .stdout
        .split('\0')
        .filter(|file| !file.is_empty())
        .map(str::to_owned)
        .collect();
    if files.is_empty() {
        return Err(worktree::git_failure(
            path,
            &format!("rebase {onto}"),
            &output,
        ));
    }
    Ok(Some(files))
}

async fn abort_rebase(config: &WorktreeConfig, path: &Path) -> Result<(), WorktreeError> {
    let mut command = worktree::git_command(path);
    command.args(["rebase", "--abort"]);
    let output = process::run(command, config.git_timeout).await?;
    // Si el rebase ni llegó a empezar no hay nada que abortar, y está bien
    if output.success() || output.stderr.contains("No rebase in progress") {
        Ok(())
    } else {
        Err(worktree::git_failure(path, "rebase --abort", &output))
    }
}

/// `git push` rechazado porque `origin` tiene commits que la rama no tiene.
fn is_rejected(stderr: &str) -> bool {
    ["[rejected]", "non-fast-forward", "fetch first"]
        .iter()
        .any(|marker| stderr.contains(marker))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::agent::AgentProvider;
    use crate::task::{TaskId, TaskSpec};

    fn ready_task() -> Result<Task, Box<dyn std::error::Error>> {
        let spec = TaskSpec {
            client: None,
            repo: PathBuf::from("/repo"),
            prompt: "do it".to_owned(),
            agent: AgentProvider::Claude { model: None },
            base: "main".to_owned(),
        };
        let mut task = Task::new(TaskId::parse("t")?, spec, 1)?;
        for state in [TaskState::Agent, TaskState::Preflight, TaskState::Ready] {
            task.transition(state, 2)?;
        }
        Ok(task)
    }

    #[test]
    fn settle_maps_each_outcome() -> Result<(), Box<dyn std::error::Error>> {
        let mut task = ready_task()?;
        settle(&mut task, &IntegrationOutcome::OriginMoved, 3)?;
        assert_eq!(task.state, TaskState::Ready);
        settle(
            &mut task,
            &IntegrationOutcome::Merged {
                commit: "abc".to_owned(),
            },
            4,
        )?;
        assert_eq!(task.state, TaskState::Merged);

        let mut task = ready_task()?;
        let conflict = IntegrationOutcome::Conflict {
            files: vec!["a.rs".to_owned(), "b.rs".to_owned()],
        };
        settle(&mut task, &conflict, 3)?;
        assert_eq!(task.state, TaskState::Failed);
        assert_eq!(
            task.failure.as_deref(),
            Some("conflict with origin/main in a.rs, b.rs")
        );

        let mut task = ready_task()?;
        let red = PreflightResult {
            command: vec!["cargo".to_owned(), "test".to_owned()],
            checks: Vec::new(),
            exit_code: Some(101),
            timed_out: false,
            duration_ms: 5,
            output_tail: String::new(),
        };
        settle(&mut task, &IntegrationOutcome::Red(red.clone()), 3)?;
        assert_eq!(
            task.failure.as_deref(),
            Some("tests fail on top of origin/main: `cargo test` exited with code 101")
        );
        assert_eq!(task.preflight, Some(red));
        assert!(settle(&mut task, &IntegrationOutcome::OriginMoved, 4).is_err());
        Ok(())
    }

    #[test]
    fn recognises_a_rejected_push() {
        assert!(is_rejected(
            " ! [rejected]        HEAD -> main (fetch first)\nerror: failed to push some refs"
        ));
        assert!(is_rejected(
            "hint: Updates were rejected ... (non-fast-forward)"
        ));
        assert!(!is_rejected("fatal: could not read Username"));
    }
}
