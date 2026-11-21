//! API HTTP de `fleetd` (ADR 0001): JSON bajo `/v1`, eventos por SSE y errores con un `code`
//! estable. Solo se sirve en loopback; a la tailnet se comparte con `tailscale serve`.

use std::convert::Infallible;
use std::path::PathBuf;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use fleet_core::placement::{self, NodeCapacity};
use fleet_core::{AgentProvider, RepoRef, TaskId, TaskSpec, TaskState};
use futures_util::stream::{self, Stream};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::watch;

use crate::engine::{Engine, EngineError};
use crate::guard::{self, HostPolicy};
use crate::nodes::{Health, NodeInfo, Nodes};
use crate::proxy::{self, ProxyState};
use crate::web;

/// Lo que comparten los manejadores.
#[derive(Clone)]
pub struct AppState {
    pub engine: Engine,
    /// Nombre de esta máquina, el que dan `/v1/health` y el campo `node` de las tareas.
    pub node: String,
    /// Pasa a `true` al apagar: los flujos SSE se cierran para que el apagado no espere por
    /// ellos (nunca acaban solos).
    pub shutdown: watch::Receiver<bool>,
    /// Qué `Host` se aceptan (ver [`crate::guard`]).
    pub hosts: HostPolicy,
    /// Dónde están los repos que se piden por nombre (ADR 0002).
    pub repos_root: PathBuf,
    /// Los otros `fleetd` de la tailnet.
    pub nodes: Nodes,
}

/// La API, la web, el reenvío a otros nodos y por delante la defensa frente a otras webs.
pub fn router(state: AppState) -> Router {
    let hosts = state.hosts.clone();
    let local = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/nodes", get(list_nodes))
        .route("/v1/tasks", get(list_tasks).post(create_task))
        .route("/v1/tasks/{id}", get(get_task))
        .route("/v1/tasks/{id}/log", get(task_log))
        .route("/v1/tasks/{id}/diff", get(task_diff))
        .route("/v1/tasks/{id}/answer", post(answer_task))
        .route("/v1/tasks/{id}/cancel", post(cancel_task))
        .route("/v1/tasks/{id}/merge", post(merge_task))
        .route("/v1/tasks/{id}/drop", post(drop_task))
        .route("/v1/tasks/{id}/done", post(done_task))
        .route("/v1/overlaps", get(overlaps))
        .route("/v1/events", get(events))
        .merge(web::router())
        .fallback(not_found)
        .with_state(state.clone());
    let proxy_state = ProxyState {
        nodes: state.nodes.clone(),
        local: local.clone(),
        shutdown: state.shutdown.clone(),
    };
    Router::new()
        .route(
            "/v1/nodes/{node}/{*rest}",
            any(proxy::forward).with_state(proxy_state),
        )
        .merge(local)
        .layer(axum::middleware::from_fn_with_state(hosts, guard::layer))
}

/// Error de la API: código HTTP y `{"error": {"code", "message"}}`.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    pub(crate) fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({ "error": { "code": self.code, "message": self.message } });
        (self.status, Json(body)).into_response()
    }
}

impl From<EngineError> for ApiError {
    fn from(err: EngineError) -> Self {
        let (status, code) = match &err {
            EngineError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            EngineError::Invalid(_) => (StatusCode::BAD_REQUEST, "invalid_request"),
            EngineError::Transition(_) => (StatusCode::CONFLICT, "invalid_transition"),
            EngineError::Busy(_) => (StatusCode::CONFLICT, "busy"),
            EngineError::NotRunning(_) => (StatusCode::CONFLICT, "not_running"),
            EngineError::NoWorktree(_) => (StatusCode::CONFLICT, "no_worktree"),
            EngineError::Blocked { .. } => (StatusCode::CONFLICT, "blocked"),
            EngineError::Store(_) | EngineError::Recovery(_) | EngineError::Worktree(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "internal")
            }
        };
        Self::new(status, code, err.to_string())
    }
}

type ApiResult<T> = Result<T, ApiError>;

fn parse_id(raw: &str) -> ApiResult<TaskId> {
    TaskId::parse(raw)
        .map_err(|err| ApiError::new(StatusCode::BAD_REQUEST, "invalid_id", err.to_string()))
}

/// El cuerpo JSON se lee a mano para que un cuerpo malo responda con el mismo formato de error.
fn parse_body<T: DeserializeOwned>(body: &Bytes) -> ApiResult<T> {
    serde_json::from_slice(body).map_err(|err| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("invalid JSON body: {err}"),
        )
    })
}

