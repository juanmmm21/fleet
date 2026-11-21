//! Pre-flight: las pruebas del repo dentro del worktree de la tarea, antes de integrar.
//!
//! El comando sale de `fleet.toml` en la raíz del repo o, si no lo hay, de los ficheros que
//! delatan el stack. Sin ninguno es un error: un repo sin pruebas conocidas nunca da verde.
//!
//! Opcionalmente (`[preflight.act]`), después de las pruebas nativas se repite un job de
//! GitHub Actions con `act` (ver [`run_act`]).

use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::process::{self, OutputLine, ProcessError, Stream, StreamEnd, StreamOptions};
use crate::runner::{Tail, open_log};
use crate::worktree::{self, Worktree};

/// Fichero de configuración de fleet en la raíz del repo.
pub const CONFIG_FILE: &str = "fleet.toml";
/// Lo que escribe `npm init` en `scripts.test`: no es una prueba, es un recordatorio.
const NPM_PLACEHOLDER_TEST: &str = "no test specified";
const CHANNEL_CAPACITY: usize = 256;
/// Ficheros que delatan un proyecto de pytest.
const PYTHON_MARKERS: [&str; 3] = ["pyproject.toml", "pytest.ini", "setup.py"];

/// Cómo se pasa el pre-flight en un repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightPlan {
    /// Programa y argumentos, relativos a la raíz del worktree.
    pub command: Vec<String>,
    pub timeout: Duration,
    /// De dónde salió: `fleet.toml` o el fichero que delató el stack.
    pub source: String,
    /// Variables de entorno que se añaden al comando, con rutas ya absolutas al worktree.
    pub env: Vec<(String, OsString)>,
    /// Job de GitHub Actions que se repite con `act` si las pruebas nativas pasan.
    pub act: Option<ActPlan>,
}

/// Un job de GitHub Actions para `act`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActPlan {
    pub job: String,
    /// Opciones extra de `act` (`--container-architecture linux/amd64`, `-P ...`).
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightConfig {
    /// Plazo si `fleet.toml` no dice otro.
    pub default_timeout: Duration,
    pub grace: Duration,
    pub tail_bytes: usize,
    pub act_program: PathBuf,
    pub git_timeout: Duration,
}

