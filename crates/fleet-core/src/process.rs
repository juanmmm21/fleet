//! Procesos hijos con plazo: git, agentes y pruebas.
//!
//! Cada hijo arranca en su propio grupo de procesos para que, al cortarlo por plazo,
//! caigan también los nietos que haya lanzado (un `npm test` o un agente abren muchos).

use std::ffi::OsStr;
use std::future::Future;
use std::io;
use std::process::Stdio;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

/// Lo que dejó un proceso que terminó dentro de su plazo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutput {
    /// Código de salida; `None` si murió por una señal.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration: Duration,
}

impl ProcessOutput {
    #[must_use]
    pub fn success(&self) -> bool {
        self.exit_code == Some(0)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("could not start `{program}`: {source}")]
    Spawn { program: String, source: io::Error },
    #[error("i/o error while running `{program}`: {source}")]
    Io { program: String, source: io::Error },
    #[error("`{program}` timed out after {}s", .after.as_secs())]
    TimedOut { program: String, after: Duration },
    #[error("could not kill the process group of `{program}`: {source}")]
    Kill { program: String, source: Errno },
}

/// Lanza `command` sin entrada, recoge su salida y lo mata con todo su grupo si pasa `timeout`.
pub async fn run(mut command: Command, timeout: Duration) -> Result<ProcessOutput, ProcessError> {
    let program = program_name(command.as_std().get_program());
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);

    let started = Instant::now();
    let mut child = command.spawn().map_err(|source| ProcessError::Spawn {
        program: program.clone(),
        source,
    })?;
    // Con `process_group(0)` el id del grupo es el pid del hijo; se guarda antes de esperar
    // porque tokio lo olvida en cuanto el hijo termina.
    let group = child.id().and_then(|pid| i32::try_from(pid).ok());
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();

    let finished = tokio::time::timeout(timeout, async {
        tokio::join!(
            read_all(stdout.as_mut()),
            read_all(stderr.as_mut()),
            child.wait()
        )
    })
    .await;

    match finished {
        Ok((out, err, status)) => {
            let io_error = |source| ProcessError::Io {
                program: program.clone(),
                source,
            };
            let status = status.map_err(io_error)?;
            Ok(ProcessOutput {
                exit_code: status.code(),
                stdout: out.map_err(io_error)?,
                stderr: err.map_err(io_error)?,
                duration: started.elapsed(),
            })
        }
        Err(_elapsed) => {
            if let Some(group) = group {
                kill_group(group, Signal::SIGKILL).map_err(|source| ProcessError::Kill {
                    program: program.clone(),
                    source,
                })?;
            }
            // Recoge al hijo para no dejar un zombi; si ya había muerto con su grupo, mejor.
            if let Err(source) = child.kill().await
                && source.kind() != io::ErrorKind::InvalidInput
            {
                return Err(ProcessError::Io { program, source });
            }
            Err(ProcessError::TimedOut {
                program,
                after: timeout,
            })
        }
    }
}

/// De qué salida viene una línea.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// Una línea de la salida de un proceso, sin el salto final.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputLine {
    pub stream: Stream,
    pub line: String,
}

/// Cómo acabó un proceso lanzado con [`stream`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamEnd {
    /// Terminó solo; `None` si murió por una señal.
    Exited(Option<i32>),
    TimedOut,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamOptions {
    pub timeout: Duration,
    /// Margen entre SIGTERM y SIGKILL al cortarlo, para que el agente pueda cerrar lo suyo.
    pub grace: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamOutput {
    pub end: StreamEnd,
    pub duration: Duration,
}

/// Cuánto se espera a que se vacíen las tuberías una vez muerto el grupo. Solo se alcanza si
/// un nieto escapó del grupo (`setsid`) y sigue con la tubería abierta.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Lanza `command` sin entrada y manda cada línea de su salida a `lines` mientras corre.
/// Se corta al vencer el plazo o al completarse `cancel`. Al acabar, por la razón que sea,
/// se mata lo que quede de su grupo: un agente no deja procesos sueltos.
pub async fn stream<C>(
    mut command: Command,
    options: StreamOptions,
    cancel: C,
    lines: mpsc::Sender<OutputLine>,
) -> Result<StreamOutput, ProcessError>
where
    C: Future<Output = ()>,
{
    let program = program_name(command.as_std().get_program());
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);

    let started = Instant::now();
    let mut child = command.spawn().map_err(|source| ProcessError::Spawn {
        program: program.clone(),
        source,
    })?;
    let group = child.id().and_then(|pid| i32::try_from(pid).ok());
    let readers = [
        child
            .stdout
            .take()
            .map(|out| tokio::spawn(forward_lines(out, Stream::Stdout, lines.clone()))),
        child
            .stderr
            .take()
            .map(|err| tokio::spawn(forward_lines(err, Stream::Stderr, lines.clone()))),
    ];
    drop(lines);

    tokio::pin!(cancel);
    let io_error = |source| ProcessError::Io {
        program: program.clone(),
        source,
    };
    let end = tokio::select! {
        status = child.wait() => StreamEnd::Exited(status.map_err(io_error)?.code()),
        () = tokio::time::sleep(options.timeout) => StreamEnd::TimedOut,
        () = &mut cancel => StreamEnd::Cancelled,
    };
    if end == StreamEnd::TimedOut || end == StreamEnd::Cancelled {
        terminate(&mut child, group, options.grace)
            .await
            .map_err(|err| err.with_program(&program))?;
    }
    if let Some(group) = group {
        kill_group(group, Signal::SIGKILL).map_err(|source| ProcessError::Kill {
            program: program.clone(),
            source,
        })?;
    }
    for reader in readers.into_iter().flatten() {
        let abort = reader.abort_handle();
        match tokio::time::timeout(DRAIN_TIMEOUT, reader).await {
            Ok(Ok(result)) => result.map_err(io_error)?,
            // La tarea lectora no hace nada que pueda entrar en pánico salvo un fallo de tokio
            Ok(Err(join)) => return Err(io_error(io::Error::other(join))),
            Err(_elapsed) => abort.abort(),
        }
    }
    Ok(StreamOutput {
        end,
        duration: started.elapsed(),
    })
}