/// Tareas que ocupan la máquina ahora mismo: lo que cuenta para repartir.
async fn live_tasks(engine: &Engine) -> ApiResult<usize> {
    Ok(engine
        .list()
        .await?
        .iter()
        .filter(|task| {
            matches!(
                task.state,
                TaskState::Provisioning | TaskState::Agent | TaskState::Preflight
            )
        })
        .count())
}

async fn health(State(state): State<AppState>) -> ApiResult<Json<Health>> {
    Ok(Json(Health {
        node: state.node.clone(),
        version: fleet_core::VERSION.to_owned(),
        os: std::env::consts::OS.to_owned(),
        gpu: state.nodes.local_gpu(),
        live_tasks: live_tasks(&state.engine).await?,
    }))
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CreateTask {
    repo: RepoRef,
    prompt: String,
    agent: AgentProvider,
    #[serde(skip_serializing_if = "Option::is_none")]
    base: Option<String>,
    /// Nombre de un nodo o `auto` para que decida el reparto.
    #[serde(skip_serializing_if = "Option::is_none")]
    node: Option<String>,
    /// Etiqueta de quien la pide (ADR 0003); viaja tal cual al nodo que la crea.
    #[serde(skip_serializing_if = "Option::is_none")]
    client: Option<String>,
}

async fn list_nodes(State(state): State<AppState>) -> ApiResult<Json<Vec<NodeInfo>>> {
    Ok(Json(nodes_with_load(&state).await?))
}

/// Los nodos, con las tareas vivas de este contadas ahora.
async fn nodes_with_load(state: &AppState) -> ApiResult<Vec<NodeInfo>> {
    let mut nodes = state.nodes.list().await;
    let local = live_tasks(&state.engine).await?;
    for node in nodes.iter_mut().filter(|node| node.local) {
        node.live_tasks = local;
    }
    Ok(nodes)
}

/// `"node": "auto"`: el nodo donde debe ir la tarea según el ADR 0002.
async fn place(state: &AppState, repo: &RepoRef, agent: &AgentProvider) -> ApiResult<String> {
    let path = repo.resolve(&state.repos_root);
    // El checkout se mira aquí si existe; en otra máquina estará en el mismo sitio relativo
    let need = placement::need(path.is_dir().then_some(path.as_path()), agent);
    match placement::choose(need, &online_capacity(state).await?) {
        Ok(node) => Ok(node),
        Err(_) => {
            // La lista puede ser de hace unos segundos (un nodo que acaba de arrancar o de
            // despertar): una vuelta más antes de decir que no hay dónde
            state.nodes.refresh().await;
            placement::choose(need, &online_capacity(state).await?).map_err(|err| {
                ApiError::new(StatusCode::CONFLICT, "no_capable_node", err.to_string())
            })
        }
    }
}

async fn online_capacity(state: &AppState) -> ApiResult<Vec<NodeCapacity>> {
    Ok(nodes_with_load(state)
        .await?
        .into_iter()
        .filter(|node| node.online)
        .map(|node| NodeCapacity {
            name: node.name,
            os: node.os,
            gpu: node.gpu,
            live_tasks: node.live_tasks,
            local: node.local,
        })
        .collect())
}

async fn create_task(State(state): State<AppState>, body: Bytes) -> ApiResult<Response> {
    let mut request: CreateTask = parse_body(&body)?;
    if request.node.as_deref() == Some("auto") {
        let chosen = place(&state, &request.repo, &request.agent).await?;
        request.node = Some(chosen);
    }
    if let Some(node) = &request.node
        && node != &state.node
    {
        // Se crea allí: el otro nodo resuelve el repo en su máquina
        let body = Bytes::from(serde_json::to_vec(&request).map_err(|err| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                format!("cannot encode the task: {err}"),
            )
        })?);
        return Ok(forward_create(&state, node, body).await);
    }
    let repo = request.repo.resolve(&state.repos_root);
    let base = match request.base {
        Some(base) => base,
        None => state.engine.default_base(&repo).await?,
    };
    let task = state
        .engine
        .create(TaskSpec {
            repo,
            prompt: request.prompt,
            agent: request.agent,
            base,
            client: request.client,
        })
        .await?;
    Ok((StatusCode::CREATED, Json(task)).into_response())
}