impl Default for PreflightConfig {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_secs(30 * 60),
            grace: Duration::from_secs(10),
            tail_bytes: 16 * 1024,
            act_program: PathBuf::from("act"),
            git_timeout: Duration::from_secs(120),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PreflightError {
    #[error("no test command found in {0}: add a fleet.toml with [preflight] command = [...]")]
    NoCommand(PathBuf),
    #[error("invalid {path}: {message}")]
    Config { path: PathBuf, message: String },
    #[error("cannot read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Process(#[from] ProcessError),
    #[error("the pre-flight was cancelled")]
    Cancelled,
    #[error("cannot put {0} on PYTHONPATH: the path contains a path separator")]
    PythonPath(PathBuf),
    #[error("cannot prepare a clean clone for act: {0}")]
    ActClone(String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    preflight: Option<PreflightSection>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightSection {
    command: Option<Vec<String>>,
    timeout_secs: Option<u64>,
    act: Option<ActPlan>,
}

/// Decide qué comando de pruebas tiene el repo que hay en `root`.
pub fn plan(root: &Path, config: &PreflightConfig) -> Result<PreflightPlan, PreflightError> {
    let config_path = root.join(CONFIG_FILE);
    let invalid = |message: &str| PreflightError::Config {
        path: config_path.clone(),
        message: message.to_owned(),
    };
    let section = match read_optional(&config_path)? {
        Some(text) => {
            toml::from_str::<ConfigFile>(&text)
                .map_err(|error| invalid(error.message()))?
                .preflight
        }
        None => None,
    };
    let section = section.unwrap_or(PreflightSection {
        command: None,
        timeout_secs: None,
        act: None,
    });
    let timeout = match section.timeout_secs {
        Some(0) => return Err(invalid("preflight.timeout_secs must be positive")),
        Some(secs) => Duration::from_secs(secs),
        None => config.default_timeout,
    };
    if let Some(act) = &section.act {
        let bad_job = act.job.trim().is_empty() || act.job.starts_with('-');
        if bad_job {
            return Err(invalid("preflight.act.job must name a job"));
        }
    }
    let (source, command, env) = match section.command {
        Some(command) => {
            if command
                .first()
                .is_none_or(|program| program.trim().is_empty())
            {
                return Err(invalid("preflight.command must name a program"));
            }
            (CONFIG_FILE.to_owned(), command, Vec::new())
        }
        None => {
            let (marker, command) =
                detect(root)?.ok_or_else(|| PreflightError::NoCommand(root.to_path_buf()))?;
            let env = if PYTHON_MARKERS.contains(&marker) {
                python_code_env(root)?
            } else {
                Vec::new()
            };
            (marker.to_owned(), command, env)
        }
    };
    Ok(PreflightPlan {
        command,
        timeout,
        source,
        env,
        act: section.act,
    })
}

/// El stack por sus ficheros. El `Makefile` va primero: si tiene `test`, es la entrada que el
/// propio repo ha elegido.
fn detect(root: &Path) -> Result<Option<(&'static str, Vec<String>)>, PreflightError> {
    let owned = |args: &[&str]| args.iter().map(|&arg| arg.to_owned()).collect::<Vec<_>>();
    if let Some(makefile) = read_optional(&root.join("Makefile"))?
        && makefile.lines().any(|line| line.starts_with("test:"))
    {
        return Ok(Some(("Makefile", owned(&["make", "test"]))));
    }
    if root.join("Cargo.toml").is_file() {
        return Ok(Some(("Cargo.toml", owned(&["cargo", "test"]))));
    }
    if let Some(package) = read_optional(&root.join("package.json"))? {
        let json: serde_json::Value =
            serde_json::from_str(&package).map_err(|error| PreflightError::Config {
                path: root.join("package.json"),
                message: error.to_string(),
            })?;
        let test = json
            .get("scripts")
            .and_then(|scripts| scripts.get("test"))
            .and_then(serde_json::Value::as_str);
        if test.is_some_and(|script| !script.contains(NPM_PLACEHOLDER_TEST)) {
            return Ok(Some(("package.json", owned(&["npm", "test"]))));
        }
    }
    for marker in PYTHON_MARKERS {
        if root.join(marker).is_file() {
            // El `.venv` del checkout va enlazado en el worktree: su Python tiene las dependencias
            let python = if root.join(".venv/bin/python").exists() {
                ".venv/bin/python"
            } else {
                "python3"
            };
            return Ok(Some((marker, owned(&[python, "-m", "pytest"]))));
        }
    }
    if root.join("go.mod").is_file() {
        return Ok(Some(("go.mod", owned(&["go", "test", "./..."]))));
    }
    if root.join("Package.swift").is_file() {
        return Ok(Some(("Package.swift", owned(&["swift", "test"]))));
    }
    Ok(None)
}

/// `PYTHONPATH` y `MYPYPATH` con `<root>/src` (si existe) y `<root>` delante. El `.venv`
/// enlazado es el del checkout y, con el paquete instalado en modo editable, su `.pth` apunta
/// al código del checkout: sin esto las pruebas del worktree importarían el código de Juan y
/// podrían dar verde en falso.
fn python_code_env(root: &Path) -> Result<Vec<(String, OsString)>, PreflightError> {
    let src = root.join("src");
    let mut roots = Vec::new();
    if src.is_dir() {
        roots.push(src);
    }
    roots.push(root.to_path_buf());
    let joined =
        std::env::join_paths(&roots).map_err(|_| PreflightError::PythonPath(root.to_path_buf()))?;
    Ok(vec![
        ("PYTHONPATH".to_owned(), joined.clone()),
        ("MYPYPATH".to_owned(), joined),
    ])
}

fn read_optional(path: &Path) -> Result<Option<String>, PreflightError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(PreflightError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Pasa `plan` en `root` y escribe la salida en `log` (que se amplía, no se pisa).
pub async fn run<C>(
    root: &Path,
    plan: &PreflightPlan,
    config: &PreflightConfig,
    log: &Path,
    cancel: C,
) -> Result<PreflightResult, PreflightError>
where
    C: Future<Output = ()>,
{
    let log_error = |source| PreflightError::Io {
        path: log.to_path_buf(),
        source,
    };
    let (program, args) = plan
        .command
        .split_first()
        .ok_or_else(|| PreflightError::NoCommand(root.to_path_buf()))?;
    // Un programa con ruta relativa (`.venv/bin/python`) es relativo al worktree
    let program = if program.contains('/') && !program.starts_with('/') {
        root.join(program).into_os_string()
    } else {
        program.into()
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(root)
        // Muchas herramientas de pruebas dejan el modo interactivo o «watch» con esto
        .env("CI", "true")
        .envs(plan.env.iter().map(|(key, value)| (key, value)));
    worktree::isolate_git_env(&mut command);

    let mut file = open_log(log).await.map_err(log_error)?;
    let label = plan.command.join(" ");
    file.write_all(format!("== preflight `{label}` ({}) ==\n", plan.source).as_bytes())
        .await
        .map_err(log_error)?;
    file.flush().await.map_err(log_error)?;
    let (tx, mut rx) = mpsc::channel::<OutputLine>(CHANNEL_CAPACITY);
    let options = StreamOptions {
        timeout: plan.timeout,
        grace: config.grace,
    };
    let mut tail = Tail::new(config.tail_bytes);
    let record = async {
        let mut result = Ok(());
        while let Some(line) = rx.recv().await {
            let text = match line.stream {
                Stream::Stdout => format!("{}\n", line.line),
                Stream::Stderr => format!("[stderr] {}\n", line.line),
            };
            if result.is_ok() {
                result = file.write_all(text.as_bytes()).await;
            }
            // Se vuelca cuando no queda nada en cola: `GET /log` lee el fichero en directo
            if result.is_ok() && rx.is_empty() {
                result = file.flush().await;
            }
            tail.push(text);
        }
        result
    };
    let (streamed, recorded) =
        tokio::join!(process::stream(command, options, None, cancel, tx), record);
    let output = streamed?;
    recorded.map_err(log_error)?;
    let (exit_code, timed_out) = match output.end {
        StreamEnd::Exited(code) => (code, false),
        StreamEnd::TimedOut => (None, true),
        StreamEnd::Cancelled => return Err(PreflightError::Cancelled),
    };
    let result = PreflightResult {
        command: plan.command.clone(),
        exit_code,
        timed_out,
        duration_ms: u64::try_from(output.duration.as_millis()).unwrap_or(u64::MAX),
        output_tail: tail.into_string(),
    };
    file.write_all(format!("== {} ==\n", result.summary()).as_bytes())
        .await
        .map_err(log_error)?;
    file.flush().await.map_err(log_error)?;
    Ok(result)
}

/// Repite el job `act.job` de GitHub Actions con `act` sobre lo commiteado en la rama de la
/// tarea. `act` no entiende un worktree (ahí `.git` es un fichero, no una carpeta), así que se
/// lanza en un clon local de usar y tirar de la rama, junto al worktree. Así además prueba lo
/// mismo que se integraría: lo que está sin commitear no cuenta.
pub async fn run_act<C>(
    worktree: &Worktree,
    act: &ActPlan,
    timeout: Duration,
    config: &PreflightConfig,
    log: &Path,
    cancel: C,
) -> Result<PreflightResult, PreflightError>
where
    C: Future<Output = ()>,
{
    let clone = act_clone_path(&worktree.path)?;
    remove_clone(&clone).await?;
    let mut git = Command::new("git");
    git.arg("clone")
        .arg("--quiet")
        .arg("--no-hardlinks")
        .arg("--branch")
        .arg(&worktree.branch)
        .arg("--")
        .arg(&worktree.repo)
        .arg(&clone);
    worktree::isolate_git_env(&mut git);
    let cloned = process::run(git, config.git_timeout).await?;
    if !cloned.success() {
        return Err(PreflightError::ActClone(cloned.stderr.trim().to_owned()));
    }
    let mut command = vec![config.act_program.to_string_lossy().into_owned()];
    command.extend(["-j".to_owned(), act.job.clone()]);
    command.extend(act.args.iter().cloned());
    let plan = PreflightPlan {
        command,
        timeout,
        source: format!("act job `{}`", act.job),
        env: Vec::new(),
        act: None,
    };
    let result = run(&clone, &plan, config, log, cancel).await;
    // El clon no tiene nada que no esté en la rama: se borra pase lo que pase
    let removed = remove_clone(&clone).await;
    let result = result?;
    removed?;
    Ok(result)
}

fn act_clone_path(worktree: &Path) -> Result<PathBuf, PreflightError> {
    let name = worktree
        .file_name()
        .ok_or_else(|| PreflightError::ActClone(format!("{} has no name", worktree.display())))?;
    let mut name = name.to_os_string();
    name.push(".act");
    Ok(worktree.with_file_name(name))
}

async fn remove_clone(path: &Path) -> Result<(), PreflightError> {
    match tokio::fs::remove_dir_all(path).await {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(PreflightError::Io {
            path: path.to_path_buf(),
            source: error,
        }),
        _ => Ok(()),
    }
}

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

    type Files<'a> = &'a [(&'a str, &'a str)];

    fn repo(files: Files<'_>) -> Result<tempfile::TempDir, std::io::Error> {
        let dir = tempfile::tempdir()?;
        for (name, content) in files {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, content)?;
        }
        Ok(dir)
    }

    fn planned(files: Files<'_>) -> Result<PreflightPlan, Box<dyn std::error::Error>> {
        let dir = repo(files)?;
        Ok(plan(dir.path(), &PreflightConfig::default())?)
    }

    #[test]
    fn fleet_toml_wins_with_its_timeout() -> Result<(), Box<dyn std::error::Error>> {
        let plan = planned(&[
            ("Cargo.toml", "[package]"),
            (
                "fleet.toml",
                "[preflight]\ncommand = [\"cargo\", \"test\", \"--workspace\"]\ntimeout_secs = 90\n",
            ),
        ])?;
        assert_eq!(plan.command, ["cargo", "test", "--workspace"]);
        assert_eq!(plan.timeout, Duration::from_secs(90));
        assert_eq!(plan.source, "fleet.toml");
        Ok(())
    }

    #[test]
    fn act_can_go_with_detected_tests() -> Result<(), Box<dyn std::error::Error>> {
        let plan = planned(&[
            ("Cargo.toml", ""),
            (
                "fleet.toml",
                "[preflight]\ntimeout_secs = 60\n[preflight.act]\njob = \"test\"\nargs = [\"-P\", \"ubuntu-latest=node:20\"]\n",
            ),
        ])?;
        assert_eq!(plan.command, ["cargo", "test"]);
        assert_eq!(plan.source, "Cargo.toml");
        assert_eq!(plan.timeout, Duration::from_secs(60));
        assert_eq!(
            plan.act,
            Some(ActPlan {
                job: "test".to_owned(),
                args: vec!["-P".to_owned(), "ubuntu-latest=node:20".to_owned()],
            })
        );
        Ok(())
    }

    #[test]
    fn a_fleet_toml_without_preflight_falls_back_to_detection()
    -> Result<(), Box<dyn std::error::Error>> {
        let plan = planned(&[("fleet.toml", ""), ("go.mod", "module x")])?;
        assert_eq!(plan.command, ["go", "test", "./..."]);
        assert_eq!(plan.timeout, PreflightConfig::default().default_timeout);
        Ok(())
    }

    #[test]
    fn broken_fleet_toml_is_an_error_not_a_guess() -> Result<(), Box<dyn std::error::Error>> {
        for bad in [
            "[preflight]\ncommand = []\n",
            "[preflight]\ncommand = [\" \"]\n",
            "[preflight]\ncommand = [\"make\"]\ntimeout_secs = 0\n",
            "[preflight]\ncommand = [\"make\"]\ntypo = 1\n",
            "[preflight\n",
            "[preflight.act]\njob = \"--privileged\"\n",
            "[preflight.act]\nargs = []\n",
        ] {
            let dir = repo(&[("fleet.toml", bad), ("Cargo.toml", "")])?;
            let result = plan(dir.path(), &PreflightConfig::default());
            assert!(
                matches!(result, Err(PreflightError::Config { .. })),
                "{bad}"
            );
        }
        Ok(())
    }

    #[test]
    fn detects_each_stack() -> Result<(), Box<dyn std::error::Error>> {
        let cases: [(Files<'_>, &[&str]); 6] = [
            (
                &[
                    ("Makefile", "build:\n\ttrue\ntest:\n\ttrue\n"),
                    ("Cargo.toml", ""),
                ],
                &["make", "test"],
            ),
            (
                &[("Makefile", "build:\n"), ("Cargo.toml", "")],
                &["cargo", "test"],
            ),
            (
                &[("package.json", r#"{"scripts":{"test":"vitest run"}}"#)],
                &["npm", "test"],
            ),
            (&[("pyproject.toml", "")], &["python3", "-m", "pytest"]),
            (
                &[("pyproject.toml", ""), (".venv/bin/python", "")],
                &[".venv/bin/python", "-m", "pytest"],
            ),
            (&[("Package.swift", "")], &["swift", "test"]),
        ];
        for (files, command) in cases {
            assert_eq!(planned(files)?.command, command, "{files:?}");
        }
        Ok(())
    }

    #[test]
    fn python_puts_the_worktree_code_first() -> Result<(), Box<dyn std::error::Error>> {
        let dir = repo(&[("pyproject.toml", ""), ("src/app/__init__.py", "")])?;
        let plan = plan(dir.path(), &PreflightConfig::default())?;
        let expected = std::env::join_paths([dir.path().join("src"), dir.path().to_path_buf()])?;
        assert_eq!(
            plan.env,
            [
                ("PYTHONPATH".to_owned(), expected.clone()),
                ("MYPYPATH".to_owned(), expected),
            ]
        );

        let flat = repo(&[("setup.py", "")])?;
        let plan = super::plan(flat.path(), &PreflightConfig::default())?;
        let only_root = OsString::from(flat.path());
        assert_eq!(plan.env[0], ("PYTHONPATH".to_owned(), only_root));

        let other = planned(&[("Cargo.toml", "")])?;
        assert!(other.env.is_empty());
        let explicit = planned(&[
            ("pyproject.toml", ""),
            ("fleet.toml", "[preflight]\ncommand = [\"tox\"]\n"),
        ])?;
        assert!(explicit.env.is_empty());
        Ok(())
    }

    #[test]
    fn no_known_tests_is_an_error() -> Result<(), Box<dyn std::error::Error>> {
        let placeholder = r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#;
        for files in [
            &[("README.md", "hi")][..],
            &[("package.json", placeholder)][..],
            &[("package.json", "{}")][..],
        ] {
            let dir = repo(files)?;
            let result = plan(dir.path(), &PreflightConfig::default());
            assert!(
                matches!(result, Err(PreflightError::NoCommand(_))),
                "{files:?}"
            );
        }
        Ok(())
    }

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
