//! Resultado de pasar las pruebas del repo en el worktree de una tarea.

use serde::{Deserialize, Serialize};

/// Lo que salió de una ejecución del pre-flight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreflightResult {
    /// Programa y argumentos que se lanzaron.
    pub command: Vec<String>,
    /// Código de salida; `None` si el proceso murió por una señal o se cortó por plazo.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
    /// Final de la salida combinada, que es donde suelen estar los fallos.
    pub output_tail: String,
}

impl PreflightResult {
    /// Solo cuenta como verde una salida con código 0 dentro del plazo.
    #[must_use]
    pub fn passed(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }

    /// Una línea que explica el resultado, para el motivo de fallo y los avisos.
    #[must_use]
    pub fn summary(&self) -> String {
        let command = self.command.join(" ");
        if self.timed_out {
            return format!(
                "`{command}` timed out after {}s",
                self.duration_ms.div_ceil(1000)
            );
        }
        match self.exit_code {
            Some(0) => format!("`{command}` passed"),
            Some(code) => format!("`{command}` exited with code {code}"),
            None => format!("`{command}` was killed by a signal"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(exit_code: Option<i32>, timed_out: bool) -> PreflightResult {
        PreflightResult {
            command: vec!["npm".to_owned(), "test".to_owned()],
            exit_code,
            timed_out,
            duration_ms: 90_500,
            output_tail: String::new(),
        }
    }

    #[test]
    fn only_a_clean_exit_in_time_passes() {
        assert!(result(Some(0), false).passed());
        assert!(!result(Some(0), true).passed());
        assert!(!result(Some(1), false).passed());
        assert!(!result(None, false).passed());
    }

    #[test]
    fn summaries_say_what_happened() {
        assert_eq!(result(Some(0), false).summary(), "`npm test` passed");
        assert_eq!(
            result(Some(1), false).summary(),
            "`npm test` exited with code 1"
        );
        assert_eq!(
            result(None, true).summary(),
            "`npm test` timed out after 91s"
        );
        assert_eq!(
            result(None, false).summary(),
            "`npm test` was killed by a signal"
        );
    }
}
