//! Argumentos de `fleet`.

use std::path::PathBuf;

use fleet_core::{AgentProvider, TaskId};

pub const USAGE: &str = "usage:
  fleet run <prompt...> [--repo DIR] [--agent claude|agy|ollama[:MODEL]] [--model M] [--base B] [--json]
  fleet ls [--json]
  fleet show <id> [--json]
  fleet log <id> [--follow]
  fleet diff <id>
  fleet answer <id> <text...>
  fleet merge|drop|cancel <id>
  fleet --version | --help

FLEET_URL points at fleetd (default http://127.0.0.1:7171).";

/// Lo que pide la línea de órdenes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Version,
    Help,
    Run(RunArgs),
    List { json: bool },
    Show { id: TaskId, json: bool },
    Log { id: TaskId, follow: bool },
    Diff { id: TaskId },
    Answer { id: TaskId, answer: String },
    Merge { id: TaskId },
    Drop { id: TaskId },
    Cancel { id: TaskId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunArgs {
    pub prompt: String,
    /// Tal cual la escribió quien llama; se hace absoluta al enviarla.
    pub repo: PathBuf,
    pub agent: AgentProvider,
    pub base: Option<String>,
    pub json: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArgsError {
    #[error("missing command")]
    MissingCommand,
    #[error("unknown command `{0}`")]
    UnknownCommand(String),
    #[error("unknown option `{0}`")]
    UnknownOption(String),
    #[error("`{0}` needs a value")]
    MissingValue(&'static str),
    #[error("missing {0}")]
    Missing(&'static str),
    #[error("unexpected argument `{0}`")]
    Unexpected(String),
    #[error("invalid task id `{0}`")]
    InvalidId(String),
    #[error("unknown agent `{0}`: use claude, agy or ollama:MODEL")]
    UnknownAgent(String),
    #[error("ollama needs a model: --agent ollama:MODEL or --model MODEL")]
    OllamaWithoutModel,
}

pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Command, ArgsError> {
    let mut args = args.into_iter();
    let command = args.next().ok_or(ArgsError::MissingCommand)?;
    let rest: Vec<String> = args.collect();
    match command.as_str() {
        "--version" | "-V" => Ok(Command::Version),
        "--help" | "-h" | "help" => Ok(Command::Help),
        "run" => parse_run(rest).map(Command::Run),
        "ls" => {
            let (json, positional) = take_flag(rest, "--json");
            no_positional(&positional)?;
            Ok(Command::List { json })
        }
        "show" => {
            let (json, positional) = take_flag(rest, "--json");
            Ok(Command::Show {
                id: single_id(&positional)?,
                json,
            })
        }
        "log" => {
            let (follow, positional) = take_flag(rest, "--follow");
            Ok(Command::Log {
                id: single_id(&positional)?,
                follow,
            })
        }
        "diff" => Ok(Command::Diff {
            id: single_id(&rest)?,
        }),
        "answer" => {
            let (id, text) = rest.split_first().ok_or(ArgsError::Missing("task id"))?;
            reject_options(text)?;
            let answer = text.join(" ");
            if answer.trim().is_empty() {
                return Err(ArgsError::Missing("answer"));
            }
            Ok(Command::Answer {
                id: parse_id(id)?,
                answer,
            })
        }
        "merge" => Ok(Command::Merge {
            id: single_id(&rest)?,
        }),
        "drop" => Ok(Command::Drop {
            id: single_id(&rest)?,
        }),
        "cancel" => Ok(Command::Cancel {
            id: single_id(&rest)?,
        }),
        other => Err(ArgsError::UnknownCommand(other.to_owned())),
    }
}

fn parse_run(rest: Vec<String>) -> Result<RunArgs, ArgsError> {
    let mut words = Vec::new();
    let mut repo = PathBuf::from(".");
    let mut agent = "claude".to_owned();
    let mut model = None;
    let mut base = None;
    let mut json = false;
    let mut args = rest.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--repo" => repo = PathBuf::from(value(&mut args, "--repo")?),
            "--agent" => agent = value(&mut args, "--agent")?,
            "--model" => model = Some(value(&mut args, "--model")?),
            "--base" => base = Some(value(&mut args, "--base")?),
            "--json" => json = true,
            "--" => words.extend(args.by_ref()),
            option if option.starts_with("--") => {
                return Err(ArgsError::UnknownOption(option.to_owned()));
            }
            _ => words.push(arg),
        }
    }
    let prompt = words.join(" ");
    if prompt.trim().is_empty() {
        return Err(ArgsError::Missing("prompt"));
    }
    Ok(RunArgs {
        prompt,
        repo,
        agent: parse_agent(&agent, model)?,
        base,
        json,
    })
}

fn value(args: &mut impl Iterator<Item = String>, name: &'static str) -> Result<String, ArgsError> {
    args.next().ok_or(ArgsError::MissingValue(name))
}

/// `claude`, `agy` u `ollama`, con el modelo en `--model` o tras dos puntos en Ollama.
fn parse_agent(raw: &str, model: Option<String>) -> Result<AgentProvider, ArgsError> {
    match raw.split_once(':') {
        Some(("ollama", inline)) if !inline.is_empty() => Ok(AgentProvider::Ollama {
            model: model.unwrap_or_else(|| inline.to_owned()),
        }),
        None if raw == "ollama" => model
            .map(|model| AgentProvider::Ollama { model })
            .ok_or(ArgsError::OllamaWithoutModel),
        None if raw == "claude" => Ok(AgentProvider::Claude { model }),
        None if raw == "agy" => Ok(AgentProvider::Agy { model }),
        _ => Err(ArgsError::UnknownAgent(raw.to_owned())),
    }
}

fn take_flag(args: Vec<String>, flag: &str) -> (bool, Vec<String>) {
    let present = args.iter().any(|arg| arg == flag);
    (
        present,
        args.into_iter().filter(|arg| arg != flag).collect(),
    )
}

fn reject_options(args: &[String]) -> Result<(), ArgsError> {
    match args.iter().find(|arg| arg.starts_with("--")) {
        Some(option) => Err(ArgsError::UnknownOption(option.clone())),
        None => Ok(()),
    }
}

fn no_positional(args: &[String]) -> Result<(), ArgsError> {
    reject_options(args)?;
    match args.first() {
        Some(extra) => Err(ArgsError::Unexpected(extra.clone())),
        None => Ok(()),
    }
}

fn single_id(args: &[String]) -> Result<TaskId, ArgsError> {
    reject_options(args)?;
    match args {
        [id] => parse_id(id),
        [] => Err(ArgsError::Missing("task id")),
        [_, extra, ..] => Err(ArgsError::Unexpected(extra.clone())),
    }
}

fn parse_id(raw: &str) -> Result<TaskId, ArgsError> {
    TaskId::parse(raw).map_err(|_| ArgsError::InvalidId(raw.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_list(list: &[&str]) -> Result<Command, ArgsError> {
        parse(list.iter().map(|&a| a.to_owned()))
    }

    fn id(raw: &str) -> TaskId {
        TaskId::parse(raw).unwrap_or_else(|_| TaskId::generate(std::time::SystemTime::now()))
    }

    #[test]
    fn run_joins_the_prompt_and_reads_its_options() {
        assert_eq!(
            parse_list(&[
                "run", "add", "tests", "--repo", "/src/app", "--base", "dev", "--json"
            ]),
            Ok(Command::Run(RunArgs {
                prompt: "add tests".to_owned(),
                repo: PathBuf::from("/src/app"),
                agent: AgentProvider::Claude { model: None },
                base: Some("dev".to_owned()),
                json: true,
            }))
        );
        assert!(matches!(
            parse_list(&["run", "--", "--not-an-option"]),
            Ok(Command::Run(RunArgs { prompt, .. })) if prompt == "--not-an-option"
        ));
        assert_eq!(parse_list(&["run"]), Err(ArgsError::Missing("prompt")));
        assert_eq!(
            parse_list(&["run", "x", "--fast"]),
            Err(ArgsError::UnknownOption("--fast".to_owned()))
        );
    }

    #[test]
    fn agents_and_models() {
        let agent = |list: &[&str]| match parse_list(list) {
            Ok(Command::Run(run)) => Ok(run.agent),
            Ok(other) => Err(format!("{other:?}")),
            Err(err) => Err(err.to_string()),
        };
        assert_eq!(
            agent(&["run", "x", "--agent", "agy", "--model", "gemini-3"]),
            Ok(AgentProvider::Agy {
                model: Some("gemini-3".to_owned())
            })
        );
        assert_eq!(
            agent(&["run", "x", "--agent", "ollama:qwen3-coder:30b"]),
            Ok(AgentProvider::Ollama {
                model: "qwen3-coder:30b".to_owned()
            })
        );
        assert_eq!(
            agent(&["run", "x", "--agent", "ollama"]),
            Err(ArgsError::OllamaWithoutModel.to_string())
        );
        assert_eq!(
            agent(&["run", "x", "--agent", "gpt"]),
            Err(ArgsError::UnknownAgent("gpt".to_owned()).to_string())
        );
    }

    #[test]
    fn task_commands_take_one_valid_id() {
        assert_eq!(
            parse_list(&["show", "abc-1", "--json"]),
            Ok(Command::Show {
                id: id("abc-1"),
                json: true
            })
        );
        assert_eq!(
            parse_list(&["log", "--follow", "abc-1"]),
            Ok(Command::Log {
                id: id("abc-1"),
                follow: true
            })
        );
        assert_eq!(
            parse_list(&["merge", "../x"]),
            Err(ArgsError::InvalidId("../x".to_owned()))
        );
        assert_eq!(parse_list(&["drop"]), Err(ArgsError::Missing("task id")));
        assert_eq!(
            parse_list(&["cancel", "a", "b"]),
            Err(ArgsError::Unexpected("b".to_owned()))
        );
        assert_eq!(
            parse_list(&["ls", "extra"]),
            Err(ArgsError::Unexpected("extra".to_owned()))
        );
    }

    #[test]
    fn answer_joins_the_text() {
        assert_eq!(
            parse_list(&["answer", "abc", "use", "sqlite"]),
            Ok(Command::Answer {
                id: id("abc"),
                answer: "use sqlite".to_owned()
            })
        );
        assert_eq!(
            parse_list(&["answer", "abc"]),
            Err(ArgsError::Missing("answer"))
        );
        assert_eq!(parse_list(&[]), Err(ArgsError::MissingCommand));
        assert_eq!(
            parse_list(&["deploy"]),
            Err(ArgsError::UnknownCommand("deploy".to_owned()))
        );
    }
}
