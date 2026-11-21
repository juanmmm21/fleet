//! Agentes que pueden trabajar en una tarea: CLIs de suscripción u Ollama local, nunca APIs de pago.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::transcript::Transcript;

/// Con cualquiera de estas variables Claude Code cobraría por API en vez de usar la suscripción.
const PAID_ANTHROPIC_ENV: [&str; 2] = ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"];
/// Con estas, `agy` podría ir por un proyecto de Google Cloud o una clave de API de pago.
const PAID_GOOGLE_ENV: [&str; 5] = [
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GOOGLE_GENAI_USE_VERTEXAI",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_CLOUD_PROJECT",
];
/// Claude Code añade un trailer de co-autoría a sus commits: lo que se integra va a nombre de
/// quien lanza la tarea, nada más.
const CLAUDE_SETTINGS: &str = r#"{"attribution":{"commit":"","pr":""}}"#;
/// Lo que se le dice al agente antes del encargo en la primera vuelta. Va en el propio encargo
/// y no como instrucciones de sistema porque `agy` no tiene esa opción, y así todos igual.
pub const PREAMBLE: &str = "You are running unattended inside a dedicated git worktree created \
by fleet; nobody is watching this session.
- Work only inside this directory. Commit your work on the current branch with clear messages. \
Do not push and do not switch branches: fleet tests and integrates the branch itself.
- If you need a decision that only the person who assigned the task can make, stop and make your \
final message start with \"QUESTION:\" followed by one short, self-contained question. You will \
be resumed with the answer.

Task:
";

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
    #[error("`{0}` runs in Ollama's cloud; only local models are allowed")]
    CloudModel(String),
    #[error("ollama must be reached on this machine, not at `{0}`")]
    RemoteOllama(String),
    #[error("the prompt is empty")]
    EmptyPrompt,
    #[error("invalid session id `{0}`")]
    InvalidSession(String),
}

/// Dónde están los programas de los agentes en esta máquina.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPrograms {
    pub claude: PathBuf,
    pub agy: PathBuf,
    /// API de Ollama compatible con la de Anthropic, en esta misma máquina.
    pub ollama_url: String,
}

impl Default for AgentPrograms {
    fn default() -> Self {
        Self {
            claude: PathBuf::from("claude"),
            agy: PathBuf::from("agy"),
            ollama_url: "http://127.0.0.1:11434".to_owned(),
        }
    }
}

/// Un agente listo para lanzarse: la orden, el encargo (que va por stdin) y el lector de su
/// salida.
#[derive(Debug)]
pub struct Invocation {
    pub command: Command,
    pub stdin: Vec<u8>,
    pub transcript: Transcript,
}

impl AgentProvider {
    /// Nombre del proveedor, el mismo que la etiqueta `kind` de la API.
    #[must_use]
    pub const fn name(&self) -> &'static str {
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
            // Los `:cloud` de Ollama se ejecutan en sus servidores y gastan su cuota de pago
            (Self::Ollama { model }, _) if model.ends_with("cloud") => {
                Err(AgentError::CloudModel(model.clone()))
            }
            _ => Ok(()),
        }
    }

    /// La orden que lanza este agente para trabajar sin nadie delante sobre `prompt`. Con
    /// `resume`, retoma esa sesión y `prompt` es la respuesta a lo que preguntó; sin él, el
    /// encargo va detrás de [`PREAMBLE`].
    ///
    /// Todos trabajan sin pedir permiso: van en un worktree aislado, nada se integra sin pasar
    /// el pre-flight y quien lanza la tarea ya ha decidido darle ese margen. Ollama va a través
    /// de Claude Code apuntado a la API local de Ollama, que es la forma de que un modelo local
    /// edite ficheros y ejecute órdenes como los demás.
    pub fn invocation(
        &self,
        programs: &AgentPrograms,
        prompt: &str,
        resume: Option<&str>,
    ) -> Result<Invocation, AgentError> {
        self.validate()?;
        if prompt.trim().is_empty() {
            return Err(AgentError::EmptyPrompt);
        }
        if let Some(session) = resume
            && !is_valid_session(session)
        {
            return Err(AgentError::InvalidSession(session.to_owned()));
        }
        let command = match self {
            Self::Claude { model } => {
                let mut command = claude_command(&programs.claude, model.as_deref(), resume);
                for var in PAID_ANTHROPIC_ENV {
                    command.env_remove(var);
                }
                command
            }
            Self::Ollama { model } => {
                if !is_loopback_url(&programs.ollama_url) {
                    return Err(AgentError::RemoteOllama(programs.ollama_url.clone()));
                }
                let mut command = claude_command(&programs.claude, Some(model), resume);
                command
                    .env("ANTHROPIC_BASE_URL", &programs.ollama_url)
                    // Ollama lo exige pero no lo comprueba; vacía, la clave no puede cobrar nada
                    .env("ANTHROPIC_AUTH_TOKEN", "ollama")
                    .env("ANTHROPIC_API_KEY", "");
                command
            }
            Self::Agy { model } => {
                let mut command = Command::new(&programs.agy);
                // Sin `-p`: el encargo llega por stdin. `--disable-slash-commands` para que un
                // encargo que empiece por «/» no se tome como una orden del propio CLI
                command.args([
                    "--output-format",
                    "stream-json",
                    "--disable-slash-commands",
                    "--dangerously-skip-permissions",
                ]);
                if let Some(model) = model {
                    command.args(["--model", model]);
                }
                if let Some(session) = resume {
                    command.args(["--conversation", session]);
                }
                for var in PAID_GOOGLE_ENV {
                    command.env_remove(var);
                }
                command
            }
        };
        let stdin = match resume {
            Some(_) => prompt.to_owned(),
            None => format!("{PREAMBLE}{prompt}"),
        };
        Ok(Invocation {
            command,
            stdin: stdin.into_bytes(),
            transcript: Transcript::for_provider(self),
        })
    }
}