/// Error interno de [`terminate`], al que se le pone el programa al subir.
enum TerminateError {
    Kill(Errno),
    Io(io::Error),
}

impl TerminateError {
    fn with_program(self, program: &str) -> ProcessError {
        let program = program.to_owned();
        match self {
            Self::Kill(source) => ProcessError::Kill { program, source },
            Self::Io(source) => ProcessError::Io { program, source },
        }
    }
}

/// SIGTERM al grupo, espera `grace` y, si el hijo sigue, SIGKILL. Siempre recoge al hijo.
async fn terminate(
    child: &mut Child,
    group: Option<i32>,
    grace: Duration,
) -> Result<(), TerminateError> {
    if let Some(group) = group {
        kill_group(group, Signal::SIGTERM).map_err(TerminateError::Kill)?;
        if let Ok(waited) = tokio::time::timeout(grace, child.wait()).await {
            waited.map_err(TerminateError::Io)?;
            return Ok(());
        }
        kill_group(group, Signal::SIGKILL).map_err(TerminateError::Kill)?;
    }
    match child.kill().await {
        Err(source) if source.kind() != io::ErrorKind::InvalidInput => {
            Err(TerminateError::Io(source))
        }
        _ => Ok(()),
    }
}

/// Lee línea a línea y las reenvía. Si nadie escucha ya, sigue leyendo igualmente para que
/// el proceso no se quede bloqueado con la tubería llena.
async fn forward_lines<R: AsyncRead + Unpin>(
    reader: R,
    stream: Stream,
    lines: mpsc::Sender<OutputLine>,
) -> io::Result<()> {
    let mut reader = BufReader::new(reader);
    let mut buffer = Vec::new();
    let mut listening = true;
    loop {
        buffer.clear();
        if reader.read_until(b'\n', &mut buffer).await? == 0 {
            return Ok(());
        }
        if listening {
            let line = String::from_utf8_lossy(&buffer)
                .trim_end_matches(['\n', '\r'])
                .to_owned();
            listening = lines.send(OutputLine { stream, line }).await.is_ok();
        }
    }
}

fn kill_group(group: i32, signal: Signal) -> Result<(), Errno> {
    match killpg(Pid::from_raw(group), signal) {
        // ESRCH: el grupo ya no existe, que es justo lo que se quería
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(errno) => Err(errno),
    }
}

