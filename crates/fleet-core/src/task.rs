//! Tareas y su máquina de estados (ADR 0001).

use std::fmt;
use std::hash::{BuildHasher, Hasher, RandomState};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::agent::AgentProvider;
use crate::preflight::PreflightResult;

/// Longitud máxima de un id: va dentro de un nombre de rama y de carpeta.
const MAX_ID_LEN: usize = 64;
/// Los milisegundos van con ancho fijo para que el orden alfabético sea el temporal.
const TIME_WIDTH: usize = 10;
const RANDOM_WIDTH: usize = 4;
const BASE36: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// Identificador de una tarea: corto, ordenable por tiempo y válido como rama y carpeta.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TaskId(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TaskIdError {
    #[error("task id is empty")]
    Empty,
    #[error("task id is longer than {MAX_ID_LEN} characters")]
    TooLong,
    #[error("task id `{0}` may only contain a-z, 0-9 and inner dashes")]
    InvalidCharacters(String),
}

impl TaskId {
    /// Genera un id nuevo a partir del instante `now`: milisegundos en base 36 y cuatro
    /// caracteres aleatorios para que dos tareas del mismo milisegundo no choquen.
    #[must_use]
    pub fn generate(now: SystemTime) -> Self {
        let millis = now
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis());
        let mut id = base36(millis, TIME_WIDTH);
        id.push('-');
        // RandomState trae claves aleatorias por proceso: basta para desempatar sin otro crate
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u128(millis);
        id.push_str(&base36(u128::from(hasher.finish()), RANDOM_WIDTH));
        Self(id)
    }

    /// Valida un id que viene de fuera (API, base de datos).
    pub fn parse(raw: &str) -> Result<Self, TaskIdError> {
        if raw.is_empty() {
            return Err(TaskIdError::Empty);
        }
        if raw.len() > MAX_ID_LEN {
            return Err(TaskIdError::TooLong);
        }
        let valid_chars = raw
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !valid_chars || raw.starts_with('-') || raw.ends_with('-') || raw.contains("--") {
            return Err(TaskIdError::InvalidCharacters(raw.to_owned()));
        }
        Ok(Self(raw.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Rama de git en la que trabaja la tarea.
    #[must_use]
    pub fn branch(&self) -> String {
        format!("fleet/{}", self.0)
    }
}

/// Escribe `value` en base 36 con exactamente `width` cifras: rellena con ceros a la izquierda
/// y, si sobran, se queda con las menos significativas.
fn base36(mut value: u128, width: usize) -> String {
    let mut digits = Vec::with_capacity(width);
    while value > 0 {
        let digit = usize::try_from(value % 36).unwrap_or(0);
        digits.push(char::from(BASE36[digit]));
        value /= 36;
    }
    while digits.len() < width {
        digits.push('0');
    }
    digits.truncate(width);
    digits.iter().rev().collect()
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for TaskId {
    type Err = TaskIdError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::parse(raw)
    }
}

impl TryFrom<String> for TaskId {
    type Error = TaskIdError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(&raw)
    }
}

impl From<TaskId> for String {
    fn from(id: TaskId) -> Self {
        id.0
    }
}

/// Estado de una tarea. Las únicas transiciones válidas son las de [`TaskState::can_transition`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// Creando el worktree y preparando su entorno.
    Provisioning,
    /// El agente está trabajando.
    Agent,
    /// El agente ha preguntado algo y espera respuesta.
    Waiting,
    /// Pasando las pruebas del repo en el worktree.
    Preflight,
    /// Pruebas en verde: lista para integrar.
    Ready,
    /// Integrada en la rama base.
    Merged,
    /// Algo salió mal; la rama y el worktree se quedan para mirarlos.
    Failed,
    /// Descartada a propósito.
    Dropped,
}

impl TaskState {
    pub const ALL: [Self; 8] = [
        Self::Provisioning,
        Self::Agent,
        Self::Waiting,
        Self::Preflight,
        Self::Ready,
        Self::Merged,
        Self::Failed,
        Self::Dropped,
    ];

