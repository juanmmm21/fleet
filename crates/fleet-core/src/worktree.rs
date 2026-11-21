//! Ciclo de vida del worktree de una tarea: crearlo desde `origin`, prepararle el entorno
//! y borrarlo sin perder trabajo.
//!
//! La rama `fleet/<id>` sale de `origin/<base>` recién traído y no del checkout de Juan,
//! que puede ir por detrás o tener trabajo a medias (mismo criterio que el ADR 0023 de Palo).

use std::ffi::OsStr;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::process::{self, ProcessError, ProcessOutput};
use crate::task::TaskId;

/// Ficheros que se copian del checkout: cada tarea puede tocar el suyo sin afectar a Juan.
pub const DEFAULT_COPIED: &[&str] = &[".env"];

/// Lo que se enlaza en vez de copiarse: dependencias que pesan cientos de MB y los ficheros
/// de contexto de los agentes, que se gitignoran y deben ser los mismos que ve Juan.
pub const DEFAULT_LINKED: &[&str] = &[
    ".venv",
    "node_modules",
    "target",
    "AGENTS.md",
    "CLAUDE.md",
    "PROGRESS.md",
];

/// Cómo se preparan los worktrees en esta máquina.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeConfig {
    /// Carpeta donde viven todos los worktrees, fuera de cualquier repo.
    pub root: PathBuf,
    /// Plazo de cada orden de git (un `fetch` por una red lenta es lo que más tarda).
    pub git_timeout: Duration,
    /// Rutas relativas al checkout que se copian si existen y git no las sigue.
    pub copied: Vec<PathBuf>,
    /// Rutas relativas al checkout que se enlazan si existen y git no las sigue.
    pub linked: Vec<PathBuf>,
}

impl WorktreeConfig {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            git_timeout: Duration::from_secs(120),
            copied: DEFAULT_COPIED.iter().map(PathBuf::from).collect(),
            linked: DEFAULT_LINKED.iter().map(PathBuf::from).collect(),
        }
    }

    /// Carpeta del worktree de una tarea: `<root>/<nombre del repo>-<id>`.
    pub fn path_for(&self, repo: &Path, id: &TaskId) -> Result<PathBuf, WorktreeError> {
        let name = repo
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| WorktreeError::InvalidRepo(repo.to_path_buf()))?;
        Ok(self.root.join(format!("{name}-{}", id.as_str())))
    }
}

/// Un worktree creado para una tarea. Se guarda tal cual en el estado del demonio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    /// Checkout del que sale (y con el que comparte objetos).
    pub repo: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    pub base: String,
    /// Commit de `origin/<base>` del que salió la rama: el punto contra el que se mira su diff.
    pub base_commit: String,
    /// Lo que se copió y enlazó desde el checkout, relativo a la raíz del worktree.
    pub copied: Vec<PathBuf>,
    pub linked: Vec<PathBuf>,
}

/// Qué hacer con el trabajo que haya en el worktree al borrarlo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removal {
    /// Solo si no hay cambios sin commitear y la rama ya está en `origin/<base>`.
    IfIntegrated,
    /// Descarte a propósito: se pierden la rama y los cambios.
    Discard,
}

#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error("`{0}` is not a usable repository path")]
    InvalidRepo(PathBuf),
    #[error("`{0}` is not a valid base branch name")]
    InvalidBase(String),
    #[error("`{0}` must be a relative path inside the repository")]
    InvalidEntry(PathBuf),
    #[error("the worktree folder `{0}` already exists")]
    AlreadyExists(PathBuf),
    #[error("`git {args}` failed in `{cwd}`: {stderr}")]
    Git {
        cwd: PathBuf,
        args: String,
        stderr: String,
    },
    #[error(transparent)]
    Process(#[from] ProcessError),
    #[error("{action} `{path}`: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    #[error("the worktree `{0}` has uncommitted changes")]
    Dirty(PathBuf),
    #[error("branch `{branch}` is not integrated into `origin/{base}`")]
    NotIntegrated { branch: String, base: String },
}