async fn forward_create(state: &AppState, node: &str, body: Bytes) -> Response {
    let proxy_state = ProxyState {
        nodes: state.nodes.clone(),
        // No se usa: el nodo no es este
        local: Router::new(),
        shutdown: state.shutdown.clone(),
    };
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/tasks")
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body));
    match request {
        Ok(request) => {
            proxy::forward(
                State(proxy_state),
                Path((node.to_owned(), "tasks".to_owned())),
                request,
            )
            .await
        }
        Err(err) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("cannot build the request: {err}"),
        )
        .into_response(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListFilter {
    /// Solo las tareas con esta etiqueta de cliente.
    client: Option<String>,
}

async fn list_tasks(
    State(state): State<AppState>,
    filter: Result<Query<ListFilter>, QueryRejection>,
) -> ApiResult<Response> {
    let Query(filter) = filter.map_err(|err| {
        ApiError::new(StatusCode::BAD_REQUEST, "invalid_request", err.body_text())
    })?;
    let mut tasks = state.engine.list().await?;
    if let Some(client) = &filter.client {
        tasks.retain(|task| task.client.as_ref() == Some(client));
    }
    Ok(Json(tasks).into_response())
}

async fn get_task(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    let id = parse_id(&id)?;
    Ok(Json(state.engine.get(&id).await?).into_response())
}

fn plain_text(text: String) -> Response {
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response()
}

async fn task_log(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    let id = parse_id(&id)?;
    Ok(plain_text(state.engine.log(&id).await?))
}

async fn task_diff(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    let id = parse_id(&id)?;
    Ok(plain_text(state.engine.patch(&id).await?))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Answer {
    answer: String,
}

async fn answer_task(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<Response> {
    let id = parse_id(&id)?;
    let request: Answer = parse_body(&body)?;
    Ok(Json(state.engine.answer(&id, request.answer).await?).into_response())
}

async fn cancel_task(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    let id = parse_id(&id)?;
    state.engine.cancel(&id).await?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "cancelling": id }))).into_response())
}

async fn merge_task(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    let id = parse_id(&id)?;
    let task = state.engine.merge(&id).await?;
    Ok((StatusCode::ACCEPTED, Json(task)).into_response())
}

async fn drop_task(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    let id = parse_id(&id)?;
    Ok(Json(state.engine.drop_task(&id).await?).into_response())
}

/// El agente externo acabó: pre-flight en marcha (ADR 0003).
async fn done_task(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    let id = parse_id(&id)?;
    let task = state.engine.done(&id).await?;
    Ok((StatusCode::ACCEPTED, Json(task)).into_response())
}

async fn overlaps(State(state): State<AppState>) -> ApiResult<Response> {
    Ok(Json(state.engine.overlaps().await?).into_response())
}

async fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such route")
}

/// Eventos en directo. Cada evento SSE lleva como nombre su tipo y como datos el JSON. Un
/// cliente lento que se queda atrás recibe `lagged` con cuántos se perdió, y sigue.
async fn events(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let receiver = state.engine.subscribe();
    let stream = stream::unfold(
        (receiver, state.shutdown),
        |(mut receiver, mut shutdown)| async move {
            let received = tokio::select! {
                received = receiver.recv() => received,
                () = stopping(&mut shutdown) => return None,
            };
            let event = match received {
                Ok(event) => {
                    let name = match &event {
                        crate::engine::Event::Task { .. } => "task",
                        crate::engine::Event::Output { .. } => "output",
                        crate::engine::Event::Overlaps { .. } => "overlaps",
                    };
                    match SseEvent::default().event(name).json_data(&event) {
                        Ok(sse) => sse,
                        Err(err) => SseEvent::default()
                            .event("error")
                            .data(format!("cannot encode event: {err}")),
                    }
                }
                Err(RecvError::Lagged(skipped)) => SseEvent::default()
                    .event("lagged")
                    .data(json!({ "skipped": skipped }).to_string()),
                Err(RecvError::Closed) => return None,
            };
            Some((Ok(event), (receiver, shutdown)))
        },
    );
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// Acaba cuando empieza el apagado. Sin emisor no habrá apagado que esperar: no acaba nunca.
pub(crate) async fn stopping(shutdown: &mut watch::Receiver<bool>) {
    if shutdown.wait_for(|stopping| *stopping).await.is_err() {
        std::future::pending::<()>().await;
    }
}
