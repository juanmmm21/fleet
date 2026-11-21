//! Estado de la TUI, sin terminal ni red: las teclas y los eventos del demonio lo cambian y
//! devuelven la [`Action`] que el bucle tiene que hacer contra la API.

use std::collections::{HashMap, VecDeque};

use fleet_core::{Task, TaskId, TaskState};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;

use crate::client::StreamEvent;

/// Líneas de salida que se guardan por tarea.
const MAX_LINES: usize = 2000;

/// Qué se ve a la derecha.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Output,
    Diff,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    /// Escribiendo la respuesta a la pregunta de la tarea.
    Answering {
        id: TaskId,
        text: String,
    },
    /// Esperando la confirmación para descartar.
    ConfirmDrop {
        id: TaskId,
    },
}

/// Lo que el bucle tiene que hacer tras una tecla.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    Refresh,
    LoadLog(TaskId),
    LoadDiff(TaskId),
    Merge(TaskId),
    Cancel(TaskId),
    Drop(TaskId),
    Answer(TaskId, String),
}

#[derive(Debug, Clone)]
pub struct App {
    tasks: Vec<Task>,
    selected: usize,
    view: View,
    mode: Mode,
    output: HashMap<TaskId, VecDeque<String>>,
    diff: Option<(TaskId, String)>,
    /// Última línea de estado (lo que pasó con la última acción).
    status: String,
    /// Si hay conexión con el demonio.
    connected: bool,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    #[must_use]
    pub fn new() -> Self {
        Self {
            tasks: Vec::new(),
            selected: 0,
            view: View::Output,
            mode: Mode::Normal,
            output: HashMap::new(),
            diff: None,
            status: "q quit · ↑↓ select · tab output/diff · m merge · a answer · c cancel · d drop"
                .to_owned(),
            connected: false,
        }
    }

    #[must_use]
    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    #[must_use]
    pub fn selected_index(&self) -> Option<usize> {
        (!self.tasks.is_empty()).then_some(self.selected)
    }

    #[must_use]
    pub fn selected(&self) -> Option<&Task> {
        self.tasks.get(self.selected)
    }

    #[must_use]
    pub fn view(&self) -> View {
        self.view
    }

    #[must_use]
    pub fn mode(&self) -> &Mode {
        &self.mode
    }

    #[must_use]
    pub fn status(&self) -> &str {
        &self.status
    }

    #[must_use]
    pub fn connected(&self) -> bool {
        self.connected
    }

    /// Salida guardada de la tarea seleccionada.
    #[must_use]
    pub fn selected_output(&self) -> Option<&VecDeque<String>> {
        self.selected().and_then(|task| self.output.get(&task.id))
    }

    /// Diff de la tarea seleccionada, si es el que hay cargado.
    #[must_use]
    pub fn selected_diff(&self) -> Option<&str> {
        let task = self.selected()?;
        self.diff
            .as_ref()
            .filter(|(id, _)| id == &task.id)
            .map(|(_, diff)| diff.as_str())
    }

    pub fn set_status(&mut self, status: impl Into<String>) {
        self.status = status.into();
    }

    pub fn set_connected(&mut self, connected: bool) {
        self.connected = connected;
    }

    /// Lista completa del demonio; mantiene seleccionada la misma tarea si sigue ahí. La
    /// primera vez se selecciona la más nueva.
    pub fn set_tasks(&mut self, tasks: Vec<Task>) -> Action {
        let previous = self.selected().map(|task| task.id.clone());
        let first_load = self.tasks.is_empty();
        self.tasks = tasks;
        self.selected = match previous {
            Some(id) => self.tasks.iter().position(|t| t.id == id).unwrap_or(0),
            None if first_load => self.tasks.len().saturating_sub(1),
            None => 0,
        };
        self.load_selected()
    }