/// Crea el worktree de la tarea `id` en una rama nueva `fleet/<id>` desde `origin/<base>`.
pub async fn create(
    config: &WorktreeConfig,
    repo: &Path,
    id: &TaskId,
    base: &str,
) -> Result<Worktree, WorktreeError> {
    if !repo.is_absolute() || !repo.is_dir() {
        return Err(WorktreeError::InvalidRepo(repo.to_path_buf()));
    }
    for entry in config.copied.iter().chain(&config.linked) {
        check_entry(entry)?;
    }
    check_base(config, repo, base).await?;
    let path = config.path_for(repo, id)?;
    if fs_exists(&path)? {
        return Err(WorktreeError::AlreadyExists(path));
    }

    git(config, repo, &["fetch", "--quiet", "origin", base]).await?;
    let remote_base = format!("origin/{base}");
    let base_commit = git(
        config,
        repo,
        &[
            "rev-parse",
            "--verify",
            &format!("{remote_base}^{{commit}}"),
        ],
    )
    .await?
    .stdout
    .trim()
    .to_owned();

    std::fs::create_dir_all(&config.root).map_err(|source| WorktreeError::Io {
        action: "creating",
        path: config.root.clone(),
        source,
    })?;
    let branch = id.branch();
    let mut add = git_command(repo);
    // Desde el commit y con --no-track: la rama no sigue a origin/<base>, así un `git push`
    // suelto del agente no puede caer en la rama principal.
    add.args(["worktree", "add", "--quiet", "--no-track", "-b", &branch])
        .arg(&path)
        .arg(&base_commit);
    git_checked(config, repo, add, "worktree add").await?;

    let mut worktree = Worktree {
        repo: repo.to_path_buf(),
        path,
        branch,
        base: base.to_owned(),
        base_commit,
        copied: Vec::new(),
        linked: Vec::new(),
    };
    if let Err(error) = prepare_environment(config, &mut worktree).await {
        // El worktree aún no tiene trabajo de nadie: se deshace para no dejarlo a medias
        if let Err(cleanup) = remove(config, &worktree, Removal::Discard).await {
            return Err(WorktreeError::Io {
                action: "cleaning up a half-prepared worktree at",
                path: worktree.path.clone(),
                source: io::Error::other(format!("{error}; cleanup also failed: {cleanup}")),
            });
        }
        return Err(error);
    }
    Ok(worktree)
}

/// Borra el worktree y su rama. Con [`Removal::IfIntegrated`] se niega si se perdería algo.
pub async fn remove(
    config: &WorktreeConfig,
    worktree: &Worktree,
    removal: Removal,
) -> Result<(), WorktreeError> {
    let present = fs_exists(&worktree.path)?;
    if removal == Removal::IfIntegrated {
        if present {
            let status = git(
                config,
                &worktree.path,
                &["status", "--porcelain", "--untracked-files=all"],
            )
            .await?;
            if !status.stdout.trim().is_empty() {
                return Err(WorktreeError::Dirty(worktree.path.clone()));
            }
        }
        if !is_integrated(config, worktree).await? {
            return Err(WorktreeError::NotIntegrated {
                branch: worktree.branch.clone(),
                base: worktree.base.clone(),
            });
        }
    }

    if present {
        let mut command = git_command(&worktree.repo);
        command.args(["worktree", "remove"]);
        if removal == Removal::Discard {
            command.arg("--force");
        }
        command.arg(&worktree.path);
        git_checked(config, &worktree.repo, command, "worktree remove").await?;
    } else {
        // Alguien borró la carpeta a mano: git aún la tiene registrada
        git(config, &worktree.repo, &["worktree", "prune"]).await?;
    }

    // -D y no -d: `-d` compara con el HEAD del checkout de Juan, no con origin/<base>,
    // y la integración ya se ha comprobado arriba contra lo que de verdad importa.
    if branch_exists(config, worktree).await? {
        git(
            config,
            &worktree.repo,
            &["branch", "-D", "--", &worktree.branch],
        )
        .await?;
    }
    Ok(())
}

