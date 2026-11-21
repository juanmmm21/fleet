//! Lectura de la salida `stream-json` de los agentes: el resultado final, la sesión, los turnos
//! y si se acabó el cupo. Los tipos de línea que no importan se ignoran a propósito: las CLIs
//! añaden tipos nuevos a menudo y eso no debe romper a fleet.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::AgentProvider;

/// Lo que Claude Code pone en `apiKeySource` cuando usa la suscripción.
const SUBSCRIPTION_KEY_SOURCE: &str = "none";
const AGY_SUCCESS: &str = "SUCCESS";
/// Sin una grabación real del cupo agotado de `agy`: el texto que usan los productos de Google.
const QUOTA_WORDS: [&str; 8] = [
    "quota",
    "rate limit",
    "rate-limit",
    "resource_exhausted",
    " 429",
    "exhausted",
    "usage limit",
    "out of credits",
];

/// Cómo acabó el agente según él mismo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AgentOutcome {
    Success { text: String },
    Error { message: String },
}

/// Resumen de una vuelta del agente sacado de su salida.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSummary {
    /// Sesión o conversación, para retomarla con la respuesta a una pregunta.
    pub session_id: Option<String>,
    /// `None` si el agente acabó sin escribir su línea de resultado.
    pub outcome: Option<AgentOutcome>,
    pub turns: Option<u32>,
    pub quota_exhausted: bool,
    /// Por qué fleet cortó al agente, si lo cortó.
    pub refused: Option<String>,
}

/// Con qué empieza el último mensaje del agente cuando pregunta algo (ver `agent::PREAMBLE`).
const QUESTION_PREFIX: &str = "QUESTION:";

impl AgentSummary {
    /// La pregunta del agente, si acabó con una.
    #[must_use]
    pub fn question(&self) -> Option<&str> {
        let Some(AgentOutcome::Success { text }) = &self.outcome else {
            return None;
        };
        let question = text.trim_start().strip_prefix(QUESTION_PREFIX)?.trim();
        (!question.is_empty()).then_some(question)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    /// `allow_api_key`: con Ollama, Claude Code usa una clave de mentira contra la API local.
    Claude {
        allow_api_key: bool,
    },
    Agy,
}

/// Va leyendo la salida de un agente línea a línea.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    format: Format,
    summary: AgentSummary,
}

impl Transcript {
    #[must_use]
    pub fn for_provider(provider: &AgentProvider) -> Self {
        let format = match provider {
            AgentProvider::Claude { .. } => Format::Claude {
                allow_api_key: false,
            },
            AgentProvider::Ollama { .. } => Format::Claude {
                allow_api_key: true,
            },
            AgentProvider::Agy { .. } => Format::Agy,
        };
        Self {
            format,
            summary: AgentSummary::default(),
        }
    }

    /// Procesa una línea de stdout. Lo que no es JSON (avisos sueltos de la CLI) se ignora.
    pub fn feed(&mut self, line: &str) {
        let Ok(Value::Object(data)) = serde_json::from_str::<Value>(line) else {
            return;
        };
        match self.format {
            Format::Claude { allow_api_key } => self.feed_claude(&data, allow_api_key),
            Format::Agy => self.feed_agy(&data),
        }
    }

    /// Si hay que cortar al agente ya, el motivo.
    #[must_use]
    pub fn must_stop(&self) -> Option<&str> {
        self.summary.refused.as_deref()
    }

    #[must_use]
    pub fn finish(self) -> AgentSummary {
        self.summary
    }

    fn feed_claude(&mut self, data: &serde_json::Map<String, Value>, allow_api_key: bool) {
        if let Some(session) = non_empty_str(data.get("session_id")) {
            self.summary.session_id = Some(session.to_owned());
        }
        match data.get("type").and_then(Value::as_str) {
            Some("system") if data.get("subtype").and_then(Value::as_str) == Some("init") => {
                let source = data.get("apiKeySource").and_then(Value::as_str);
                if !allow_api_key
                    && let Some(source) = source
                    && source != SUBSCRIPTION_KEY_SOURCE
                {
                    self.summary.refused = Some(format!(
                        "claude is authenticated with {source}, which is billed per use; \
                         log in with the subscription instead"
                    ));
                }
            }
            Some("rate_limit_event") => {
                let status = data
                    .get("rate_limit_info")
                    .and_then(|info| info.get("status"))
                    .and_then(Value::as_str);
                if status == Some("rejected") {
                    self.summary.quota_exhausted = true;
                }
            }
            Some("result") => {
                let text = data
                    .get("result")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                let success = data.get("subtype").and_then(Value::as_str) == Some("success")
                    && data.get("is_error").and_then(Value::as_bool) != Some(true);
                self.summary.turns = data
                    .get("num_turns")
                    .and_then(Value::as_u64)
                    .and_then(|turns| u32::try_from(turns).ok());
                self.summary.outcome = Some(if success {
                    AgentOutcome::Success { text }
                } else {
                    if is_quota_text(&text) {
                        self.summary.quota_exhausted = true;
                    }
                    let message = if text.is_empty() {
                        data.get("subtype")
                            .and_then(Value::as_str)
                            .unwrap_or("error")
                            .to_owned()
                    } else {
                        text
                    };
                    AgentOutcome::Error { message }
                });
            }
            _ => {}
        }
    }