    /// Todo el registro de una tarea, tal como lo da `/log`.
    pub fn set_log(&mut self, id: TaskId, log: &str) {
        let mut lines: VecDeque<String> = log.lines().map(str::to_owned).collect();
        while lines.len() > MAX_LINES {
            lines.pop_front();
        }
        self.output.insert(id, lines);
    }

    pub fn set_diff(&mut self, id: TaskId, diff: String) {
        self.diff = Some((id, diff));
    }

    /// Un evento del demonio.
    pub fn on_event(&mut self, event: &StreamEvent) -> Action {
        match event.name.as_str() {
            "task" => {
                let task = event
                    .data
                    .get("task")
                    .cloned()
                    .and_then(|task| serde_json::from_value::<Task>(task).ok());
                if let Some(task) = task {
                    self.upsert(task);
                }
                Action::None
            }
            "output" => {
                let field = |name: &str| event.data.get(name).and_then(Value::as_str);
                let id = field("id").and_then(|id| TaskId::parse(id).ok());
                if let (Some(id), Some(line)) = (id, field("line")) {
                    let prefix = if field("stream") == Some("stderr") {
                        "[stderr] "
                    } else {
                        ""
                    };
                    let lines = self.output.entry(id).or_default();
                    lines.push_back(format!("{prefix}{line}"));
                    if lines.len() > MAX_LINES {
                        lines.pop_front();
                    }
                }
                Action::None
            }
            "overlaps" => {
                if let Some(Value::Array(overlaps)) = event.data.get("overlaps")
                    && !overlaps.is_empty()
                {
                    self.status =
                        format!("{} pair(s) of tasks touch the same files", overlaps.len());
                }
                Action::None
            }
            // Se perdieron eventos: lo seguro es volver a pedir la lista entera
            "lagged" => Action::Refresh,
            _ => Action::None,
        }
    }

    fn upsert(&mut self, task: Task) {
        match self.tasks.iter().position(|t| t.id == task.id) {
            Some(index) => self.tasks[index] = task,
            None => {
                let was_empty = self.tasks.is_empty();
                self.tasks.push(task);
                self.tasks.sort_by(|a, b| a.id.cmp(&b.id));
                if was_empty {
                    self.selected = 0;
                }
            }
        }
    }

