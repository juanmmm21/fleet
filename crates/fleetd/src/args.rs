//! Argumentos de `fleetd`.

use std::net::SocketAddr;
use std::path::PathBuf;

pub const USAGE: &str = "usage: fleetd [--listen 127.0.0.1:7171] [--state-dir DIR] \
                         [--repos-root DIR] [--allow-host NAME]... [--allow-login LOGIN]... \
                         | --version | --help";
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
    /// Dónde se buscan los repos pedidos por nombre; por defecto `~/Desarrollo`.
    pub repos_root: PathBuf,
    /// Nombres de máquina aceptados en `Host` además de loopback y `*.ts.net`.
    pub allow_hosts: Vec<String>,
    /// Logins de Tailscale que pueden entrar por la tailnet además del dueño de la máquina.
    pub allow_logins: Vec<String>,
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
    #[error("`{0}` is not a host name (no scheme, port or path)")]
    BadHost(String),
    #[error("cannot find a state folder: set XDG_STATE_HOME or HOME, or pass --state-dir")]
    NoStateDir,
    #[error("cannot find the repos folder: set HOME or pass --repos-root")]
    NoReposRoot,
    #[error("`{0}` is not a Tailscale login")]
    BadLogin(String),
    #[error("`{0}` must be an absolute path")]
    NotAbsolute(String),
}

/// Lee los argumentos. `env` da las variables de entorno (inyectable para los tests).
pub fn parse<I, E>(args: I, env: E) -> Result<Command, ArgsError>
where
    I: IntoIterator<Item = String>,
    E: Fn(&str) -> Option<String>,
{
    let mut listen = None;
    let mut state_dir = None;
    let mut allow_hosts = Vec::new();
    let mut repos_root = None;
    let mut allow_logins = Vec::new();
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
            "--repos-root" => {
                let raw = args.next().ok_or(ArgsError::MissingValue("--repos-root"))?;
                if !raw.starts_with('/') {
                    return Err(ArgsError::NotAbsolute(raw));
                }
                repos_root = Some(PathBuf::from(raw));
            }
            "--allow-login" => {
                let login = args
                    .next()
                    .ok_or(ArgsError::MissingValue("--allow-login"))?;
                if login.is_empty() || login.chars().any(char::is_whitespace) {
                    return Err(ArgsError::BadLogin(login));
                }
                allow_logins.push(login);
            }
            "--allow-host" => {
                let host = args.next().ok_or(ArgsError::MissingValue("--allow-host"))?;
                allow_hosts.push(host_name(host)?);
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
    let repos_root = match repos_root {
        Some(root) => root,
        None => env("HOME")
            .filter(|home| !home.is_empty())
            .map(|home| PathBuf::from(home).join("Desarrollo"))
            .ok_or(ArgsError::NoReposRoot)?,
    };
    Ok(Command::Serve(ServeArgs {
        listen,
        state_dir,
        repos_root,
        allow_hosts,
        allow_logins,
    }))
}

/// Un nombre de máquina tal cual, en minúsculas: así se compara con `Host` sin el puerto.
fn host_name(raw: String) -> Result<String, ArgsError> {
    let valid = !raw.is_empty()
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
    if valid {
        Ok(raw.to_ascii_lowercase())
    } else {
        Err(ArgsError::BadHost(raw))
    }
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
                repos_root: PathBuf::from("/home/juan/Desarrollo"),
                allow_hosts: Vec::new(),
                allow_logins: Vec::new(),
            }))
        );
        let xdg = |key: &str| match key {
            "XDG_STATE_HOME" => Some("/state".to_owned()),
            _ => home(key),
        };
        assert!(matches!(
            parse(args(&[]), xdg),
            Ok(Command::Serve(ServeArgs { state_dir, .. })) if state_dir.as_path() == std::path::Path::new("/state/fleet")
        ));
        assert_eq!(parse(args(&[]), |_| None), Err(ArgsError::NoStateDir));
        assert_eq!(
            parse(args(&["--state-dir", "/s"]), |_| None),
            Err(ArgsError::NoReposRoot)
        );
    }

    #[test]
    fn takes_listen_and_state_dir() {
        assert_eq!(
            parse(
                args(&[
                    "--listen",
                    "[::1]:9000",
                    "--state-dir",
                    "/tmp/fleet",
                    "--repos-root",
                    "/srv/repos",
                    "--allow-host",
                    "PC",
                    "--allow-host",
                    "mac.local",
                    "--allow-login",
                    "friend@example.com"
                ]),
                |_| None
            ),
            Ok(Command::Serve(ServeArgs {
                listen: "[::1]:9000"
                    .parse()
                    .unwrap_or_else(|_| SocketAddr::from(([0; 4], 0))),
                state_dir: PathBuf::from("/tmp/fleet"),
                repos_root: PathBuf::from("/srv/repos"),
                allow_hosts: vec!["pc".to_owned(), "mac.local".to_owned()],
                allow_logins: vec!["friend@example.com".to_owned()],
            }))
        );
        assert_eq!(
            parse(args(&["--repos-root", "repos"]), home),
            Err(ArgsError::NotAbsolute("repos".to_owned()))
        );
        assert_eq!(
            parse(args(&["--allow-login", "a b"]), home),
            Err(ArgsError::BadLogin("a b".to_owned()))
        );
        for bad in ["http://pc", "pc:7171", ""] {
            assert_eq!(
                parse(args(&["--allow-host", bad]), home),
                Err(ArgsError::BadHost(bad.to_owned()))
            );
        }
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