    /// Nombre estable del estado, el mismo que en la API y en la base de datos.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Provisioning => "provisioning",
            Self::Agent => "agent",
            Self::Waiting => "waiting",
            Self::Preflight => "preflight",
            Self::Ready => "ready",
            Self::Merged => "merged",
            Self::Failed => "failed",
            Self::Dropped => "dropped",
        }
    }

    /// Una tarea integrada o descartada ya no cambia.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Merged | Self::Dropped)
    }

    /// Si se puede pasar de `self` a `to`.
    #[must_use]
    pub const fn can_transition(self, to: Self) -> bool {
        use TaskState::{Agent, Dropped, Failed, Merged, Preflight, Provisioning, Ready, Waiting};
        match self {
            Provisioning => matches!(to, Agent | Failed | Dropped),
            Agent => matches!(to, Waiting | Preflight | Failed | Dropped),
            // Con la respuesta de Juan el agente sigue
            Waiting => matches!(to, Agent | Failed | Dropped),
            Preflight => matches!(to, Ready | Failed | Dropped),
            // Integrar puede fallar (conflicto o pruebas en rojo tras el rebase)
            Ready => matches!(to, Merged | Failed | Dropped),
            // Una tarea fallida se reintenta con el agente o se descarta; nunca se integra
            // sin volver a pasar por el pre-flight
            Failed => matches!(to, Agent | Dropped),
            Merged | Dropped => false,
        }
    }
}

impl fmt::Display for TaskState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("task cannot go from {from} to {to}")]
pub struct TransitionError {
    pub from: TaskState,
    pub to: TaskState,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NewTaskError {
    #[error("the task prompt is empty")]
    EmptyPrompt,
    #[error("the base branch is empty")]
    EmptyBase,
    #[error("the repository path must be absolute: {0}")]
    RelativeRepo(PathBuf),
}

/// Lo que pide quien crea una tarea.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSpec {
    /// Checkout del repo en esta máquina (ruta absoluta).
    pub repo: PathBuf,
    pub prompt: String,
    pub agent: AgentProvider,
    /// Rama de `origin` de la que sale la tarea y en la que se integra.
    pub base: String,
}

/// Una tarea con su estado. Los instantes van en milisegundos Unix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub repo: PathBuf,
    pub prompt: String,
    pub agent: AgentProvider,
    pub base: String,
    pub branch: String,
    /// Carpeta del worktree, cuando ya existe.
    pub worktree: Option<PathBuf>,
    pub state: TaskState,
    /// Por qué falló, mientras está en `failed`.
    pub failure: Option<String>,
    /// Pregunta del agente, mientras está en `waiting`.
    pub question: Option<String>,
    /// Último pre-flight pasado en el worktree.
    pub preflight: Option<PreflightResult>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

impl Task {
    /// Crea una tarea en `provisioning`.
    pub fn new(id: TaskId, spec: TaskSpec, now_ms: u64) -> Result<Self, NewTaskError> {
        if spec.prompt.trim().is_empty() {
            return Err(NewTaskError::EmptyPrompt);
        }
        if spec.base.trim().is_empty() {
            return Err(NewTaskError::EmptyBase);
        }
        if !spec.repo.is_absolute() {
            return Err(NewTaskError::RelativeRepo(spec.repo));
        }
        let branch = id.branch();
        Ok(Self {
            id,
            repo: spec.repo,
            prompt: spec.prompt,
            agent: spec.agent,
            base: spec.base,
            branch,
            worktree: None,
            state: TaskState::Provisioning,
            failure: None,
            question: None,
            preflight: None,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        })
    }

    /// Cambia de estado si la transición es válida. Al salir de `failed` o de `waiting`
    /// se olvidan el motivo y la pregunta, que solo tienen sentido en esos estados.
    pub fn transition(&mut self, to: TaskState, now_ms: u64) -> Result<(), TransitionError> {
        if !self.state.can_transition(to) {
            return Err(TransitionError {
                from: self.state,
                to,
            });
        }
        if to != TaskState::Failed {
            self.failure = None;
        }
        if to != TaskState::Waiting {
            self.question = None;
        }
        self.state = to;
        self.updated_at_ms = now_ms.max(self.updated_at_ms);
        Ok(())
    }

