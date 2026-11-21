//! Procesos hijos con plazo: git, agentes y pruebas.
//!
//! Cada hijo arranca en su propio grupo de procesos para que, al cortarlo por plazo,
//! caigan también los nietos que haya lanzado (un `npm test` o un agente abren muchos).

use std::ffi::OsStr;
use std::io;
use std::process::Stdio;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

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
                kill_group(group).map_err(|source| ProcessError::Kill {
                    program: program.clone(),
                    source,
                })?;
            }
            // Recoge al hijo para no dejar un zombi; si ya había muerto con su grupo, mejor.
            if let Err(source) = child.kill().await {
                if source.kind() != io::ErrorKind::InvalidInput {
                    return Err(ProcessError::Io { program, source });
                }
            }
            Err(ProcessError::TimedOut {
                program,
                after: timeout,
            })
        }
    }
}

fn kill_group(group: i32) -> Result<(), Errno> {
    match killpg(Pid::from_raw(group), Signal::SIGKILL) {
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