    fn load_selected(&self) -> Action {
        let Some(task) = self.selected() else {
            return Action::None;
        };
        match self.view {
            View::Output if !self.output.contains_key(&task.id) => Action::LoadLog(task.id.clone()),
            View::Diff if task.worktree.is_some() => Action::LoadDiff(task.id.clone()),
            View::Output | View::Diff => Action::None,
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Action::Quit;
        }
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Normal => self.on_normal_key(key.code),
            Mode::Answering { id, mut text } => match key.code {
                KeyCode::Enter if !text.trim().is_empty() => Action::Answer(id, text),
                KeyCode::Esc => {
                    self.status = "answer cancelled".to_owned();
                    Action::None
                }
                code => {
                    match code {
                        KeyCode::Char(c) => text.push(c),
                        KeyCode::Backspace => {
                            text.pop();
                        }
                        _ => {}
                    }
                    self.mode = Mode::Answering { id, text };
                    Action::None
                }
            },
            Mode::ConfirmDrop { id } => {
                if key.code == KeyCode::Char('y') {
                    Action::Drop(id)
                } else {
                    self.status = "drop cancelled".to_owned();
                    Action::None
                }
            }
        }
    }

    fn on_normal_key(&mut self, code: KeyCode) -> Action {
        let selected = self.selected().map(|task| (task.id.clone(), task.state));
        match code {
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Char('r') => Action::Refresh,
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                self.load_selected()
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.tasks.len() {
                    self.selected += 1;
                }
                self.load_selected()
            }
            KeyCode::Tab => {
                self.view = match self.view {
                    View::Output => View::Diff,
                    View::Diff => View::Output,
                };
                match (self.view, self.selected()) {
                    // El diff cambia mientras el agente trabaja: se pide de nuevo cada vez
                    (View::Diff, Some(task)) if task.worktree.is_some() => {
                        Action::LoadDiff(task.id.clone())
                    }
                    _ => self.load_selected(),
                }
            }
            KeyCode::Char('m') => match selected {
                Some((id, TaskState::Ready)) => Action::Merge(id),
                Some(_) => self.refuse("only a ready task can be merged"),
                None => Action::None,
            },
            KeyCode::Char('c') => match selected {
                Some((id, _)) => Action::Cancel(id),
                None => Action::None,
            },
            KeyCode::Char('a') => match selected {
                Some((id, TaskState::Waiting)) => {
                    self.mode = Mode::Answering {
                        id,
                        text: String::new(),
                    };
                    Action::None
                }
                Some(_) => self.refuse("only a waiting task has a question"),
                None => Action::None,
            },
            KeyCode::Char('d') => match selected {
                Some((id, state)) if !state.is_terminal() => {
                    self.status = format!("drop {id}? its worktree and branch go away (y/n)");
                    self.mode = Mode::ConfirmDrop { id };
                    Action::None
                }
                Some(_) => self.refuse("the task is already finished"),
                None => Action::None,
            },
            _ => Action::None,
        }
    }

    fn refuse(&mut self, why: &str) -> Action {
        self.status = why.to_owned();
        Action::None
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use fleet_core::{AgentProvider, TaskSpec};
    use serde_json::json;

    use super::*;

    fn task(id: &str, state: TaskState) -> Result<Task, Box<dyn std::error::Error>> {
        let spec = TaskSpec {
            repo: PathBuf::from("/repo"),
            prompt: format!("task {id}"),
            agent: AgentProvider::Claude { model: None },
            base: "main".to_owned(),
        };
        let mut task = Task::new(TaskId::parse(id)?, spec, 1)?;
        let path: &[TaskState] = match state {
            TaskState::Provisioning => &[],
            TaskState::Agent => &[TaskState::Agent],
            TaskState::Waiting => &[TaskState::Agent, TaskState::Waiting],
            TaskState::Ready => &[TaskState::Agent, TaskState::Preflight, TaskState::Ready],
            TaskState::Dropped => &[TaskState::Dropped],
            other => return Err(format!("unsupported {other}").into()),
        };
        for step in path {
            task.transition(*step, 2)?;
        }
        Ok(task)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn id(raw: &str) -> Result<TaskId, Box<dyn std::error::Error>> {
        Ok(TaskId::parse(raw)?)
    }

    #[test]
    fn selects_the_newest_first_and_keeps_the_selection() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut app = App::new();
        let action = app.set_tasks(vec![
            task("a", TaskState::Agent)?,
            task("b", TaskState::Ready)?,
        ]);
        assert_eq!(action, Action::LoadLog(id("b")?));
        assert_eq!(app.on_key(key(KeyCode::Up)), Action::LoadLog(id("a")?));
        app.set_log(id("a")?, "one\ntwo\n");
        // Con el registro ya cargado no se vuelve a pedir
        assert_eq!(
            app.set_tasks(vec![
                task("a", TaskState::Agent)?,
                task("c", TaskState::Agent)?
            ]),
            Action::None
        );
        assert_eq!(app.selected().map(|t| t.id.as_str()), Some("a"));
        assert_eq!(app.selected_output().map(VecDeque::len), Some(2));
        Ok(())
    }

    #[test]
    fn events_update_tasks_and_output() -> Result<(), Box<dyn std::error::Error>> {
        let mut app = App::new();
        app.set_tasks(vec![task("a", TaskState::Agent)?]);
        let ready = serde_json::to_value(task("a", TaskState::Ready)?)?;
        app.on_event(&StreamEvent {
            name: "task".to_owned(),
            data: json!({ "type": "task", "task": ready }),
        });
        assert_eq!(app.selected().map(|t| t.state), Some(TaskState::Ready));
        app.on_event(&StreamEvent {
            name: "output".to_owned(),
            data: json!({ "type": "output", "id": "a", "stream": "stderr", "line": "careful" }),
        });
        assert_eq!(
            app.selected_output()
                .and_then(|lines| lines.back())
                .map(String::as_str),
            Some("[stderr] careful")
        );
        let new = serde_json::to_value(task("b", TaskState::Provisioning)?)?;
        app.on_event(&StreamEvent {
            name: "task".to_owned(),
            data: json!({ "task": new }),
        });
        assert_eq!(app.tasks().len(), 2);
        assert_eq!(
            app.on_event(&StreamEvent {
                name: "lagged".to_owned(),
                data: json!({ "skipped": 3 })
            }),
            Action::Refresh
        );
        Ok(())
    }

    #[test]
    fn actions_only_where_they_make_sense() -> Result<(), Box<dyn std::error::Error>> {
        let mut app = App::new();
        app.set_tasks(vec![task("a", TaskState::Agent)?]);
        assert_eq!(app.on_key(key(KeyCode::Char('m'))), Action::None);
        assert_eq!(app.status(), "only a ready task can be merged");
        assert_eq!(
            app.on_key(key(KeyCode::Char('c'))),
            Action::Cancel(id("a")?)
        );

        app.set_tasks(vec![task("a", TaskState::Ready)?]);
        assert_eq!(app.on_key(key(KeyCode::Char('m'))), Action::Merge(id("a")?));

        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(app.on_key(key(KeyCode::Char('n'))), Action::None);
        assert_eq!(app.mode(), &Mode::Normal);
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(app.on_key(key(KeyCode::Char('y'))), Action::Drop(id("a")?));

        app.set_tasks(vec![task("a", TaskState::Dropped)?]);
        assert_eq!(app.on_key(key(KeyCode::Char('d'))), Action::None);
        assert_eq!(app.on_key(key(KeyCode::Char('q'))), Action::Quit);
        Ok(())
    }

    #[test]
    fn answering_a_question_with_a_text_field() -> Result<(), Box<dyn std::error::Error>> {
        let mut app = App::new();
        app.set_tasks(vec![task("a", TaskState::Waiting)?]);
        assert_eq!(app.on_key(key(KeyCode::Char('a'))), Action::None);
        for c in "sqlx".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Backspace));
        // `q` dentro del campo es texto, no salir
        assert_eq!(app.on_key(key(KeyCode::Char('q'))), Action::None);
        assert_eq!(
            app.mode(),
            &Mode::Answering {
                id: id("a")?,
                text: "sqlq".to_owned()
            }
        );
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Action::Answer(id("a")?, "sqlq".to_owned())
        );
        assert_eq!(app.mode(), &Mode::Normal);
        Ok(())
    }

    #[test]
    fn tab_switches_to_a_fresh_diff() -> Result<(), Box<dyn std::error::Error>> {
        let mut app = App::new();
        let mut working = task("a", TaskState::Agent)?;
        working.worktree = Some(fleet_core::Worktree {
            repo: PathBuf::from("/repo"),
            path: PathBuf::from("/wt/a"),
            branch: "fleet/a".to_owned(),
            base: "main".to_owned(),
            base_commit: "abc".to_owned(),
            copied: Vec::new(),
            linked: Vec::new(),
        });
        app.set_tasks(vec![working]);
        assert_eq!(app.on_key(key(KeyCode::Tab)), Action::LoadDiff(id("a")?));
        app.set_diff(id("a")?, "+line".to_owned());
        assert_eq!(app.selected_diff(), Some("+line"));
        assert_eq!(app.on_key(key(KeyCode::Tab)), Action::LoadLog(id("a")?));
        Ok(())
    }

    #[test]
    fn ctrl_c_always_quits() -> Result<(), Box<dyn std::error::Error>> {
        let mut app = App::new();
        app.set_tasks(vec![task("a", TaskState::Waiting)?]);
        app.on_key(key(KeyCode::Char('a')));
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(app.on_key(ctrl_c), Action::Quit);
        Ok(())
    }
}