    /// Pasa a `failed` con el motivo.
    pub fn fail(&mut self, reason: impl Into<String>, now_ms: u64) -> Result<(), TransitionError> {
        self.transition(TaskState::Failed, now_ms)?;
        self.failure = Some(reason.into());
        Ok(())
    }

    /// Pasa a `waiting` con la pregunta del agente.
    pub fn ask(&mut self, question: impl Into<String>, now_ms: u64) -> Result<(), TransitionError> {
        self.transition(TaskState::Waiting, now_ms)?;
        self.question = Some(question.into());
        Ok(())
    }

    /// Apunta el resultado del pre-flight y pasa a `ready` o a `failed` según haya ido.
    pub fn finish_preflight(
        &mut self,
        result: PreflightResult,
        now_ms: u64,
    ) -> Result<(), TransitionError> {
        if self.state != TaskState::Preflight {
            let to = if result.passed() {
                TaskState::Ready
            } else {
                TaskState::Failed
            };
            return Err(TransitionError {
                from: self.state,
                to,
            });
        }
        let summary = result.summary();
        let passed = result.passed();
        self.preflight = Some(result);
        if passed {
            self.transition(TaskState::Ready, now_ms)
        } else {
            self.fail(summary, now_ms)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn spec() -> TaskSpec {
        TaskSpec {
            repo: PathBuf::from("/home/juan/Desarrollo/fleet"),
            prompt: "add the task types".to_owned(),
            agent: AgentProvider::Claude { model: None },
            base: "main".to_owned(),
        }
    }

    fn task() -> Result<Task, Box<dyn std::error::Error>> {
        Ok(Task::new(TaskId::parse("abc")?, spec(), 10)?)
    }

    fn preflight(exit_code: Option<i32>, timed_out: bool) -> PreflightResult {
        PreflightResult {
            command: vec!["cargo".to_owned(), "test".to_owned()],
            exit_code,
            timed_out,
            duration_ms: 1200,
            output_tail: "test result: ok".to_owned(),
        }
    }

    #[test]
    fn generated_ids_are_valid_and_sort_by_time() -> Result<(), TaskIdError> {
        let earlier = TaskId::generate(UNIX_EPOCH + Duration::from_millis(1_763_700_000_000));
        let later = TaskId::generate(UNIX_EPOCH + Duration::from_millis(1_763_700_000_001));
        assert_eq!(TaskId::parse(earlier.as_str())?, earlier);
        assert_eq!(earlier.as_str().len(), TIME_WIDTH + 1 + RANDOM_WIDTH);
        assert!(earlier < later);
        Ok(())
    }

    #[test]
    fn rejects_ids_that_are_not_safe_as_branch_or_folder() {
        assert_eq!(TaskId::parse(""), Err(TaskIdError::Empty));
        assert_eq!(TaskId::parse(&"a".repeat(65)), Err(TaskIdError::TooLong));
        for raw in ["Abc", "a b", "a/b", "-a", "a-", "a--b", "../x", "ñ"] {
            assert!(
                matches!(TaskId::parse(raw), Err(TaskIdError::InvalidCharacters(_))),
                "{raw} should be rejected"
            );
        }
    }

    #[test]
    fn id_serializes_as_a_plain_string_and_validates_on_the_way_in() {
        let id = TaskId::parse("mi3x-0a1b");
        assert!(id.is_ok());
        let json = id.ok().and_then(|id| serde_json::to_string(&id).ok());
        assert_eq!(json.as_deref(), Some("\"mi3x-0a1b\""));
        assert!(serde_json::from_str::<TaskId>("\"../etc\"").is_err());
    }

    #[test]
    fn base36_pads_to_width() {
        assert_eq!(base36(0, 4), "0000");
        assert_eq!(base36(35, 2), "0z");
        assert_eq!(base36(36, 2), "10");
        assert_eq!(base36(36_u128.pow(4) + 1, 4), "0001");
    }

    #[test]
    fn happy_path_reaches_merged() -> Result<(), Box<dyn std::error::Error>> {
        let mut task = task()?;
        assert_eq!(task.branch, "fleet/abc");
        task.transition(TaskState::Agent, 11)?;
        task.ask("which crate?", 12)?;
        assert_eq!(task.question.as_deref(), Some("which crate?"));
        task.transition(TaskState::Agent, 13)?;
        assert_eq!(task.question, None);
        task.transition(TaskState::Preflight, 14)?;
        task.finish_preflight(preflight(Some(0), false), 15)?;
        assert_eq!(task.state, TaskState::Ready);
        task.transition(TaskState::Merged, 16)?;
        assert!(task.state.is_terminal());
        assert_eq!(task.updated_at_ms, 16);
        Ok(())
    }

    #[test]
    fn red_preflight_fails_with_its_summary_and_can_retry() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut task = task()?;
        task.transition(TaskState::Agent, 11)?;
        task.transition(TaskState::Preflight, 12)?;
        task.finish_preflight(preflight(Some(101), false), 13)?;
        assert_eq!(task.state, TaskState::Failed);
        assert_eq!(
            task.failure.as_deref(),
            Some("`cargo test` exited with code 101")
        );
        task.transition(TaskState::Agent, 14)?;
        assert_eq!(task.failure, None);
        assert!(task.preflight.is_some());
        Ok(())
    }

    #[test]
    fn finish_preflight_outside_preflight_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let mut task = task()?;
        let err = task.finish_preflight(preflight(Some(0), false), 11);
        assert_eq!(
            err,
            Err(TransitionError {
                from: TaskState::Provisioning,
                to: TaskState::Ready
            })
        );
        assert_eq!(task.preflight, None);
        Ok(())
    }