/// Si la punta de la rama ya está en `origin/<base>` recién traído.
pub async fn is_integrated(
    config: &WorktreeConfig,
    worktree: &Worktree,
) -> Result<bool, WorktreeError> {
    git(
        config,
        &worktree.repo,
        &["fetch", "--quiet", "origin", &worktree.base],
    )
    .await?;
    let remote_base = format!("origin/{}", worktree.base);
    let mut command = git_command(&worktree.repo);
    command.args([
        "merge-base",
        "--is-ancestor",
        &worktree.branch,
        &remote_base,
    ]);
    let output = process::run(command, config.git_timeout).await?;
    match output.exit_code {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(git_failure(
            &worktree.repo,
            "merge-base --is-ancestor",
            &output,
        )),
    }
}

async fn branch_exists(
    config: &WorktreeConfig,
    worktree: &Worktree,
) -> Result<bool, WorktreeError> {
    let reference = format!("refs/heads/{}", worktree.branch);
    let mut command = git_command(&worktree.repo);
    command.args(["show-ref", "--verify", "--quiet", &reference]);
    let output = process::run(command, config.git_timeout).await?;
    match output.exit_code {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(git_failure(&worktree.repo, "show-ref --verify", &output)),
    }
}

async fn prepare_environment(
    config: &WorktreeConfig,
    worktree: &mut Worktree,
) -> Result<(), WorktreeError> {
    for entry in &config.copied {
        if is_available(config, worktree, entry).await? {
            copy_file(&worktree.repo.join(entry), &worktree.path.join(entry))?;
            worktree.copied.push(entry.clone());
        }
    }
    for entry in &config.linked {
        if is_available(config, worktree, entry).await? {
            let link = worktree.path.join(entry);
            create_parent(&link)?;
            std::os::unix::fs::symlink(worktree.repo.join(entry), &link).map_err(|source| {
                WorktreeError::Io {
                    action: "linking",
                    path: link.clone(),
                    source,
                }
            })?;
            worktree.linked.push(entry.clone());
        }
    }
    let added: Vec<PathBuf> = worktree
        .copied
        .iter()
        .chain(&worktree.linked)
        .cloned()
        .collect();
    exclude(config, worktree, &added).await
}

/// Cómo cambió un fichero respecto a la base de la tarea.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    /// Cambió de tipo (fichero normal, enlace, submódulo).
    TypeChanged,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileChange {
    /// Ruta relativa a la raíz del repo, con `/` como separador (lo que da git).
    pub path: String,
    pub kind: ChangeKind,
}

/// Lo que ha hecho una tarea en su worktree respecto al commit del que salió.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diff {
    /// Commits nuevos en la rama de la tarea.
    pub commits: u32,
    /// Si quedan cambios sin commitear (incluidos ficheros nuevos sin añadir).
    pub uncommitted: bool,
    /// Ficheros cambiados, commiteados o no, ordenados por ruta.
    pub files: Vec<FileChange>,
}

impl Diff {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.commits == 0 && self.files.is_empty()
    }
}

/// Mira qué ha cambiado en el worktree desde `base_commit`: lo commiteado, lo que está sin
/// commitear y los ficheros nuevos que git aún no sigue. Los renombrados salen como un
/// borrado y un añadido, que es lo que importa para saber si dos tareas se pisan.
pub async fn diff(config: &WorktreeConfig, worktree: &Worktree) -> Result<Diff, WorktreeError> {
    let range = format!("{}..HEAD", worktree.base_commit);
    let count = git(config, &worktree.path, &["rev-list", "--count", &range]).await?;
    let commits = count
        .stdout
        .trim()
        .parse()
        .map_err(|_| WorktreeError::Git {
            cwd: worktree.path.clone(),
            args: format!("rev-list --count {range}"),
            stderr: format!("unexpected output `{}`", count.stdout.trim()),
        })?;

    let tracked = git(
        config,
        &worktree.path,
        &[
            "diff",
            "--name-status",
            "--no-renames",
            "-z",
            &worktree.base_commit,
            "--",
        ],
    )
    .await?;
    let mut files = parse_name_status(&tracked.stdout).map_err(|stderr| WorktreeError::Git {
        cwd: worktree.path.clone(),
        args: "diff --name-status".to_owned(),
        stderr,
    })?;
    let untracked = git(
        config,
        &worktree.path,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )
    .await?;
    files.extend(
        untracked
            .stdout
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(|path| FileChange {
                path: path.to_owned(),
                kind: ChangeKind::Added,
            }),
    );
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files.dedup_by(|a, b| a.path == b.path);

    let status = git(
        config,
        &worktree.path,
        &["status", "--porcelain", "--untracked-files=normal"],
    )
    .await?;
    Ok(Diff {
        commits,
        uncommitted: !status.stdout.trim().is_empty(),
        files,
    })
}

