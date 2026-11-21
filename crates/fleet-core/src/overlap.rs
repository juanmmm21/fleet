//! Choques entre tareas: dos tareas vivas del mismo repo y la misma base que tocan los mismos
//! ficheros. Se avisa antes de integrar y se ordena la integración: la más antigua primero, y
//! la otra espera y luego se rebasa sobre ella (lo hace `integrate` al rebasar sobre `origin`).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::task::{Task, TaskId, TaskState};

/// Dos tareas que tocan los mismos ficheros; `a` es la más antigua.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overlap {
    pub a: TaskId,
    pub b: TaskId,
    pub files: Vec<String>,
}

/// Una tarea cuenta si está viva, no ha fallado y ya tiene diff. Una fallida no bloquea a
/// nadie: tendrá que rehacerse de todos modos y entonces se rebasará sobre lo integrado.
fn counts(task: &Task) -> bool {
    !task.state.is_terminal() && task.state != TaskState::Failed && task.diff.is_some()
}

fn same_target(a: &Task, b: &Task) -> bool {
    a.repo == b.repo && a.base == b.base
}

fn shared_files(a: &Task, b: &Task) -> Vec<String> {
    let (Some(da), Some(db)) = (&a.diff, &b.diff) else {
        return Vec::new();
    };
    let left: BTreeSet<&str> = da.files.iter().map(|f| f.path.as_str()).collect();
    db.files
        .iter()
        .map(|f| f.path.as_str())
        .filter(|path| left.contains(path))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// Todos los pares de tareas que se pisan, ordenados por la más antigua de cada par.
#[must_use]
pub fn overlaps(tasks: &[Task]) -> Vec<Overlap> {
    let mut live: Vec<&Task> = tasks.iter().filter(|t| counts(t)).collect();
    // Los ids se ordenan por la hora de creación
    live.sort_by(|a, b| a.id.cmp(&b.id));
    let mut found = Vec::new();
    for (i, a) in live.iter().enumerate() {
        for b in &live[i + 1..] {
            if !same_target(a, b) {
                continue;
            }
            let files = shared_files(a, b);
            if !files.is_empty() {
                found.push(Overlap {
                    a: a.id.clone(),
                    b: b.id.clone(),
                    files,
                });
            }
        }
    }
    found
}

/// Las tareas más antiguas que `task` con las que se pisa y que aún no se han integrado:
/// hasta que no salgan, `task` no debería integrarse.
#[must_use]
pub fn waits_for(task: &Task, tasks: &[Task]) -> Vec<TaskId> {
    if !counts(task) {
        return Vec::new();
    }
    let mut earlier: Vec<TaskId> = tasks
        .iter()
        .filter(|other| other.id < task.id && counts(other) && same_target(task, other))
        .filter(|other| !shared_files(task, other).is_empty())
        .map(|other| other.id.clone())
        .collect();
    earlier.sort();
    earlier
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::agent::AgentProvider;
    use crate::task::TaskSpec;
    use crate::worktree::{ChangeKind, Diff, FileChange};

    fn task(
        id: &str,
        repo: &str,
        files: &[&str],
        state: TaskState,
    ) -> Result<Task, Box<dyn std::error::Error>> {
        let spec = TaskSpec {
            repo: PathBuf::from(repo),
            prompt: "x".to_owned(),
            agent: AgentProvider::Claude { model: None },
            base: "main".to_owned(),
        };
        let mut task = Task::new(TaskId::parse(id)?, spec, 1)?;
        task.diff = Some(Diff {
            commits: 1,
            uncommitted: false,
            files: files
                .iter()
                .map(|path| FileChange {
                    path: (*path).to_owned(),
                    kind: ChangeKind::Modified,
                })
                .collect(),
        });
        let path: &[TaskState] = match state {
            TaskState::Agent => &[TaskState::Agent],
            TaskState::Ready => &[TaskState::Agent, TaskState::Preflight, TaskState::Ready],
            TaskState::Failed => &[TaskState::Agent, TaskState::Failed],
            TaskState::Merged => &[
                TaskState::Agent,
                TaskState::Preflight,
                TaskState::Ready,
                TaskState::Merged,
            ],
            other => return Err(format!("unsupported state {other}").into()),
        };
        for step in path {
            task.transition(*step, 2)?;
        }
        Ok(task)
    }

    #[test]
    fn finds_shared_files_between_live_tasks() -> Result<(), Box<dyn std::error::Error>> {
        let tasks = [
            task("c", "/app", &["src/a.rs", "README.md"], TaskState::Agent)?,
            task("a", "/app", &["src/a.rs", "src/b.rs"], TaskState::Ready)?,
            task("b", "/app", &["docs/x.md"], TaskState::Agent)?,
            task("d", "/other", &["src/a.rs"], TaskState::Agent)?,
        ];
        assert_eq!(
            overlaps(&tasks),
            [Overlap {
                a: TaskId::parse("a")?,
                b: TaskId::parse("c")?,
                files: vec!["src/a.rs".to_owned()],
            }]
        );
        Ok(())
    }

    #[test]
    fn finished_failed_and_diffless_tasks_do_not_count() -> Result<(), Box<dyn std::error::Error>> {
        let mut fresh = task("e", "/app", &["f"], TaskState::Agent)?;
        fresh.diff = None;
        let tasks = [
            task("a", "/app", &["f"], TaskState::Merged)?,
            task("b", "/app", &["f"], TaskState::Failed)?,
            task("c", "/app", &["f"], TaskState::Agent)?,
            fresh,
        ];
        assert!(overlaps(&tasks).is_empty());
        assert!(waits_for(&tasks[2], &tasks).is_empty());
        Ok(())
    }

    #[test]
    fn the_newer_task_waits_for_the_older_one() -> Result<(), Box<dyn std::error::Error>> {
        let tasks = [
            task("a", "/app", &["f", "g"], TaskState::Agent)?,
            task("b", "/app", &["g"], TaskState::Ready)?,
            task("c", "/app", &["f"], TaskState::Ready)?,
        ];
        assert!(waits_for(&tasks[0], &tasks).is_empty());
        assert_eq!(waits_for(&tasks[1], &tasks), [TaskId::parse("a")?]);
        assert_eq!(waits_for(&tasks[2], &tasks), [TaskId::parse("a")?]);
        Ok(())
    }

    #[test]
    fn a_different_base_never_collides() -> Result<(), Box<dyn std::error::Error>> {
        let mut release = task("b", "/app", &["f"], TaskState::Agent)?;
        release.base = "release".to_owned();
        let tasks = [task("a", "/app", &["f"], TaskState::Agent)?, release];
        assert!(overlaps(&tasks).is_empty());
        Ok(())
    }
}
