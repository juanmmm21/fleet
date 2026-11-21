//! Argumentos de `fleetd`.

use std::net::SocketAddr;
use std::path::PathBuf;

pub const USAGE: &str =
    "usage: fleetd [--listen 127.0.0.1:7171] [--state-dir DIR] | --version | --help";
pub const DEFAULT_LISTEN: &str = "127.0.0.1:7171";

/// Qué pide la línea de órdenes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Version,
    Help,
    Serve(ServeArgs),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeArgs {
    pub listen: SocketAddr,
    pub state_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArgsError {
    #[error("unknown argument `{0}`")]
    Unknown(String),
    #[error("`{0}` needs a value")]
    MissingValue(&'static str),
    #[error("`{0}` is not an address like 127.0.0.1:7171")]
    BadAddress(String),
    #[error("fleetd only listens on loopback, not on {0}; share it with `tailscale serve`")]
    NotLoopback(SocketAddr),
    #[error("cannot find a state folder: set XDG_STATE_HOME or HOME, or pass --state-dir")]
    NoStateDir,
}

/// Lee los argumentos. `env` da las variables de entorno (inyectable para los tests).
pub fn parse<I, E>(args: I, env: E) -> Result<Command, ArgsError>
where
    I: IntoIterator<Item = String>,
    E: Fn(&str) -> Option<String>,
{
    let mut listen = None;
    let mut state_dir = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--version" | "-V" => return Ok(Command::Version),
            "--help" | "-h" => return Ok(Command::Help),
            "--listen" => listen = Some(args.next().ok_or(ArgsError::MissingValue("--listen"))?),
            "--state-dir" => {
                state_dir = Some(PathBuf::from(
                    args.next().ok_or(ArgsError::MissingValue("--state-dir"))?,
                ));
            }
            _ => return Err(ArgsError::Unknown(arg)),
        }
    }
    let raw = listen.unwrap_or_else(|| DEFAULT_LISTEN.to_owned());
    let listen: SocketAddr = raw.parse().map_err(|_| ArgsError::BadAddress(raw))?;
    if !listen.ip().is_loopback() {
        return Err(ArgsError::NotLoopback(listen));
    }
    let state_dir = match state_dir {
        Some(dir) => dir,
        None => default_state_dir(&env)?,
    };
    Ok(Command::Serve(ServeArgs { listen, state_dir }))
}

/// `$XDG_STATE_HOME/fleet` o `~/.local/state/fleet` (ADR 0001), en Linux y en macOS.
fn default_state_dir<E: Fn(&str) -> Option<String>>(env: &E) -> Result<PathBuf, ArgsError> {
    let non_empty = |key: &str| env(key).filter(|value| !value.is_empty());
    if let Some(state) = non_empty("XDG_STATE_HOME") {
        return Ok(PathBuf::from(state).join("fleet"));
    }
    let home = non_empty("HOME").ok_or(ArgsError::NoStateDir)?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("fleet"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|&a| a.to_owned()).collect()
    }

    fn home(key: &str) -> Option<String> {
        (key == "HOME").then(|| "/home/juan".to_owned())
    }

    #[test]
    fn defaults_to_loopback_and_the_xdg_state_folder() {
        assert_eq!(
            parse(args(&[]), home),
            Ok(Command::Serve(ServeArgs {
                listen: SocketAddr::from(([127, 0, 0, 1], 7171)),
                state_dir: PathBuf::from("/home/juan/.local/state/fleet"),
            }))
        );
        let xdg = |key: &str| (key == "XDG_STATE_HOME").then(|| "/state".to_owned());
        assert!(matches!(
            parse(args(&[]), xdg),
            Ok(Command::Serve(ServeArgs { state_dir, .. })) if state_dir.as_path() == std::path::Path::new("/state/fleet")
        ));
        assert_eq!(parse(args(&[]), |_| None), Err(ArgsError::NoStateDir));
    }

    #[test]
    fn takes_listen_and_state_dir() {
        assert_eq!(
            parse(
                args(&["--listen", "[::1]:9000", "--state-dir", "/tmp/fleet"]),
                |_| None
            ),
            Ok(Command::Serve(ServeArgs {
                listen: "[::1]:9000"
                    .parse()
                    .unwrap_or_else(|_| SocketAddr::from(([0; 4], 0))),
                state_dir: PathBuf::from("/tmp/fleet"),
            }))
        );
    }

    #[test]
    fn refuses_anything_but_loopback() {
        for addr in ["0.0.0.0:7171", "100.64.0.1:7171", "[::]:7171"] {
            assert!(
                matches!(
                    parse(args(&["--listen", addr]), home),
                    Err(ArgsError::NotLoopback(_))
                ),
                "{addr}"
            );
        }
        assert!(matches!(
            parse(args(&["--listen", "localhost:7171"]), home),
            Err(ArgsError::BadAddress(_))
        ));
    }

    #[test]
    fn version_help_and_mistakes() {
        assert_eq!(parse(args(&["--version"]), home), Ok(Command::Version));
        assert_eq!(parse(args(&["-h"]), home), Ok(Command::Help));
        assert_eq!(
            parse(args(&["--listen"]), home),
            Err(ArgsError::MissingValue("--listen"))
        );
        assert_eq!(
            parse(args(&["serve"]), home),
            Err(ArgsError::Unknown("serve".to_owned()))
        );
    }
}