    fn feed_agy(&mut self, data: &serde_json::Map<String, Value>) {
        let Some(event) = data.get("event").and_then(Value::as_str) else {
            return;
        };
        let payload = data.get(event);
        let conversation = non_empty_str(data.get("conversation_id"))
            .or_else(|| non_empty_str(payload.and_then(|p| p.get("conversation_id"))));
        if let Some(conversation) = conversation {
            self.summary.session_id = Some(conversation.to_owned());
        }
        if event != "result" {
            return;
        }
        let field = |name: &str| {
            payload
                .and_then(|p| p.get(name))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_owned()
        };
        let status = field("status");
        self.summary.outcome = Some(if status == AGY_SUCCESS {
            AgentOutcome::Success {
                text: field("response"),
            }
        } else {
            let error = field("error");
            if is_quota_text(&error) {
                self.summary.quota_exhausted = true;
            }
            AgentOutcome::Error {
                message: match (status.is_empty(), error.is_empty()) {
                    (_, false) => error,
                    (false, true) => status,
                    (true, true) => "ERROR".to_owned(),
                },
            }
        });
    }
}

fn non_empty_str(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn is_quota_text(text: &str) -> bool {
    let lower = format!(" {}", text.to_lowercase());
    QUOTA_WORDS.iter().any(|word| lower.contains(word))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude() -> Transcript {
        Transcript::for_provider(&AgentProvider::Claude { model: None })
    }

    #[test]
    fn claude_success_with_session_and_turns() {
        let mut t = claude();
        t.feed(r#"{"type":"system","subtype":"init","session_id":"s-1","apiKeySource":"none"}"#);
        t.feed(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#);
        t.feed("not json at all");
        t.feed(r#"{"type":"something_new","x":1}"#);
        t.feed(
            r#"{"type":"result","subtype":"success","is_error":false,"result":" Done. \n","num_turns":7,"session_id":"s-1"}"#,
        );
        assert_eq!(t.must_stop(), None);
        assert_eq!(
            t.finish(),
            AgentSummary {
                session_id: Some("s-1".to_owned()),
                outcome: Some(AgentOutcome::Success {
                    text: "Done.".to_owned()
                }),
                turns: Some(7),
                quota_exhausted: false,
                refused: None,
            }
        );
    }

    #[test]
    fn claude_with_a_paid_key_must_stop() {
        let mut t = claude();
        t.feed(r#"{"type":"system","subtype":"init","apiKeySource":"ANTHROPIC_API_KEY"}"#);
        assert!(
            t.must_stop()
                .is_some_and(|r| r.contains("ANTHROPIC_API_KEY"))
        );
    }

    #[test]
    fn ollama_through_claude_may_use_its_placeholder_key() {
        let mut t = Transcript::for_provider(&AgentProvider::Ollama {
            model: "qwen3:8b".to_owned(),
        });
        t.feed(r#"{"type":"system","subtype":"init","apiKeySource":"ANTHROPIC_AUTH_TOKEN"}"#);
        assert_eq!(t.must_stop(), None);
    }

    #[test]
    fn claude_quota_rejection_and_error_result() {
        let mut t = claude();
        t.feed(r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#);
        assert!(!t.clone().finish().quota_exhausted);
        t.feed(r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected"}}"#);
        t.feed(r#"{"type":"result","subtype":"error_during_execution","is_error":true}"#);
        let summary = t.finish();
        assert!(summary.quota_exhausted);
        assert_eq!(
            summary.outcome,
            Some(AgentOutcome::Error {
                message: "error_during_execution".to_owned()
            })
        );
    }

    #[test]
    fn agy_success_and_quota_error() {
        let mut t = Transcript::for_provider(&AgentProvider::Agy { model: None });
        t.feed(r#"{"event":"step_update","step_update":{"conversation_id":"c-9"}}"#);
        t.feed(r#"{"event":"result","result":{"status":"SUCCESS","response":"ok\n"}}"#);
        let summary = t.finish();
        assert_eq!(summary.session_id.as_deref(), Some("c-9"));
        assert_eq!(
            summary.outcome,
            Some(AgentOutcome::Success {
                text: "ok".to_owned()
            })
        );

        let mut t = Transcript::for_provider(&AgentProvider::Agy { model: None });
        t.feed(
            r#"{"event":"result","conversation_id":"c-1","result":{"status":"ERROR","error":"RESOURCE_EXHAUSTED (code 429)"}}"#,
        );
        let summary = t.finish();
        assert!(summary.quota_exhausted);
        assert_eq!(summary.session_id.as_deref(), Some("c-1"));
    }

    #[test]
    fn a_final_message_starting_with_question_is_a_question() {
        let summary = |text: &str| AgentSummary {
            outcome: Some(AgentOutcome::Success {
                text: text.to_owned(),
            }),
            ..AgentSummary::default()
        };
        assert_eq!(
            summary("QUESTION: which crate?").question(),
            Some("which crate?")
        );
        assert_eq!(
            summary("  QUESTION:\n  sqlite or files? ").question(),
            Some("sqlite or files?")
        );
        assert_eq!(summary("QUESTION:   ").question(), None);
        assert_eq!(summary("Done. QUESTION: none").question(), None);
        let error = AgentSummary {
            outcome: Some(AgentOutcome::Error {
                message: "QUESTION: x".to_owned(),
            }),
            ..AgentSummary::default()
        };
        assert_eq!(error.question(), None);
    }

    #[test]
    fn no_result_line_leaves_the_outcome_empty() {
        let mut t = claude();
        t.feed(r#"{"type":"assistant","message":{"content":[]}}"#);
        assert_eq!(t.finish().outcome, None);
    }
}
