//! Integración de una tarea lista: rebase sobre `origin/<base>`, pruebas otra vez sobre lo
//! rebasado y `push` sin forzar. Si algo sale mal, la rama se queda como estaba para mirarla.

use std::future::Future;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::preflight::{self, PreflightConfig, PreflightError, PreflightPlan, PreflightResult};
use crate::process;
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
    use super::*;

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