    #[test]
    fn invalid_transitions_leave_the_task_untouched() -> Result<(), Box<dyn std::error::Error>> {
        let mut task = task()?;
        let before = task.clone();
        assert!(task.transition(TaskState::Merged, 99).is_err());
        assert!(task.transition(TaskState::Ready, 99).is_err());
        assert_eq!(task, before);
        Ok(())
    }

    #[test]
    fn failed_tasks_never_merge_without_a_new_preflight() {
        assert!(!TaskState::Failed.can_transition(TaskState::Merged));
        assert!(!TaskState::Failed.can_transition(TaskState::Ready));
        assert!(!TaskState::Agent.can_transition(TaskState::Merged));
    }

    #[test]
    fn terminal_states_have_no_way_out() {
        for from in [TaskState::Merged, TaskState::Dropped] {
            assert!(from.is_terminal());
            for to in TaskState::ALL {
                assert!(!from.can_transition(to), "{from} -> {to}");
            }
        }
    }

    #[test]
    fn every_live_state_can_be_dropped() {
        for from in TaskState::ALL.into_iter().filter(|s| !s.is_terminal()) {
            assert!(from.can_transition(TaskState::Dropped), "{from}");
        }
    }

    #[test]
    fn state_names_match_serde() -> Result<(), serde_json::Error> {
        for state in TaskState::ALL {
            assert_eq!(
                serde_json::to_string(&state)?,
                format!("\"{}\"", state.as_str())
            );
        }
        Ok(())
    }

    #[test]
    fn new_task_validates_its_spec() {
        let id = || TaskId::generate(SystemTime::now());
        let empty_prompt = TaskSpec {
            prompt: "  ".to_owned(),
            ..spec()
        };
        assert_eq!(
            Task::new(id(), empty_prompt, 0),
            Err(NewTaskError::EmptyPrompt)
        );
        let empty_base = TaskSpec {
            base: String::new(),
            ..spec()
        };
        assert_eq!(Task::new(id(), empty_base, 0), Err(NewTaskError::EmptyBase));
        let relative = TaskSpec {
            repo: PathBuf::from("fleet"),
            ..spec()
        };
        assert_eq!(
            Task::new(id(), relative, 0),
            Err(NewTaskError::RelativeRepo(PathBuf::from("fleet")))
        );
    }
}
