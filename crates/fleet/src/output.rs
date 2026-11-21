//! Cómo se ven las tareas en la terminal.

use std::io::{self, Write};

use fleet_core::{Task, TaskState};

/// Estados en los que la tarea está quieta esperando a alguien (o ya acabó).
#[must_use]
pub fn is_resting(state: TaskState) -> bool {
    !matches!(
        state,
        TaskState::Provisioning | TaskState::Agent | TaskState::Preflight
    )
}

fn repo_name(task: &Task) -> String {
    task.repo.file_name().map_or_else(
        || task.repo.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// La primera línea del encargo, recortada a `width` caracteres.
fn headline(prompt: &str, width: usize) -> String {
    let first = prompt.lines().next().unwrap_or_default().trim();
    if first.chars().count() <= width {
        return first.to_owned();
    }
    let cut: String = first.chars().take(width.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// Una línea por tarea, para `fleet ls`.
pub fn task_table(out: &mut impl Write, tasks: &[Task]) -> io::Result<()> {
    if tasks.is_empty() {
        return writeln!(out, "no tasks");
    }
    for task in tasks {
        writeln!(
            out,
            "{:<15}  {:<12}  {:<16}  {:<14}  {}",
            task.id,
            task.state,
            task.agent.to_string(),
            repo_name(task),
            headline(&task.prompt, 60)
        )?;
    }
    Ok(())
}

/// Todo lo que interesa de una tarea, para `fleet show`.
pub fn task_detail(out: &mut impl Write, task: &Task) -> io::Result<()> {
    writeln!(out, "task      {}", task.id)?;
    writeln!(out, "state     {}", task.state)?;
    writeln!(
        out,
        "repo      {} (base {})",
        task.repo.display(),
        task.base
    )?;
    writeln!(out, "branch    {}", task.branch)?;
    writeln!(out, "agent     {}", task.agent)?;
    if let Some(worktree) = &task.worktree {
        writeln!(out, "worktree  {}", worktree.path.display())?;
    }
    if let Some(question) = &task.question {
        writeln!(out, "question  {question}")?;
    }
    if let Some(failure) = &task.failure {
        writeln!(out, "failure   {failure}")?;
    }
    if let Some(preflight) = &task.preflight {
        writeln!(out, "preflight {}", preflight.summary())?;
    }
    if let Some(diff) = &task.diff {
        let pending = if diff.uncommitted {
            ", uncommitted changes"
        } else {
            ""
        };
        writeln!(out, "changes   {} commit(s){pending}", diff.commits)?;
        for file in &diff.files {
            writeln!(out, "          {:?} {}", file.kind, file.path)?;
        }
    }
    writeln!(out, "prompt")?;
    for line in task.prompt.lines() {
        writeln!(out, "  {line}")?;
    }
    Ok(())
}

/// Lo que se dice cuando una tarea seguida con `--follow` se queda quieta.
pub fn resting_line(out: &mut impl Write, task: &Task) -> io::Result<()> {
    write!(out, "== task {} is {}", task.id, task.state)?;
    if let Some(question) = &task.question {
        write!(
            out,
            ": {question} (answer with `fleet answer {} ...`)",
            task.id
        )?;
    } else if let Some(failure) = &task.failure {
        write!(out, ": {failure}")?;
    } else if task.state == TaskState::Ready {
        write!(out, " (integrate with `fleet merge {}`)", task.id)?;
    }
    writeln!(out)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use fleet_core::{AgentProvider, TaskId, TaskSpec};

    use super::*;

    fn task(prompt: &str) -> Result<Task, Box<dyn std::error::Error>> {
        let spec = TaskSpec {
            repo: PathBuf::from("/home/juan/Desarrollo/fleet"),
            prompt: prompt.to_owned(),
            agent: AgentProvider::Claude { model: None },
            base: "main".to_owned(),
        };
        Ok(Task::new(TaskId::parse("mi3x-0a1b")?, spec, 1)?)
    }

    fn render(write: impl FnOnce(&mut Vec<u8>) -> io::Result<()>) -> Result<String, io::Error> {
        let mut out = Vec::new();
        write(&mut out)?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    #[test]
    fn the_table_shows_one_line_per_task() -> Result<(), Box<dyn std::error::Error>> {
        let long = format!("{}\nsecond line", "x".repeat(80));
        let tasks = [task(&long)?];
        let text = render(|out| task_table(out, &tasks))?;
        assert_eq!(text.lines().count(), 1);
        assert!(text.starts_with("mi3x-0a1b"));
        assert!(text.contains("provisioning") && text.contains("fleet"));
        assert!(text.trim_end().ends_with('…'));
        assert_eq!(render(|out| task_table(out, &[]))?, "no tasks\n");
        Ok(())
    }

    #[test]
    fn the_detail_and_the_resting_line_say_what_to_do() -> Result<(), Box<dyn std::error::Error>> {
        let mut waiting = task("store tasks")?;
        waiting.transition(TaskState::Agent, 2)?;
        waiting.ask("sqlite or files?", 3)?;
        let detail = render(|out| task_detail(out, &waiting))?;
        assert!(detail.contains("question  sqlite or files?"), "{detail}");
        assert!(detail.contains("branch    fleet/mi3x-0a1b"), "{detail}");
        let resting = render(|out| resting_line(out, &waiting))?;
        assert_eq!(
            resting,
            "== task mi3x-0a1b is waiting: sqlite or files? (answer with `fleet answer mi3x-0a1b ...`)\n"
        );
        assert!(is_resting(TaskState::Waiting));
        assert!(!is_resting(TaskState::Preflight));
        Ok(())
    }
}
