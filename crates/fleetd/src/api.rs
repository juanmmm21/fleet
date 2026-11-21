//! API HTTP de `fleetd` (ADR 0001): JSON bajo `/v1`, eventos por SSE y errores con un `code`
//! estable. Solo se sirve en loopback; a la tailnet se comparte con `tailscale serve`.

use std::convert::Infallible;
use std::path::PathBuf;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use fleet_core::{AgentProvider, TaskId, TaskSpec};
use futures_util::stream::{self, Stream};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::watch;

use crate::engine::{Engine, EngineError};
use crate::guard::{self, HostPolicy};

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
}

/// La API y por delante la defensa frente a otras webs del navegador.
pub fn router(state: AppState) -> Router {
    let hosts = state.hosts.clone();
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/tasks", get(list_tasks).post(create_task))
        .route("/v1/tasks/{id}", get(get_task))
        .route("/v1/tasks/{id}/log", get(task_log))
        .route("/v1/tasks/{id}/diff", get(task_diff))
        .route("/v1/tasks/{id}/answer", post(answer_task))
        .route("/v1/tasks/{id}/cancel", post(cancel_task))
        .route("/v1/tasks/{id}/merge", post(merge_task))
        .route("/v1/tasks/{id}/drop", post(drop_task))
        .route("/v1/overlaps", get(overlaps))
        .route("/v1/events", get(events))
        .fallback(not_found)
        .with_state(state)
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
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
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

async fn health(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({ "version": fleet_core::VERSION, "node": state.node }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateTask {
    repo: PathBuf,
    prompt: String,
    agent: AgentProvider,
    base: Option<String>,
    node: Option<String>,
}

async fn create_task(State(state): State<AppState>, body: Bytes) -> ApiResult<Response> {
    let request: CreateTask = parse_body(&body)?;
    if let Some(node) = &request.node
        && node != &state.node
    {
        // Repartir entre máquinas es la Fase 6; hasta entonces, mejor decirlo que ignorarlo
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "unknown_node",
            format!(
                "this is node `{}`; running on `{node}` is not supported yet",
                state.node
            ),
        ));
    }
    let base = match request.base {
        Some(base) => base,
        None => state.engine.default_base(&request.repo).await?,
    };
    let task = state
        .engine
        .create(TaskSpec {
            repo: request.repo,
            prompt: request.prompt,
            agent: request.agent,
            base,
        })
        .await?;
    Ok((StatusCode::CREATED, Json(task)).into_response())
}

async fn list_tasks(State(state): State<AppState>) -> ApiResult<Response> {
    Ok(Json(state.engine.list().await?).into_response())
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
    state.engine.cancel(&id)?;
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
async fn stopping(shutdown: &mut watch::Receiver<bool>) {
    if shutdown.wait_for(|stopping| *stopping).await.is_err() {
        std::future::pending::<()>().await;
    }
}
