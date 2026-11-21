//! Preguntar a Tailscale por la tailnet (ADR 0002): `tailscale status --json` habla con el
//! socket local de `tailscaled`, sin API ni claves.

use std::path::PathBuf;
use std::time::Duration;

use fleet_core::{ProcessError, TailnetError, TailnetStatus, process};
use tokio::process::Command;

/// Lo que tarda de sobra en responder un `tailscaled` sano.
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum TailnetQueryError {
    #[error(transparent)]
    Process(#[from] ProcessError),
    #[error("`tailscale status` failed: {0}")]
    Failed(String),
    #[error(transparent)]
    Parse(#[from] TailnetError),
}

/// Cómo se llama a `tailscale` en esta máquina.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tailscale {
    pub program: PathBuf,
}

impl Default for Tailscale {
    fn default() -> Self {
        Self {
            program: PathBuf::from("tailscale"),
        }
    }
}

impl Tailscale {
    pub async fn status(&self) -> Result<TailnetStatus, TailnetQueryError> {
        let mut command = Command::new(&self.program);
        command.args(["status", "--json"]);
        let output = process::run(command, STATUS_TIMEOUT).await?;
        // Con Tailscale parado `status --json` aún imprime el JSON (y sale con error): se
        // analiza igual para dar el motivo de verdad
        match TailnetStatus::parse(output.stdout.as_bytes()) {
            Ok(status) => Ok(status),
            Err(err) if output.success() => Err(err.into()),
            Err(TailnetError::NotRunning(state)) => Err(TailnetError::NotRunning(state).into()),
            Err(_) => Err(TailnetQueryError::Failed(output.stderr.trim().to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn fake(dir: &std::path::Path, script: &str) -> Result<Tailscale, std::io::Error> {
        let program = dir.join("tailscale");
        std::fs::write(&program, format!("#!/bin/sh\n{script}\n"))?;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))?;
        Ok(Tailscale { program })
    }

    #[tokio::test]
    async fn reads_the_status_and_explains_failures() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let running = fake(
            dir.path(),
            r#"echo '{"BackendState":"Running","Self":{"HostName":"pc","DNSName":"pc.t.ts.net.","OS":"linux","Online":true,"UserID":1},"User":{"1":{"LoginName":"owner@example.com"}}}'"#,
        )?;
        let status = running.status().await?;
        assert_eq!(status.this.login.as_deref(), Some("owner@example.com"));

        let stopped = fake(
            dir.path(),
            r#"echo '{"BackendState":"Stopped","Self":{}}'; exit 1"#,
        )?;
        assert!(matches!(
            stopped.status().await,
            Err(TailnetQueryError::Parse(TailnetError::NotRunning(_)))
        ));

        let broken = fake(
            dir.path(),
            "echo 'failed to connect to local tailscaled' >&2; exit 1",
        )?;
        assert!(matches!(
            broken.status().await,
            Err(TailnetQueryError::Failed(message)) if message.contains("tailscaled")
        ));

        let missing = Tailscale {
            program: dir.path().join("nowhere"),
        };
        assert!(matches!(
            missing.status().await,
            Err(TailnetQueryError::Process(_))
        ));
        Ok(())
    }
}
