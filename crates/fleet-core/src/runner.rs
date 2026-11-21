//! Lanzar el agente de una tarea en su worktree: con plazo y cancelación, su salida a un
//! registro en disco (y en directo a quien escuche) y, al acabar, el diff de lo que hizo.

use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::process::{self, OutputLine, ProcessError, Stream, StreamEnd, StreamOptions};
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

/// Lanza `command` (el agente ya configurado) dentro del worktree.
pub async fn run_agent<C>(
    worktree_config: &WorktreeConfig,
    worktree: &Worktree,
    mut command: Command,
    config: &RunConfig,
    log: &Path,
    cancel: C,
    live: Option<mpsc::Sender<OutputLine>>,
) -> Result<AgentReport, RunError>
where
    C: Future<Output = ()>,
{
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
    let (streamed, recorded) = tokio::join!(
        process::stream(command, options, cancel, tx),
        record(rx, file, live, config.tail_bytes)
    );
    let (mut file, output_tail, write_result) = recorded;
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
    })
}

async fn open_log(path: &Path) -> std::io::Result<BufWriter<File>> {
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

/// Escribe cada línea en el registro, guarda el final y la reenvía en directo. Un fallo al
/// escribir no corta el bucle: hay que seguir vaciando el canal para no frenar al agente.
async fn record(
    mut rx: mpsc::Receiver<OutputLine>,
    mut file: BufWriter<File>,
    mut live: Option<mpsc::Sender<OutputLine>>,
    tail_bytes: usize,
) -> (BufWriter<File>, String, std::io::Result<()>) {
    let mut tail = Tail::new(tail_bytes);
    let mut write_result = Ok(());
    while let Some(line) = rx.recv().await {
        let text = match line.stream {
            Stream::Stdout => format!("{}\n", line.line),
            Stream::Stderr => format!("[stderr] {}\n", line.line),
        };
        if write_result.is_ok() {
            write_result = file.write_all(text.as_bytes()).await;
        }
        tail.push(text);
        if let Some(sender) = &live
            && sender.send(line).await.is_err()
        {
            live = None;
        }
    }
    (file, tail.into_string(), write_result)
}

/// Las últimas líneas que caben en un número de bytes.
struct Tail {
    limit: usize,
    size: usize,
    lines: VecDeque<String>,
}

impl Tail {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            size: 0,
            lines: VecDeque::new(),
        }
    }

    fn push(&mut self, line: String) {
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

    fn into_string(self) -> String {
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
    use super::*;

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
