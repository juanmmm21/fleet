//! Agentes que pueden trabajar en una tarea: CLIs de suscripción u Ollama local, nunca APIs de pago.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Qué agente trabaja en una tarea y con qué modelo.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentProvider {
    /// Claude Code (`claude -p`) con la suscripción de Juan; sin modelo, el que tenga por defecto.
    Claude {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// Antigravity (`agy`).
    Agy {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// Un modelo de Ollama en esta máquina; aquí el modelo es obligatorio.
    Ollama { model: String },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AgentError {
    #[error("ollama needs a model name")]
    MissingModel,
    #[error("invalid model name `{0}`")]
    InvalidModel(String),
}

impl AgentProvider {
    /// Programa que se lanza para este agente.
    #[must_use]
    pub const fn program(&self) -> &'static str {
        match self {
            Self::Claude { .. } => "claude",
            Self::Agy { .. } => "agy",
            Self::Ollama { .. } => "ollama",
        }
    }

    #[must_use]
    pub fn model(&self) -> Option<&str> {
        match self {
            Self::Claude { model } | Self::Agy { model } => model.as_deref(),
            Self::Ollama { model } => Some(model),
        }
    }

    /// Comprueba el modelo antes de aceptar la tarea: va como argumento a un proceso y no
    /// puede traer espacios ni empezar por `-`, que el programa leería como una opción.
    pub fn validate(&self) -> Result<(), AgentError> {
        match (self, self.model()) {
            (Self::Ollama { .. }, Some("")) => Err(AgentError::MissingModel),
            (_, Some(model)) if !is_valid_model(model) => {
                Err(AgentError::InvalidModel(model.to_owned()))
            }
            _ => Ok(()),
        }
    }
}

fn is_valid_model(model: &str) -> bool {
    !model.is_empty()
        && !model.starts_with('-')
        && model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/'))
}

impl fmt::Display for AgentProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.model() {
            Some(model) => write!(f, "{} ({model})", self.program()),
            None => f.write_str(self.program()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_with_a_kind_tag() -> Result<(), serde_json::Error> {
        let claude = AgentProvider::Claude { model: None };
        assert_eq!(serde_json::to_string(&claude)?, r#"{"kind":"claude"}"#);
        let ollama: AgentProvider =
            serde_json::from_str(r#"{"kind":"ollama","model":"qwen3-coder:30b"}"#)?;
        assert_eq!(
            ollama,
            AgentProvider::Ollama {
                model: "qwen3-coder:30b".to_owned()
            }
        );
        assert!(serde_json::from_str::<AgentProvider>(r#"{"kind":"ollama"}"#).is_err());
        assert!(serde_json::from_str::<AgentProvider>(r#"{"kind":"openai"}"#).is_err());
        Ok(())
    }

    #[test]
    fn validates_model_names() {
        assert_eq!(AgentProvider::Agy { model: None }.validate(), Ok(()));
        assert_eq!(
            AgentProvider::Claude {
                model: Some("opus".to_owned())
            }
            .validate(),
            Ok(())
        );
        assert_eq!(
            AgentProvider::Ollama {
                model: String::new()
            }
            .validate(),
            Err(AgentError::MissingModel)
        );
        for bad in ["--dangerously", "a b", "x;rm"] {
            assert_eq!(
                AgentProvider::Claude {
                    model: Some(bad.to_owned())
                }
                .validate(),
                Err(AgentError::InvalidModel(bad.to_owned()))
            );
        }
    }

    #[test]
    fn displays_program_and_model() {
        assert_eq!(AgentProvider::Claude { model: None }.to_string(), "claude");
        assert_eq!(
            AgentProvider::Ollama {
                model: "llama3".to_owned()
            }
            .to_string(),
            "ollama (llama3)"
        );
    }
}
