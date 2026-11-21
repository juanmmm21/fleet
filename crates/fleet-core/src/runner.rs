//! Lanzar el agente de una tarea en su worktree: con plazo y cancelación, su salida a un
//! registro en disco (y en directo a quien escuche) y, al acabar, el diff de lo que hizo.

use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::{mpsc, oneshot};

use crate::agent::Invocation;
use crate::process::{self, OutputLine, ProcessError, Stream, StreamEnd, StreamOptions};
use crate::task::{Task, TaskState, TransitionError};
use crate::transcript::{AgentOutcome, AgentSummary, Transcript};
use crate::worktree::{self, Diff, Worktree, WorktreeConfig, WorktreeError};

/// Líneas en vuelo entre los lectores y el registro antes de frenar al proceso.
const CHANNEL_CAPACITY: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunConfig {
    pub timeout: Duration,
    /// Margen entre SIGTERM y SIGKILL al cortar el agente.
    pub grace: Duration,
    /// Bytes del final de la salida que van en el informe.
    pub tail_bytes: usize,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(60 * 60),
            grace: Duration::from_secs(10),
            tail_bytes: 16 * 1024,
        }
    }
}

/// Cómo acabó una vuelta del agente.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentReport {
    pub end: StreamEnd,
    pub duration_ms: u64,
    /// Registro completo de la salida (se le añade cada vuelta, no se pisa).
    pub log: PathBuf,
    /// Final de la salida, para mostrarlo sin abrir el registro.
    pub output_tail: String,
    /// Lo que hay en el worktree respecto a la base al acabar, aunque se cortara.
    pub diff: Diff,
    /// Lo que dijo el agente de sí mismo: resultado, sesión, cupo.
    pub summary: AgentSummary,
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Process(#[from] ProcessError),
    #[error(transparent)]
    Worktree(#[from] WorktreeError),
    #[error("cannot write the agent log {path}: {source}")]
    Log {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Lanza el agente de `invocation` dentro del worktree. Si su salida delata algo que no se
/// puede consentir (una CLI autenticada con una clave de pago), se corta en el acto.
pub async fn run_agent<C>(
    worktree_config: &WorktreeConfig,
    worktree: &Worktree,
    invocation: Invocation,
    config: &RunConfig,
    log: &Path,
    cancel: C,
    live: Option<mpsc::Sender<OutputLine>>,
) -> Result<AgentReport, RunError>
where
    C: Future<Output = ()>,
{
    let Invocation {
        mut command,
        stdin,
        transcript,
    } = invocation;
    command.current_dir(&worktree.path);
    worktree::isolate_git_env(&mut command);
    let program = command
        .as_std()
        .get_program()
        .to_string_lossy()
        .into_owned();
    let log_error = |source| RunError::Log {
        path: log.to_path_buf(),
        source,
    };
    let mut file = open_log(log).await.map_err(log_error)?;
    file.write_all(format!("== {} {program} started ==\n", now_ms()).as_bytes())
        .await
        .map_err(log_error)?;

    let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
    let options = StreamOptions {
        timeout: config.timeout,
        grace: config.grace,
    };
    let (abort_tx, abort_rx) = oneshot::channel::<()>();
    let stop = async move {
        tokio::select! {
            () = cancel => {}
            aborted = abort_rx => {
                // Si el registro acabó sin pedir el corte, el proceso sigue a lo suyo
                if aborted.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        }
    };
    let recorder = Recorder {
        file,
        live,
        tail: Tail::new(config.tail_bytes),
        transcript,
        abort: Some(abort_tx),
    };
    let (streamed, recorded) = tokio::join!(
        process::stream(command, options, Some(stdin), stop, tx),
        recorder.run(rx)
    );
    let Recorded {
        mut file,
        output_tail,
        summary,
        write_result,
    } = recorded;
    // El fallo del proceso manda: sin él no hay vuelta que registrar
    let output = streamed?;
    write_result.map_err(log_error)?;
    file.write_all(
        format!(
            "== {} {program} {} after {}s ==\n",
            now_ms(),
            describe(output.end),
            output.duration.as_secs()
        )
        .as_bytes(),
    )
    .await
    .map_err(log_error)?;
    file.flush().await.map_err(log_error)?;

    let diff = worktree::diff(worktree_config, worktree).await?;
    Ok(AgentReport {
        end: output.end,
        duration_ms: u64::try_from(output.duration.as_millis()).unwrap_or(u64::MAX),
        log: log.to_path_buf(),
        output_tail,
        diff,
        summary,
    })
}

/// Lleva a la tarea al estado que toca tras una vuelta del agente: `waiting` si pregunta,
/// `preflight` si acabó bien y `failed` con el motivo en cualquier otro caso. Guarda la sesión
/// para poder retomarla y el diff para ver choques con otras tareas. La tarea tiene que estar
/// en `agent`.
pub fn settle(task: &mut Task, report: &AgentReport, now_ms: u64) -> Result<(), TransitionError> {
    if task.state != TaskState::Agent {
        return Err(TransitionError {
            from: task.state,
            to: TaskState::Preflight,
        });
    }
    if let Some(session) = &report.summary.session_id {
        task.session_id = Some(session.clone());
    }
    task.diff = Some(report.diff.clone());
    match failure(report) {
        Some(reason) => task.fail(reason, now_ms),
        None => match report.summary.question() {
            Some(question) => task.ask(question, now_ms),
            None => task.transition(TaskState::Preflight, now_ms),
        },
    }
}

/// Por qué la vuelta no cuenta como buena, en orden de lo más útil de saber.
fn failure(report: &AgentReport) -> Option<String> {
    let summary = &report.summary;
    if let Some(reason) = &summary.refused {
        return Some(format!("stopped by fleet: {reason}"));
    }
    if summary.quota_exhausted {
        return Some("the agent ran out of quota".to_owned());
    }
    match report.end {
        StreamEnd::TimedOut => return Some("the agent timed out".to_owned()),
        StreamEnd::Cancelled => return Some("the agent was cancelled".to_owned()),
        StreamEnd::Exited(_) => {}
    }
    if let Some(AgentOutcome::Error { message }) = &summary.outcome {
        return Some(format!("the agent failed: {message}"));
    }
    match report.end {
        StreamEnd::Exited(Some(0)) if summary.outcome.is_some() => None,
        StreamEnd::Exited(Some(0)) => Some("the agent ended without a result".to_owned()),
        other => Some(format!("the agent {}", describe(other))),
    }
}

pub(crate) async fn open_log(path: &Path) -> std::io::Result<BufWriter<File>> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    Ok(BufWriter::new(file))
}

struct Recorder {
    file: BufWriter<File>,
    live: Option<mpsc::Sender<OutputLine>>,
    tail: Tail,
    transcript: Transcript,
    abort: Option<oneshot::Sender<()>>,
}

struct Recorded {
    file: BufWriter<File>,
    output_tail: String,
    summary: AgentSummary,
    write_result: std::io::Result<()>,
}

impl Recorder {
    /// Escribe cada línea en el registro, guarda el final, la reenvía en directo y se la pasa
    /// al lector del agente. Un fallo al escribir no corta el bucle: hay que seguir vaciando el
    /// canal para no frenar al agente.
    async fn run(mut self, mut rx: mpsc::Receiver<OutputLine>) -> Recorded {
        let mut write_result = Ok(());
        while let Some(line) = rx.recv().await {
            let text = match line.stream {
                Stream::Stdout => {
                    self.transcript.feed(&line.line);
                    format!("{}\n", line.line)
                }
                Stream::Stderr => format!("[stderr] {}\n", line.line),
            };
            if write_result.is_ok() {
                write_result = self.file.write_all(text.as_bytes()).await;
            }
            self.tail.push(text);
            if let Some(reason) = self.transcript.must_stop()
                && let Some(abort) = self.abort.take()
            {
                if write_result.is_ok() {
                    let note = format!("== fleet stopped the agent: {reason} ==\n");
                    write_result = self.file.write_all(note.as_bytes()).await;
                }
                // Si el proceso ya terminó nadie escucha el corte, y da igual
                abort.send(()).ok();
            }
            if let Some(sender) = &self.live
                && sender.send(line).await.is_err()
            {
                self.live = None;
            }
        }
        Recorded {
            file: self.file,
            output_tail: self.tail.into_string(),
            summary: self.transcript.finish(),
            write_result,
        }
    }
}

/// Las últimas líneas que caben en un número de bytes.
pub(crate) struct Tail {
    limit: usize,
    size: usize,
    lines: VecDeque<String>,
}

impl Tail {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            limit,
            size: 0,
            lines: VecDeque::new(),
        }
    }

    pub(crate) fn push(&mut self, line: String) {
        // Con stream-json una sola línea puede ocupar más que todo el final: se guarda su cola
        if line.len() > self.limit {
            let mut cut = line.len() - self.limit;
            while !line.is_char_boundary(cut) {
                cut += 1;
            }
            self.lines.clear();
            self.size = line.len() - cut;
            self.lines.push_back(line[cut..].to_owned());
            return;
        }
        self.size += line.len();
        self.lines.push_back(line);
        while self.size > self.limit {
            match self.lines.pop_front() {
                Some(old) => self.size -= old.len(),
                None => break,
            }
        }
    }

    pub(crate) fn into_string(self) -> String {
        self.lines.into_iter().collect()
    }
}

fn describe(end: StreamEnd) -> String {
    match end {
        StreamEnd::Exited(Some(code)) => format!("exited with code {code}"),
        StreamEnd::Exited(None) => "was killed by a signal".to_owned(),
        StreamEnd::TimedOut => "timed out".to_owned(),
        StreamEnd::Cancelled => "was cancelled".to_owned(),
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::agent::AgentProvider;
    use crate::task::{TaskId, TaskSpec};

    fn agent_task() -> Result<Task, Box<dyn std::error::Error>> {
        let spec = TaskSpec {
            repo: PathBuf::from("/repo"),
            prompt: "do it".to_owned(),
            agent: AgentProvider::Claude { model: None },
            base: "main".to_owned(),
        };
        let mut task = Task::new(TaskId::parse("t")?, spec, 1)?;
        task.transition(TaskState::Agent, 2)?;
        Ok(task)
    }

    fn report(end: StreamEnd, outcome: Option<AgentOutcome>) -> AgentReport {
        AgentReport {
            end,
            duration_ms: 10,
            log: PathBuf::from("/logs/t.log"),
            output_tail: String::new(),
            diff: Diff::default(),
            summary: AgentSummary {
                session_id: Some("s-1".to_owned()),
                outcome,
                ..AgentSummary::default()
            },
        }
    }

    fn success(text: &str) -> Option<AgentOutcome> {
        Some(AgentOutcome::Success {
            text: text.to_owned(),
        })
    }

    #[test]
    fn a_good_run_goes_to_preflight_and_keeps_the_session() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut task = agent_task()?;
        settle(
            &mut task,
            &report(StreamEnd::Exited(Some(0)), success("done")),
            3,
        )?;
        assert_eq!(task.state, TaskState::Preflight);
        assert_eq!(task.session_id.as_deref(), Some("s-1"));
        Ok(())
    }

    #[test]
    fn a_question_waits_and_the_answer_resumes() -> Result<(), Box<dyn std::error::Error>> {
        let mut task = agent_task()?;
        let asked = report(StreamEnd::Exited(Some(0)), success("QUESTION: sqlite?"));
        settle(&mut task, &asked, 3)?;
        assert_eq!(task.state, TaskState::Waiting);
        assert_eq!(task.question.as_deref(), Some("sqlite?"));
        task.transition(TaskState::Agent, 4)?;
        assert_eq!(task.question, None);
        assert_eq!(task.session_id.as_deref(), Some("s-1"));
        Ok(())
    }

    #[test]
    fn every_bad_ending_fails_with_its_reason() -> Result<(), Box<dyn std::error::Error>> {
        let error = Some(AgentOutcome::Error {
            message: "boom".to_owned(),
        });
        let cases = [
            (report(StreamEnd::TimedOut, None), "the agent timed out"),
            (
                report(StreamEnd::Cancelled, success("QUESTION: x")),
                "the agent was cancelled",
            ),
            (
                report(StreamEnd::Exited(Some(1)), error),
                "the agent failed: boom",
            ),
            (
                report(StreamEnd::Exited(Some(2)), success("ok")),
                "the agent exited with code 2",
            ),
            (
                report(StreamEnd::Exited(None), None),
                "the agent was killed by a signal",
            ),
            (
                report(StreamEnd::Exited(Some(0)), None),
                "the agent ended without a result",
            ),
        ];
        for (report, reason) in cases {
            let mut task = agent_task()?;
            settle(&mut task, &report, 3)?;
            assert_eq!(task.state, TaskState::Failed);
            assert_eq!(task.failure.as_deref(), Some(reason));
        }

        let mut quota = report(StreamEnd::Exited(Some(1)), None);
        quota.summary.quota_exhausted = true;
        let mut task = agent_task()?;
        settle(&mut task, &quota, 3)?;
        assert_eq!(task.failure.as_deref(), Some("the agent ran out of quota"));

        let mut refused = report(StreamEnd::Cancelled, None);
        refused.summary.refused = Some("paid key".to_owned());
        let mut task = agent_task()?;
        settle(&mut task, &refused, 3)?;
        assert_eq!(task.failure.as_deref(), Some("stopped by fleet: paid key"));
        Ok(())
    }

    #[test]
    fn settle_only_applies_to_tasks_with_the_agent() -> Result<(), Box<dyn std::error::Error>> {
        let mut task = agent_task()?;
        task.transition(TaskState::Preflight, 3)?;
        let before = task.clone();
        assert!(
            settle(
                &mut task,
                &report(StreamEnd::Exited(Some(0)), success("ok")),
                4
            )
            .is_err()
        );
        assert_eq!(task, before);
        Ok(())
    }

    #[test]
    fn tail_keeps_whole_lines_within_the_limit() {
        let mut tail = Tail::new(10);
        for line in ["aaaa\n", "bbbb\n", "cccc\n"] {
            tail.push(line.to_owned());
        }
        assert_eq!(tail.into_string(), "bbbb\ncccc\n");
    }

    #[test]
    fn a_line_longer_than_the_limit_keeps_its_end() {
        let mut tail = Tail::new(4);
        tail.push("ok\n".to_owned());
        tail.push("too long\n".to_owned());
        assert_eq!(tail.into_string(), "ong\n");
        let mut tail = Tail::new(3);
        tail.push("ñañ\n".to_owned());
        assert_eq!(tail.into_string(), "ñ\n");
    }
}
