//! Qué pasa con las tareas vivas cuando `fleetd` arranca de nuevo.
//!
//! Un agente o un pre-flight cortados por el reinicio no se relanzan solos: volver a lanzar
//! un agente gasta cupo de la suscripción y puede repetir trabajo a medias. La tarea pasa a
//! `failed` con el motivo y su rama y worktree se quedan; desde ahí se reintenta o se descarta.

use std::path::PathBuf;

use crate::store::{Store, StoreError};
use crate::task::{Task, TaskState, TransitionError};

/// Lo que se hizo con una tarea al arrancar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovery {
    /// Sigue igual: esperaba respuesta, estaba lista o ya había fallado.
    Kept,
    /// Se cortó a medias (aprovisionando, con el agente o en el pre-flight).
    Interrupted { was: TaskState },
    /// Su worktree ya no está en disco.
    WorktreeMissing { path: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovered {
    pub task: Task,
    pub action: Recovery,
}

#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Transition(#[from] TransitionError),
    #[error("cannot check worktree {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Revisa las tareas vivas del almacén, guarda las que cambian y las devuelve todas
/// (en el orden de [`Store::live`]) con lo que se hizo con cada una.
pub async fn recover(store: &Store, now_ms: u64) -> Result<Vec<Recovered>, RecoveryError> {
    let mut recovered = Vec::new();
    for mut task in store.live().await? {
        let action = decide(&task)?;
        match &action {
            Recovery::Kept => {}
            Recovery::Interrupted { was } => {
                task.fail(
                    format!("interrupted: fleetd restarted during {was}"),
                    now_ms,
                )?;
                store.update(&task).await?;
            }
            Recovery::WorktreeMissing { path } => {
                task.fail(format!("worktree {} is gone", path.display()), now_ms)?;
                store.update(&task).await?;
            }
        }
        recovered.push(Recovered { task, action });
    }
    Ok(recovered)
}

fn decide(task: &Task) -> Result<Recovery, RecoveryError> {
    // Una fallida ya tiene su motivo; no se pisa aunque además le falte el worktree
    if task.state == TaskState::Failed {
        return Ok(Recovery::Kept);
    }
    if let Some(path) = &task.worktree {
        // Una sola llamada a `stat`: no compensa mandarla a otro hilo
        let exists = path.try_exists().map_err(|source| RecoveryError::Io {
            path: path.clone(),
            source,
        })?;
        if !exists {
            return Ok(Recovery::WorktreeMissing { path: path.clone() });
        }
    }
    Ok(match task.state {
        TaskState::Provisioning | TaskState::Agent | TaskState::Preflight => {
            Recovery::Interrupted { was: task.state }
        }
        _ => Recovery::Kept,
    })
}
