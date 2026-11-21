//! Motor de tareas: lleva cada tarea de `provisioning` a `ready` (o a donde toque) con las
//! piezas de `fleet-core`, guarda cada paso en SQLite y lo cuenta por un canal de eventos.
//!
//! Toda modificación de una tarea pasa por [`Engine::mutate`], que la lee de la base de datos,
//! le aplica el cambio y la guarda bajo un mismo cerrojo: el trabajo largo (agente, pruebas,
//! integración) se hace fuera del cerrojo y al acabar se vuelve a pasar por ahí. Si entretanto
//! alguien descartó la tarea, la transición falla y el trabajo se abandona sin pisar nada.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fleet_core::integrate::{self, IntegrationOutcome};
use fleet_core::preflight::{self, PreflightConfig, PreflightError};
use fleet_core::runner::{self, RunConfig};
use fleet_core::worktree::{self, Removal, WorktreeConfig};
use fleet_core::{
    AgentPrograms, Overlap, Store, StoreError, Stream, Task, TaskId, TaskSpec, TaskState,
    TransitionError, overlap, recovery,
};
use serde::Serialize;
use tokio::sync::{Mutex, broadcast, mpsc, watch};

/// Eventos en cola por suscriptor antes de que uno lento empiece a perder los más viejos.
const EVENT_CAPACITY: usize = 1024;
/// Cuántas veces se repite una integración si `origin` se mueve durante las pruebas.
const MERGE_ATTEMPTS: usize = 3;
/// Lo que se espera a que el trabajo de fondo de una tarea se dé de baja tras su último paso
/// (el evento sale un instante antes de que acabe).
const SETTLE_PATIENCE: Duration = Duration::from_secs(5);

/// Dónde y cómo trabaja el motor.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Carpeta de estado: base de datos, registros y worktrees.
    pub state_dir: PathBuf,
    pub programs: AgentPrograms,
    pub run: RunConfig,
    pub preflight: PreflightConfig,
    pub worktree: WorktreeConfig,
    /// Nombre de este nodo, que se apunta en cada tarea nueva.
    pub node: String,
}

impl EngineConfig {
    #[must_use]
    pub fn new(state_dir: PathBuf) -> Self {
        Self {
            worktree: WorktreeConfig::new(state_dir.join("worktrees")),
            state_dir,
            programs: AgentPrograms::default(),
            run: RunConfig::default(),
            preflight: PreflightConfig::default(),
            node: "local".to_owned(),
        }
    }

    fn database(&self) -> PathBuf {
        self.state_dir.join("fleet.sqlite3")
    }

    /// Registro de la tarea: el agente y las pruebas escriben en el mismo, uno tras otro.
    #[must_use]
    pub fn log_path(&self, id: &TaskId) -> PathBuf {
        self.state_dir.join("logs").join(format!("{id}.log"))
    }
}

