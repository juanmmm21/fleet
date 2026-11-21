//! Dibujo de la TUI a partir de [`App`]; no cambia nada.

use fleet_core::TaskState;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};

use super::app::{App, Mode, View};

fn state_color(state: TaskState) -> Color {
    match state {
        TaskState::Provisioning | TaskState::Agent | TaskState::Preflight => Color::Cyan,
        TaskState::Waiting => Color::Yellow,
        TaskState::Ready => Color::Green,
        TaskState::Merged => Color::Blue,
        TaskState::Failed => Color::Red,
        TaskState::Dropped => Color::DarkGray,
    }
}

pub fn draw(frame: &mut Frame, app: &App) {
    let [main, status] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(frame.area());
    let [list_area, detail_area] =
        Layout::horizontal([Constraint::Length(44), Constraint::Min(20)]).areas(main);

    let items: Vec<ListItem> = app
        .tasks()
        .iter()
        .map(|task| {
            let first = task.prompt.lines().next().unwrap_or_default();
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(
                        format!("{:<12}", task.state.as_str()),
                        Style::new().fg(state_color(task.state)),
                    ),
                    Span::raw(task.id.as_str().to_owned()),
                ]),
                Line::from(Span::styled(
                    format!("  {first}"),
                    Style::new().fg(Color::Gray),
                )),
            ])
        })
        .collect();
    let title = if app.connected() {
        " tasks "
    } else {
        " tasks (fleetd unreachable) "
    };
    let list = List::new(items)
        .block(Block::bordered().title(title))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default().with_selected(app.selected_index());
    frame.render_stateful_widget(list, list_area, &mut state);

    let (title, text) = match (app.view(), app.selected()) {
        (_, None) => (" output ".to_owned(), "no task selected".to_owned()),
        (View::Output, Some(task)) => {
            let mut lines: Vec<String> = Vec::new();
            if let Some(question) = &task.question {
                lines.push(format!("QUESTION: {question}  (press a to answer)"));
            }
            if let Some(failure) = &task.failure {
                lines.push(format!("FAILED: {failure}"));
            }
            if let Some(output) = app.selected_output() {
                lines.extend(output.iter().cloned());
            }
            (format!(" output · {} ", task.id), lines.join("\n"))
        }
        (View::Diff, Some(task)) => (
            format!(" diff · {} ", task.id),
            app.selected_diff().unwrap_or("no diff yet").to_owned(),
        ),
    };
    // Se ve el final: es donde está lo último que hizo el agente
    let height = usize::from(detail_area.height.saturating_sub(2));
    let total = text.lines().count();
    let scroll = u16::try_from(total.saturating_sub(height)).unwrap_or(u16::MAX);
    let detail = Paragraph::new(text)
        .block(Block::bordered().title(title))
        .scroll((
            if app.view() == View::Output {
                scroll
            } else {
                0
            },
            0,
        ))
        .wrap(Wrap { trim: false });
    frame.render_widget(detail, detail_area);

    let status_line = match app.mode() {
        Mode::Answering { id, text } => format!("answer {id}: {text}▏ (enter send · esc cancel)"),
        Mode::ConfirmDrop { .. } | Mode::Normal => app.status().to_owned(),
    };
    frame.render_widget(Paragraph::new(status_line), status);
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use fleet_core::{AgentProvider, Task, TaskId, TaskSpec};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    fn screen(app: &App) -> Result<String, Box<dyn std::error::Error>> {
        let mut terminal = Terminal::new(TestBackend::new(100, 12))?;
        terminal.draw(|frame| draw(frame, app))?;
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        Ok(text)
    }

    #[test]
    fn shows_tasks_their_state_and_the_question() -> Result<(), Box<dyn std::error::Error>> {
        let spec = TaskSpec {
            client: None,
            repo: PathBuf::from("/repo"),
            prompt: "store the tasks\nin sqlite".to_owned(),
            agent: AgentProvider::Claude { model: None },
            base: "main".to_owned(),
        };
        let mut task = Task::new(TaskId::parse("mi3x-0a1b")?, spec, 1)?;
        task.transition(TaskState::Agent, 2)?;
        task.ask("which crate?", 3)?;
        let mut app = App::new();
        app.set_tasks(vec![task]);
        app.set_log(
            TaskId::parse("mi3x-0a1b")?,
            "agent line one\nagent line two",
        );
        app.set_connected(true);
        let text = screen(&app)?;
        assert!(text.contains("waiting     mi3x-0a1b"), "{text}");
        assert!(text.contains("store the tasks"), "{text}");
        assert!(text.contains("QUESTION: which crate?"), "{text}");
        assert!(text.contains("agent line two"), "{text}");
        assert!(!text.contains("unreachable"), "{text}");
        Ok(())
    }

    #[test]
    fn says_when_the_daemon_is_down() -> Result<(), Box<dyn std::error::Error>> {
        let text = screen(&App::new())?;
        assert!(text.contains("fleetd unreachable"), "{text}");
        assert!(text.contains("no task selected"), "{text}");
        Ok(())
    }
}