async fn read_all<R: AsyncRead + Unpin>(reader: Option<&mut R>) -> io::Result<String> {
    let mut bytes = Vec::new();
    if let Some(reader) = reader {
        reader.read_to_end(&mut bytes).await?;
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn program_name(program: &OsStr) -> String {
    program.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        command
    }

    #[tokio::test]
    async fn collects_output_and_exit_code() -> Result<(), ProcessError> {
        let output = run(
            sh("echo out; echo err >&2; exit 3"),
            Duration::from_secs(10),
        )
        .await?;
        assert_eq!(output.exit_code, Some(3));
        assert!(!output.success());
        assert_eq!(output.stdout, "out\n");
        assert_eq!(output.stderr, "err\n");
        Ok(())
    }

    #[tokio::test]
    async fn a_missing_program_is_a_spawn_error() {
        let result = run(
            Command::new("fleet-no-such-program"),
            Duration::from_secs(5),
        )
        .await;
        assert!(matches!(result, Err(ProcessError::Spawn { .. })));
    }

    #[tokio::test]
    async fn timeout_kills_the_whole_process_group() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let pid_file = dir.path().join("grandchild.pid");
        let script = format!("sleep 30 & echo $! > '{}'; wait", pid_file.display());

        let started = Instant::now();
        let result = run(sh(&script), Duration::from_millis(500)).await;
        assert!(matches!(result, Err(ProcessError::TimedOut { .. })));
        assert!(started.elapsed() < Duration::from_secs(10));

        let grandchild: i32 = std::fs::read_to_string(&pid_file)?.trim().parse()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while is_running(grandchild)? {
            assert!(
                Instant::now() < deadline,
                "grandchild {grandchild} survived"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(())
    }

    fn options(timeout_ms: u64, grace_ms: u64) -> StreamOptions {
        StreamOptions {
            timeout: Duration::from_millis(timeout_ms),
            grace: Duration::from_millis(grace_ms),
        }
    }

    async fn collect(mut rx: mpsc::Receiver<OutputLine>) -> Vec<OutputLine> {
        let mut lines = Vec::new();
        while let Some(line) = rx.recv().await {
            lines.push(line);
        }
        lines
    }

    #[tokio::test]
    async fn streams_each_line_while_it_runs() -> Result<(), ProcessError> {
        let (tx, rx) = mpsc::channel(4);
        let collected = tokio::spawn(collect(rx));
        let output = stream(
            sh("printf 'one\\ntwo\\r\\n'; echo oops >&2; printf 'no newline'; exit 2"),
            options(10_000, 1_000),
            std::future::pending(),
            tx,
        )
        .await?;
        assert_eq!(output.end, StreamEnd::Exited(Some(2)));
        let lines = collected.await.map_err(|e| ProcessError::Io {
            program: "join".to_owned(),
            source: io::Error::other(e),
        })?;
        let stdout: Vec<&str> = lines
            .iter()
            .filter(|l| l.stream == Stream::Stdout)
            .map(|l| l.line.as_str())
            .collect();
        assert_eq!(stdout, ["one", "two", "no newline"]);
        assert!(lines.contains(&OutputLine {
            stream: Stream::Stderr,
            line: "oops".to_owned()
        }));
        Ok(())
    }

    #[tokio::test]
    async fn cancelling_sends_sigterm_first() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let marker = dir.path().join("terminated");
        // `exec sleep` dejaría sin trap al shell; el sleep en segundo plano mantiene el trap vivo
        let script = format!(
            "trap 'echo bye > \"{}\"; exit 0' TERM; echo ready; sleep 30 & wait",
            marker.display()
        );
        let (tx, mut rx) = mpsc::channel(4);
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        let run = tokio::spawn(stream(
            sh(&script),
            options(30_000, 5_000),
            async move {
                // Si el emisor se suelta sin avisar, también se cancela
                let _ = cancel_rx.await;
            },
            tx,
        ));
        let first = rx.recv().await.ok_or("no output")?;
        assert_eq!(first.line, "ready");
        cancel_tx.send(()).map_err(|()| "run already finished")?;
        let output = run.await??;
        assert_eq!(output.end, StreamEnd::Cancelled);
        assert!(output.duration < Duration::from_secs(5), "{output:?}");
        assert_eq!(std::fs::read_to_string(&marker)?, "bye\n");
        Ok(())
    }

    #[tokio::test]
    async fn a_process_that_ignores_sigterm_is_killed_after_the_grace()
    -> Result<(), Box<dyn std::error::Error>> {
        let (tx, rx) = mpsc::channel(4);
        drop(rx);
        let started = Instant::now();
        let output = stream(
            sh("trap '' TERM; while true; do sleep 0.1; done"),
            options(300, 300),
            std::future::pending(),
            tx,
        )
        .await?;
        assert_eq!(output.end, StreamEnd::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
        Ok(())
    }

    #[tokio::test]
    async fn nobody_listening_does_not_block_a_chatty_process() -> Result<(), ProcessError> {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let output = stream(
            sh("i=0; while [ $i -lt 20000 ]; do echo line $i; i=$((i+1)); done"),
            options(20_000, 1_000),
            std::future::pending(),
            tx,
        )
        .await?;
        assert_eq!(output.end, StreamEnd::Exited(Some(0)));
        Ok(())
    }

    #[tokio::test]
    async fn leftover_background_processes_die_with_the_group()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let pid_file = dir.path().join("orphan.pid");
        let script = format!(
            "sleep 30 > /dev/null 2>&1 & echo $! > '{}'",
            pid_file.display()
        );
        let (tx, rx) = mpsc::channel(4);
        drop(rx);
        let output = stream(
            sh(&script),
            options(10_000, 1_000),
            std::future::pending(),
            tx,
        )
        .await?;
        assert_eq!(output.end, StreamEnd::Exited(Some(0)));
        let orphan: i32 = std::fs::read_to_string(&pid_file)?.trim().parse()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while is_running(orphan)? {
            assert!(Instant::now() < deadline, "orphan {orphan} survived");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(())
    }

    /// Un zombi aún responde a `kill -0` hasta que su nuevo padre lo recoge, y eso depende de
    /// quién haga de init: se mira el estado con `ps`, que existe en Linux y en macOS.
    fn is_running(pid: i32) -> std::io::Result<bool> {
        let output = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()?;
        let stat = String::from_utf8_lossy(&output.stdout);
        let stat = stat.trim();
        Ok(!stat.is_empty() && !stat.starts_with('Z'))
    }
}