/// Lo que se cuenta por `GET /v1/events`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// La tarea cambió (estado u otros campos); va entera.
    Task { task: Box<Task> },
    /// Una línea de la salida del agente.
    Output {
        id: TaskId,
        stream: OutputStream,
        line: String,
    },
    /// Todos los pares de tareas que tocan los mismos ficheros, cada vez que una cambia de estado.
    Overlaps { overlaps: Vec<Overlap> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("task {0} does not exist")]
    NotFound(TaskId),
    #[error(transparent)]
    Transition(#[from] TransitionError),
    #[error("{0}")]
    Invalid(String),
    #[error("task {0} is busy; wait for it or cancel it")]
    Busy(TaskId),
    #[error("task {0} is not running")]
    NotRunning(TaskId),
    #[error("task {0} has no worktree yet")]
    NoWorktree(TaskId),
    #[error("task {id} touches the same files as {}; integrate those first", join(.waits_for))]
    Blocked { id: TaskId, waits_for: Vec<TaskId> },
    #[error("cannot recover the tasks: {0}")]
    Recovery(#[from] recovery::RecoveryError),
    #[error("cannot remove the worktree: {0}")]
    Worktree(#[from] worktree::WorktreeError),
}

fn join(ids: &[TaskId]) -> String {
    ids.iter()
        .map(TaskId::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Lo siguiente que hace el trabajo de fondo de una tarea.
enum Step {
    Provision,
    Agent {
        prompt: String,
        resume: Option<String>,
    },
    Preflight,
    Integrate,
}

/// Trabajo de fondo en marcha de una tarea.
struct Job {
    cancel: watch::Sender<bool>,
    done: watch::Receiver<bool>,
}

struct Inner {
    config: EngineConfig,
    store: Store,
    events: broadcast::Sender<Event>,
    /// Serializa leer-cambiar-guardar de las tareas.
    lock: Mutex<()>,
    jobs: StdMutex<HashMap<TaskId, Job>>,
}

/// El motor. Es barato de clonar: todas las copias comparten estado.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

impl Engine {
    /// Abre el estado y recupera las tareas que un reinicio dejó a medias.
    pub async fn start(config: EngineConfig) -> Result<Self, EngineError> {
        create_dir(&config.state_dir)?;
        let store = Store::open(config.database()).await?;
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let engine = Self {
            inner: Arc::new(Inner {
                config,
                store,
                events,
                lock: Mutex::new(()),
                jobs: StdMutex::new(HashMap::new()),
            }),
        };
        let recovered = recovery::recover(&engine.inner.store, now_ms()).await?;
        for item in recovered {
            if item.action != recovery::Recovery::Kept {
                engine.emit(Event::Task {
                    task: Box::new(item.task),
                });
            }
        }
        Ok(engine)
    }

    #[must_use]
    pub fn config(&self) -> &EngineConfig {
        &self.inner.config
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    pub async fn list(&self) -> Result<Vec<Task>, EngineError> {
        Ok(self.inner.store.list().await?)
    }

    pub async fn get(&self, id: &TaskId) -> Result<Task, EngineError> {
        self.inner
            .store
            .get(id)
            .await?
            .ok_or_else(|| EngineError::NotFound(id.clone()))
    }

    /// Pares de tareas vivas que se pisan.
    pub async fn overlaps(&self) -> Result<Vec<Overlap>, EngineError> {
        Ok(overlap::overlaps(&self.inner.store.live().await?))
    }

    /// Registro de la tarea (agente y pruebas); vacío si aún no ha escrito nada.
    pub async fn log(&self, id: &TaskId) -> Result<String, EngineError> {
        self.get(id).await?;
        let path = self.inner.config.log_path(id);
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(err) => Err(EngineError::Invalid(format!(
                "cannot read {}: {err}",
                path.display()
            ))),
        }
    }

    /// Parche de la tarea contra su base.
    pub async fn patch(&self, id: &TaskId) -> Result<String, EngineError> {
        let task = self.get(id).await?;
        let worktree = task
            .worktree
            .ok_or_else(|| EngineError::NoWorktree(id.clone()))?;
        Ok(worktree::patch(&self.inner.config.worktree, &worktree).await?)
    }

    /// Rama por defecto del `origin` de `repo`, para las tareas que no dicen otra.
    pub async fn default_base(&self, repo: &std::path::Path) -> Result<String, EngineError> {
        worktree::default_base(&self.inner.config.worktree, repo)
            .await
            .map_err(|err| EngineError::Invalid(format!("cannot tell the base branch: {err}")))
    }

    /// Crea una tarea y lanza su trabajo de fondo.
    pub async fn create(&self, spec: TaskSpec) -> Result<Task, EngineError> {
        spec.agent
            .validate()
            .map_err(|err| EngineError::Invalid(err.to_string()))?;
        let now = now_ms();
        let mut task = Task::new(TaskId::generate(SystemTime::now()), spec, now)
            .map_err(|err| EngineError::Invalid(err.to_string()))?;
        task.node.clone_from(&self.inner.config.node);
        {
            let _guard = self.inner.lock.lock().await;
            self.inner.store.insert(&task).await?;
        }
        self.emit(Event::Task {
            task: Box::new(task.clone()),
        });
        self.spawn(task.id.clone(), Step::Provision)?;
        Ok(task)
    }

    /// Respuesta a la pregunta de una tarea en `waiting`: el agente sigue en la misma sesión.
    pub async fn answer(&self, id: &TaskId, answer: String) -> Result<Task, EngineError> {
        if answer.trim().is_empty() {
            return Err(EngineError::Invalid("the answer is empty".to_owned()));
        }
        let task = self.get(id).await?;
        if task.state != TaskState::Waiting {
            return Err(TransitionError {
                from: task.state,
                to: TaskState::Agent,
            }
            .into());
        }
        self.wait_idle(id, SETTLE_PATIENCE).await?;
        let task = self
            .mutate(id, |task| {
                if task.state != TaskState::Waiting {
                    return Err(TransitionError {
                        from: task.state,
                        to: TaskState::Agent,
                    }
                    .into());
                }
                task.transition(TaskState::Agent, now_ms())?;
                Ok(())
            })
            .await?;
        let step = match &task.session_id {
            Some(session) => Step::Agent {
                prompt: answer,
                resume: Some(session.clone()),
            },
            // Sin sesión que retomar, se empieza otra con el encargo y la respuesta
            None => Step::Agent {
                prompt: format!(
                    "{}\n\nAnswer to your earlier question: {answer}",
                    task.prompt
                ),
                resume: None,
            },
        };
        self.spawn(id.clone(), step)?;
        Ok(task)
    }

    /// Corta el trabajo de fondo de la tarea; la tarea acaba en `failed` con el motivo.
    pub fn cancel(&self, id: &TaskId) -> Result<(), EngineError> {
        let jobs = self.jobs()?;
        let job = jobs
            .get(id)
            .ok_or_else(|| EngineError::NotRunning(id.clone()))?;
        job.cancel.send_replace(true);
        Ok(())
    }

    /// Integra una tarea `ready` si ninguna más antigua que toque sus ficheros sigue viva.
    pub async fn merge(&self, id: &TaskId) -> Result<Task, EngineError> {
        let task = self.get(id).await?;
        if task.state != TaskState::Ready {
            return Err(TransitionError {
                from: task.state,
                to: TaskState::Merged,
            }
            .into());
        }
        self.wait_idle(id, SETTLE_PATIENCE).await?;
        let live = self.inner.store.live().await?;
        let waits_for = overlap::waits_for(&task, &live);
        if !waits_for.is_empty() {
            return Err(EngineError::Blocked {
                id: id.clone(),
                waits_for,
            });
        }
        self.spawn(id.clone(), Step::Integrate)?;
        Ok(task)
    }

    /// Descarta la tarea a propósito: corta lo que esté haciendo y borra su worktree y su rama.
    pub async fn drop_task(&self, id: &TaskId) -> Result<Task, EngineError> {
        if let Some(job) = self.jobs()?.get(id) {
            job.cancel.send_replace(true);
        }
        // Si el trabajo no acaba a tiempo, mejor no borrar la carpeta bajo sus pies
        let patience = self.inner.config.run.grace + Duration::from_secs(30);
        self.wait_idle(id, patience).await?;
        let task = self
            .mutate(id, |task| {
                task.transition(TaskState::Dropped, now_ms())?;
                Ok(())
            })
            .await?;
        if let Some(worktree) = &task.worktree {
            worktree::remove(&self.inner.config.worktree, worktree, Removal::Discard).await?;
        }
        Ok(task)
    }

    fn jobs(&self) -> Result<std::sync::MutexGuard<'_, HashMap<TaskId, Job>>, EngineError> {
        self.inner
            .jobs
            .lock()
            .map_err(|_| EngineError::Invalid("the job table was poisoned by a panic".to_owned()))
    }

    /// Espera a que la tarea no tenga trabajo de fondo, como mucho `patience`.
    async fn wait_idle(&self, id: &TaskId, patience: Duration) -> Result<(), EngineError> {
        let done = self.jobs()?.get(id).map(|job| job.done.clone());
        let Some(mut done) = done else {
            return Ok(());
        };
        match tokio::time::timeout(patience, done.wait_for(|done| *done)).await {
            // Si el emisor desapareció, el trabajo ya acabó
            Ok(_) => Ok(()),
            Err(_elapsed) => Err(EngineError::Busy(id.clone())),
        }
    }

    fn emit(&self, event: Event) {
        // Sin suscriptores `send` falla, y es normal: nadie mira ahora mismo
        self.inner.events.send(event).ok();
    }

    /// Lee la tarea, le aplica `change` y la guarda; si `change` falla, no se guarda nada.
    async fn mutate<F>(&self, id: &TaskId, change: F) -> Result<Task, EngineError>
    where
        F: FnOnce(&mut Task) -> Result<(), EngineError>,
    {
        let (before, task) = {
            let _guard = self.inner.lock.lock().await;
            let mut task = self
                .inner
                .store
                .get(id)
                .await?
                .ok_or_else(|| EngineError::NotFound(id.clone()))?;
            let before = task.state;
            change(&mut task)?;
            self.inner.store.update(&task).await?;
            (before, task)
        };
        self.emit(Event::Task {
            task: Box::new(task.clone()),
        });
        // El diff solo cambia al acabar una vuelta del agente, y entrar o salir de `failed`,
        // `merged` o `dropped` cambia qué tareas cuentan: los choques se recalculan con el estado
        if before != task.state {
            self.announce_overlaps().await;
        }
        Ok(task)
    }

    /// Pasa la tarea a `failed` con el motivo, salvo que ya no se pueda (descartada entretanto).
    async fn fail(&self, id: &TaskId, reason: String) {
        let result = self
            .mutate(id, |task| {
                task.fail(reason.clone(), now_ms())?;
                Ok(())
            })
            .await;
        if let Err(err) = result {
            report(
                id,
                &format!("could not record the failure `{reason}`: {err}"),
            );
        }
    }

    fn spawn(&self, id: TaskId, step: Step) -> Result<(), EngineError> {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (done_tx, done_rx) = watch::channel(false);
        {
            let mut jobs = self.jobs()?;
            if jobs.contains_key(&id) {
                return Err(EngineError::Busy(id));
            }
            jobs.insert(
                id.clone(),
                Job {
                    cancel: cancel_tx,
                    done: done_rx,
                },
            );
        }
        let engine = self.clone();
        tokio::spawn(async move {
            engine.drive(&id, step, cancel_rx).await;
            match engine.jobs() {
                Ok(mut jobs) => {
                    jobs.remove(&id);
                }
                Err(err) => report(&id, &err.to_string()),
            }
            done_tx.send_replace(true);
        });
        Ok(())
    }

    async fn drive(&self, id: &TaskId, mut step: Step, cancel: watch::Receiver<bool>) {
        loop {
            let next = match step {
                Step::Provision => self.provision(id).await,
                Step::Agent { prompt, resume } => {
                    self.run_agent(id, prompt, resume, cancel.clone()).await
                }
                Step::Preflight => {
                    self.preflight(id, cancel.clone()).await;
                    None
                }
                Step::Integrate => {
                    self.integrate(id, cancel.clone()).await;
                    None
                }
            };
            match next {
                Some(next) => step = next,
                None => return,
            }
        }
    }

    async fn provision(&self, id: &TaskId) -> Option<Step> {
        let task = match self.get(id).await {
            Ok(task) => task,
            Err(err) => {
                report(id, &err.to_string());
                return None;
            }
        };
        let config = &self.inner.config.worktree;
        let created = match worktree::create(config, &task.repo, id, &task.base).await {
            Ok(created) => created,
            Err(err) => {
                self.fail(id, format!("cannot create the worktree: {err}"))
                    .await;
                return None;
            }
        };
        let saved = self
            .mutate(id, |task| {
                task.worktree = Some(created.clone());
                task.transition(TaskState::Agent, now_ms())?;
                Ok(())
            })
            .await;
        match saved {
            Ok(task) => Some(Step::Agent {
                prompt: task.prompt,
                resume: None,
            }),
            Err(err) => {
                // Descartada mientras se creaba: el worktree recién hecho sobra
                report(id, &format!("abandoned after provisioning: {err}"));
                if let Err(err) = worktree::remove(config, &created, Removal::Discard).await {
                    report(id, &format!("cannot remove the new worktree: {err}"));
                }
                None
            }
        }
    }

    async fn run_agent(
        &self,
        id: &TaskId,
        prompt: String,
        resume: Option<String>,
        cancel: watch::Receiver<bool>,
    ) -> Option<Step> {
        let task = match self.get(id).await {
            Ok(task) => task,
            Err(err) => {
                report(id, &err.to_string());
                return None;
            }
        };
        let Some(worktree) = task.worktree.clone() else {
            self.fail(id, "the task has no worktree".to_owned()).await;
            return None;
        };
        let config = &self.inner.config;
        let invocation = match task
            .agent
            .invocation(&config.programs, &prompt, resume.as_deref())
        {
            Ok(invocation) => invocation,
            Err(err) => {
                self.fail(id, format!("cannot launch the agent: {err}"))
                    .await;
                return None;
            }
        };
        let (live_tx, live_rx) = mpsc::channel(256);
        let forwarder = tokio::spawn(forward_output(self.clone(), id.clone(), live_rx));
        let report_result = runner::run_agent(
            &config.worktree,
            &worktree,
            invocation,
            &config.run,
            &config.log_path(id),
            cancelled(cancel),
            Some(live_tx),
        )
        .await;
        if let Err(err) = forwarder.await {
            report(id, &format!("the output forwarder died: {err}"));
        }
        let agent_report = match report_result {
            Ok(agent_report) => agent_report,
            Err(err) => {
                self.fail(id, format!("the agent could not run: {err}"))
                    .await;
                return None;
            }
        };
        let settled = self
            .mutate(id, |task| {
                runner::settle(task, &agent_report, now_ms())?;
                Ok(())
            })
            .await;
        match settled {
            Ok(task) => (task.state == TaskState::Preflight).then_some(Step::Preflight),
            Err(err) => {
                report(id, &format!("abandoned after the agent: {err}"));
                None
            }
        }
    }

    /// Siempre con la lista entera, también vacía: así un cliente sabe que un choque se acabó.
    async fn announce_overlaps(&self) {
        match self.overlaps().await {
            Ok(overlaps) => self.emit(Event::Overlaps { overlaps }),
            Err(err) => eprintln!("fleetd: cannot compute overlaps: {err}"),
        }
    }

    /// Pruebas del repo en el worktree; con `[preflight.act]`, también el job de Actions.
    async fn preflight(&self, id: &TaskId, cancel: watch::Receiver<bool>) {
        let task = match self.get(id).await {
            Ok(task) => task,
            Err(err) => {
                report(id, &err.to_string());
                return;
            }
        };
        let Some(worktree) = task.worktree.clone() else {
            self.fail(id, "the task has no worktree".to_owned()).await;
            return;
        };
        let config = &self.inner.config;
        let log = config.log_path(id);
        let plan = match preflight::plan(&worktree.path, &config.preflight) {
            Ok(plan) => plan,
            Err(err) => {
                self.fail(id, err.to_string()).await;
                return;
            }
        };
        let mut result = preflight::run(
            &worktree.path,
            &plan,
            &config.preflight,
            &log,
            cancelled(cancel.clone()),
        )
        .await;
        if let (Ok(native), Some(act)) = (&result, &plan.act)
            && native.passed()
        {
            result = preflight::run_act(
                &worktree,
                act,
                plan.timeout,
                &config.preflight,
                &log,
                cancelled(cancel),
            )
            .await;
        }
        let result = match result {
            Ok(result) => result,
            Err(PreflightError::Cancelled) => {
                self.fail(id, "the pre-flight was cancelled".to_owned())
                    .await;
                return;
            }
            Err(err) => {
                self.fail(id, format!("the pre-flight could not run: {err}"))
                    .await;
                return;
            }
        };
        let saved = self
            .mutate(id, |task| {
                task.finish_preflight(result, now_ms())?;
                Ok(())
            })
            .await;
        if let Err(err) = saved {
            report(id, &format!("abandoned after the pre-flight: {err}"));
        }
    }

    /// Rebase, pruebas y `push`; si `origin` se mueve entretanto, se repite unas pocas veces.
    /// Integrada, su worktree y su rama sobran y se borran.
    async fn integrate(&self, id: &TaskId, cancel: watch::Receiver<bool>) {
        let config = &self.inner.config;
        for _attempt in 0..MERGE_ATTEMPTS {
            let task = match self.get(id).await {
                Ok(task) => task,
                Err(err) => {
                    report(id, &err.to_string());
                    return;
                }
            };
            let Some(mut worktree) = task.worktree.clone() else {
                self.fail(id, "the task has no worktree".to_owned()).await;
                return;
            };
            let plan = match preflight::plan(&worktree.path, &config.preflight) {
                Ok(plan) => plan,
                Err(err) => {
                    self.fail(id, err.to_string()).await;
                    return;
                }
            };
            let outcome = integrate::integrate(
                &config.worktree,
                &mut worktree,
                &plan,
                &config.preflight,
                &config.log_path(id),
                cancelled(cancel.clone()),
            )
            .await;
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(err) => {
                    self.fail(id, format!("cannot integrate: {err}")).await;
                    return;
                }
            };
            let saved = self
                .mutate(id, |task| {
                    task.worktree = Some(worktree.clone());
                    integrate::settle(task, &outcome, now_ms())?;
                    Ok(())
                })
                .await;
            if let Err(err) = saved {
                report(id, &format!("abandoned after integrating: {err}"));
                return;
            }
            match outcome {
                IntegrationOutcome::OriginMoved => {}
                IntegrationOutcome::Merged { .. } => {
                    if let Err(err) =
                        worktree::remove(&config.worktree, &worktree, Removal::IfIntegrated).await
                    {
                        report(
                            id,
                            &format!("merged, but cannot remove the worktree: {err}"),
                        );
                    }
                    return;
                }
                IntegrationOutcome::Conflict { .. } | IntegrationOutcome::Red(_) => return,
            }
        }
        report(
            id,
            &format!("origin kept moving after {MERGE_ATTEMPTS} attempts; the task stays ready"),
        );
    }
}

/// Reenvía la salida del agente como eventos.
async fn forward_output(
    engine: Engine,
    id: TaskId,
    mut lines: mpsc::Receiver<fleet_core::OutputLine>,
) {
    while let Some(line) = lines.recv().await {
        let stream = match line.stream {
            Stream::Stdout => OutputStream::Stdout,
            Stream::Stderr => OutputStream::Stderr,
        };
        engine.emit(Event::Output {
            id: id.clone(),
            stream,
            line: line.line,
        });
    }
}

/// Un futuro que acaba cuando se pide cortar el trabajo. Si el emisor desaparece sin pedirlo,
/// no acaba nunca: el trabajo sigue.
async fn cancelled(mut cancel: watch::Receiver<bool>) {
    if cancel.wait_for(|cancelled| *cancelled).await.is_err() {
        std::future::pending::<()>().await;
    }
}

fn create_dir(path: &std::path::Path) -> Result<(), EngineError> {
    std::fs::create_dir_all(path)
        .map_err(|err| EngineError::Invalid(format!("cannot create {}: {err}", path.display())))
}

/// Lo que el trabajo de fondo no puede devolver a nadie va al registro del demonio (stderr).
fn report(id: &TaskId, message: &str) {
    eprintln!("fleetd: task {id}: {message}");
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}