/// El parche completo de la tarea respecto a su base, como texto (lo commiteado y lo que está
/// sin commitear de los ficheros que git sigue; los nuevos sin añadir salen en [`diff`]).
pub async fn patch(config: &WorktreeConfig, worktree: &Worktree) -> Result<String, WorktreeError> {
    let output = git(
        config,
        &worktree.path,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            &worktree.base_commit,
            "--",
        ],
    )
    .await?;
    Ok(output.stdout)
}

/// Rama por defecto de `origin` según `refs/remotes/origin/HEAD`, que deja `git clone`.
pub async fn default_base(config: &WorktreeConfig, repo: &Path) -> Result<String, WorktreeError> {
    if !repo.is_absolute() || !repo.is_dir() {
        return Err(WorktreeError::InvalidRepo(repo.to_path_buf()));
    }
    let output = git(
        config,
        repo,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )
    .await?;
    let reference = output.stdout.trim();
    reference
        .strip_prefix("origin/")
        .filter(|base| !base.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| WorktreeError::InvalidBase(reference.to_owned()))
}

/// `git diff --name-status -z` sin renombrados: pares `estado\0ruta\0`.
fn parse_name_status(raw: &str) -> Result<Vec<FileChange>, String> {
    let mut fields = raw.split('\0').filter(|field| !field.is_empty());
    let mut files = Vec::new();
    while let Some(status) = fields.next() {
        let path = fields
            .next()
            .ok_or_else(|| format!("status `{status}` without a path"))?;
        let kind = match status.chars().next() {
            Some('A') => ChangeKind::Added,
            Some('M') => ChangeKind::Modified,
            Some('D') => ChangeKind::Deleted,
            Some('T') => ChangeKind::TypeChanged,
            _ => return Err(format!("unexpected status `{status}` for `{path}`")),
        };
        files.push(FileChange {
            path: path.to_owned(),
            kind,
        });
    }
    Ok(files)
}

/// Una entrada se trae al worktree si existe en el checkout y git no la sigue: lo que está
/// en git ya llega con el checkout del worktree, y pisarlo cambiaría el código de la tarea.
async fn is_available(
    config: &WorktreeConfig,
    worktree: &Worktree,
    entry: &Path,
) -> Result<bool, WorktreeError> {
    if !fs_exists(&worktree.repo.join(entry))? || fs_exists(&worktree.path.join(entry))? {
        return Ok(false);
    }
    let mut command = git_command(&worktree.repo);
    command
        .args(["ls-files", "--error-unmatch", "--"])
        .arg(entry);
    let output = process::run(command, config.git_timeout).await?;
    Ok(!output.success())
}

/// Los `.gitignore` suelen decir `node_modules/` con barra, y eso no casa con un enlace
/// simbólico: sin esto, `git add -A` del agente subiría el enlace. Se apunta en el
/// `info/exclude` común a todos los worktrees del repo, que no está en git.
async fn exclude(
    config: &WorktreeConfig,
    worktree: &Worktree,
    entries: &[PathBuf],
) -> Result<(), WorktreeError> {
    if entries.is_empty() {
        return Ok(());
    }
    let common = git(
        config,
        &worktree.path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await?;
    let file = PathBuf::from(common.stdout.trim())
        .join("info")
        .join("exclude");
    let current = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(WorktreeError::Io {
                action: "reading",
                path: file,
                source,
            });
        }
    };
    let mut updated = current.clone();
    for entry in entries {
        let pattern = format!("/{}", entry.to_string_lossy());
        if !current.lines().any(|line| line.trim() == pattern) {
            if !updated.is_empty() && !updated.ends_with('\n') {
                updated.push('\n');
            }
            updated.push_str(&pattern);
            updated.push('\n');
        }
    }
    if updated == current {
        return Ok(());
    }
    create_parent(&file)?;
    std::fs::write(&file, updated).map_err(|source| WorktreeError::Io {
        action: "writing",
        path: file,
        source,
    })
}