fn claude_command(program: &Path, model: Option<&str>, resume: Option<&str>) -> Command {
    let mut command = Command::new(program);
    command.args([
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--permission-mode",
        "bypassPermissions",
        "--settings",
        CLAUDE_SETTINGS,
    ]);
    if let Some(model) = model {
        command.args(["--model", model]);
    }
    if let Some(session) = resume {
        command.args(["--resume", session]);
    }
    command
}

/// Las sesiones de las CLIs son UUID o ids parecidos: va como argumento y no puede ser una opción.
fn is_valid_session(session: &str) -> bool {
    !session.is_empty()
        && session.len() <= 128
        && !session.starts_with('-')
        && session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// Solo `http` a `127.0.0.1`, `localhost` o `[::1]`, con puerto o sin él.
fn is_loopback_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or_default();
    let host = match authority.strip_prefix("[::1]") {
        Some(port) => return port.is_empty() || is_port(port),
        None => authority,
    };
    let (host, port) = host.split_once(':').unwrap_or((host, ""));
    matches!(host, "127.0.0.1" | "localhost") && (port.is_empty() || is_port(&format!(":{port}")))
}

fn is_port(suffix: &str) -> bool {
    suffix
        .strip_prefix(':')
        .is_some_and(|port| port.parse::<u16>().is_ok())
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
            Some(model) => write!(f, "{} ({model})", self.name()),
            None => f.write_str(self.name()),
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
    fn rejects_cloud_models_and_remote_ollama() {
        let cloud = AgentProvider::Ollama {
            model: "kimi-k2.7-code:cloud".to_owned(),
        };
        assert_eq!(
            cloud.validate(),
            Err(AgentError::CloudModel("kimi-k2.7-code:cloud".to_owned()))
        );
        let local = AgentProvider::Ollama {
            model: "qwen3:8b".to_owned(),
        };
        let remote = AgentPrograms {
            ollama_url: "https://ollama.com".to_owned(),
            ..AgentPrograms::default()
        };
        assert!(matches!(
            local.invocation(&remote, "do it", None),
            Err(AgentError::RemoteOllama(_))
        ));
        assert!(
            local
                .invocation(&AgentPrograms::default(), "do it", None)
                .is_ok()
        );
    }

    #[test]
    fn only_loopback_urls_count_as_local() {
        for good in [
            "http://127.0.0.1:11434",
            "http://localhost:11434/",
            "http://[::1]:11434",
            "http://127.0.0.1",
        ] {
            assert!(is_loopback_url(good), "{good}");
        }
        for bad in [
            "https://127.0.0.1:11434",
            "http://127.0.0.1.evil.com",
            "http://localhost:99999",
            "http://10.0.0.2:11434",
            "http://localhost@evil.com",
            "127.0.0.1:11434",
        ] {
            assert!(!is_loopback_url(bad), "{bad}");
        }
    }

    fn args(invocation: &Invocation) -> Vec<String> {
        invocation
            .command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn env(invocation: &Invocation, key: &str) -> Option<Option<String>> {
        invocation
            .command
            .as_std()
            .get_envs()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
    }

    #[test]
    fn claude_runs_headless_without_paid_keys() -> Result<(), AgentError> {
        let invocation = AgentProvider::Claude {
            model: Some("opus".to_owned()),
        }
        .invocation(&AgentPrograms::default(), "add tests", None)?;
        let args = args(&invocation);
        assert_eq!(invocation.command.as_std().get_program(), "claude");
        assert!(args.starts_with(&["-p".to_owned()]));
        assert!(
            args.windows(2)
                .any(|w| w == ["--permission-mode", "bypassPermissions"])
        );
        assert!(args.windows(2).any(|w| w == ["--model", "opus"]));
        assert!(args.contains(&CLAUDE_SETTINGS.to_owned()));
        // El encargo nunca va en argv
        assert!(!args.iter().any(|a| a.contains("add tests")));
        assert_eq!(
            invocation.stdin,
            format!("{PREAMBLE}add tests").into_bytes()
        );
        assert_eq!(env(&invocation, "ANTHROPIC_API_KEY"), Some(None));
        assert_eq!(env(&invocation, "ANTHROPIC_AUTH_TOKEN"), Some(None));
        Ok(())
    }

    #[test]
    fn ollama_goes_through_claude_code_to_the_local_api() -> Result<(), AgentError> {
        let invocation = AgentProvider::Ollama {
            model: "qwen3:8b".to_owned(),
        }
        .invocation(&AgentPrograms::default(), "fix it", None)?;
        assert_eq!(invocation.command.as_std().get_program(), "claude");
        assert!(
            args(&invocation)
                .windows(2)
                .any(|w| w == ["--model", "qwen3:8b"])
        );
        assert_eq!(
            env(&invocation, "ANTHROPIC_BASE_URL"),
            Some(Some("http://127.0.0.1:11434".to_owned()))
        );
        assert_eq!(
            env(&invocation, "ANTHROPIC_API_KEY"),
            Some(Some(String::new()))
        );
        Ok(())
    }

    #[test]
    fn agy_reads_the_prompt_from_stdin_without_paid_keys() -> Result<(), AgentError> {
        let invocation = AgentProvider::Agy { model: None }.invocation(
            &AgentPrograms::default(),
            "/help me",
            None,
        )?;
        let args = args(&invocation);
        assert!(!args.contains(&"-p".to_owned()));
        assert!(args.contains(&"--disable-slash-commands".to_owned()));
        assert!(!args.contains(&"--model".to_owned()));
        assert!(invocation.stdin.ends_with(b"\n/help me"));
        assert_eq!(env(&invocation, "GEMINI_API_KEY"), Some(None));
        Ok(())
    }

    #[test]
    fn resuming_passes_the_session_and_only_the_answer() -> Result<(), AgentError> {
        let programs = AgentPrograms::default();
        let claude = AgentProvider::Claude { model: None }.invocation(
            &programs,
            "use fleet-core",
            Some("0b6c-uuid_1"),
        )?;
        assert!(
            args(&claude)
                .windows(2)
                .any(|w| w == ["--resume", "0b6c-uuid_1"])
        );
        assert_eq!(claude.stdin, b"use fleet-core");
        let agy = AgentProvider::Agy { model: None }.invocation(&programs, "yes", Some("c-1"))?;
        assert!(
            args(&agy)
                .windows(2)
                .any(|w| w == ["--conversation", "c-1"])
        );
        for bad in ["--help", "a b", "", "x/../y"] {
            assert_eq!(
                AgentProvider::Agy { model: None }
                    .invocation(&programs, "yes", Some(bad))
                    .err(),
                Some(AgentError::InvalidSession(bad.to_owned()))
            );
        }
        Ok(())
    }

    #[test]
    fn an_empty_prompt_is_rejected() {
        assert!(matches!(
            AgentProvider::Agy { model: None }.invocation(&AgentPrograms::default(), " \n", None),
            Err(AgentError::EmptyPrompt)
        ));
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