fn check_entry(entry: &Path) -> Result<(), WorktreeError> {
    let inside = !entry.as_os_str().is_empty()
        && entry
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
    if inside {
        Ok(())
    } else {
        Err(WorktreeError::InvalidEntry(entry.to_path_buf()))
    }
}

/// La base viene de fuera (API): no puede parecer una opción ni ser un nombre de rama roto.
async fn check_base(config: &WorktreeConfig, repo: &Path, base: &str) -> Result<(), WorktreeError> {
    if base.is_empty() || base.starts_with('-') {
        return Err(WorktreeError::InvalidBase(base.to_owned()));
    }
    let mut command = git_command(repo);
    command.args(["check-ref-format", "--branch", base]);
    let output = process::run(command, config.git_timeout).await?;
    if output.success() {
        Ok(())
    } else {
        Err(WorktreeError::InvalidBase(base.to_owned()))
    }
}

pub(crate) fn git_command(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    command.current_dir(cwd);
    isolate_git_env(&mut command);
    command
}

/// Entorno de git para cualquier proceso que trabaje en un worktree (git o un agente).
pub(crate) fn isolate_git_env(command: &mut Command) {
    command
        // Un fetch que pide contraseña se quedaría colgado hasta el plazo sin nadie delante
        .env("GIT_TERMINAL_PROMPT", "0")
        // Si fleet corre desde un hook de git, estas variables apuntarían a otro repo
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR");
}

pub(crate) async fn git(
    config: &WorktreeConfig,
    cwd: &Path,
    args: &[&str],
) -> Result<ProcessOutput, WorktreeError> {
    let mut command = git_command(cwd);
    command.args(args);
    git_checked(config, cwd, command, &args.join(" ")).await
}

async fn git_checked(
    config: &WorktreeConfig,
    cwd: &Path,
    command: Command,
    label: &str,
) -> Result<ProcessOutput, WorktreeError> {
    let output = process::run(command, config.git_timeout).await?;
    if output.success() {
        Ok(output)
    } else {
        Err(git_failure(cwd, label, &output))
    }
}

pub(crate) fn git_failure(cwd: &Path, label: &str, output: &ProcessOutput) -> WorktreeError {
    let stderr = output.stderr.trim();
    WorktreeError::Git {
        cwd: cwd.to_path_buf(),
        args: label.to_owned(),
        stderr: if stderr.is_empty() {
            format!("exit code {:?}", output.exit_code)
        } else {
            stderr.to_owned()
        },
    }
}

/// `exists()` a secas se traga los errores de permisos y sigue los enlaces rotos.
fn fs_exists(path: &Path) -> Result<bool, WorktreeError> {
    path.try_exists()
        .map(|exists| exists || path.is_symlink())
        .map_err(|source| WorktreeError::Io {
            action: "checking",
            path: path.to_path_buf(),
            source,
        })
}

fn create_parent(path: &Path) -> Result<(), WorktreeError> {
    match path.parent() {
        Some(parent) => std::fs::create_dir_all(parent).map_err(|source| WorktreeError::Io {
            action: "creating",
            path: parent.to_path_buf(),
            source,
        }),
        None => Ok(()),
    }
}

fn copy_file(from: &Path, to: &Path) -> Result<(), WorktreeError> {
    create_parent(to)?;
    std::fs::copy(from, to)
        .map(|_bytes| ())
        .map_err(|source| WorktreeError::Io {
            action: "copying",
            path: from.to_path_buf(),
            source,
        })
}
