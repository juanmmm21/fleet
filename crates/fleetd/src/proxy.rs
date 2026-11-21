//! `/v1/nodes/{node}/...` (ADR 0002): reenvía la petición a `/v1/...` de otro nodo, flujos SSE
//! incluidos. Para este mismo nodo no sale a la red: la atiende la API local.

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use futures_util::stream::{self, StreamExt};
use tokio::sync::watch;
use tower::ServiceExt;

use crate::api::ApiError;
use crate::nodes::{NodeInfo, Nodes};

/// Más que de sobra para cualquier cuerpo de la API (un encargo, una respuesta).
const MAX_BODY: usize = 4 * 1024 * 1024;

#[derive(Clone)]
pub struct ProxyState {
    pub nodes: Nodes,
    /// La API de este nodo, para lo que va dirigido a él mismo.
    pub local: Router,
    pub shutdown: watch::Receiver<bool>,
}

pub async fn forward(
    State(state): State<ProxyState>,
    Path((node, rest)): Path<(String, String)>,
    request: Request,
) -> Response {
    let query = request
        .uri()
        .query()
        .map(|query| format!("?{query}"))
        .unwrap_or_default();
    let path = format!("/v1/{rest}{query}");
    if node == state.nodes.local_name() {
        return local(state.local, request, &path).await;
    }
    match state.nodes.find(&node).await {
        None => ApiError::new(
            StatusCode::NOT_FOUND,
            "unknown_node",
            format!("no fleet node called `{node}` on the tailnet"),
        )
        .into_response(),
        Some(info) if !info.online => offline(&info),
        Some(info) => remote(&state, &info, request, &path).await,
    }
}

fn offline(info: &NodeInfo) -> Response {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "node_offline",
        format!("node `{}` is not answering (asleep or off?)", info.name),
    )
    .into_response()
}

async fn local(router: Router, mut request: Request, path: &str) -> Response {
    match path.parse::<Uri>() {
        Ok(uri) => *request.uri_mut() = uri,
        Err(err) => {
            return ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                format!("bad path: {err}"),
            )
            .into_response();
        }
    }
    match router.oneshot(request).await {
        Ok(response) => response,
        Err(never) => match never {},
    }
}

async fn remote(state: &ProxyState, info: &NodeInfo, request: Request, path: &str) -> Response {
    let (parts, body) = request.into_parts();
    let body = match to_bytes(body, MAX_BODY).await {
        Ok(body) => body,
        Err(err) => {
            return ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                format!("cannot read the body: {err}"),
            )
            .into_response();
        }
    };
    // Solo lo que describe el cuerpo: `Host`, `Origin` y la identidad de Tailscale los pone
    // el camino hasta el otro nodo, no quien llama
    let mut outgoing = state
        .nodes
        .client()
        .request(parts.method, format!("{}{path}", info.url))
        .body(body);
    for name in [header::CONTENT_TYPE, header::ACCEPT] {
        if let Some(value) = parts.headers.get(&name) {
            outgoing = outgoing.header(name, value);
        }
    }
    let response = match outgoing.send().await {
        Ok(response) => response,
        Err(err) if err.is_connect() || err.is_timeout() => return offline(info),
        Err(err) => {
            return ApiError::new(
                StatusCode::BAD_GATEWAY,
                "node_error",
                format!("node `{}` failed: {err}", info.name),
            )
            .into_response();
        }
    };
    let status = response.status();
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    let body = until_shutdown(response.bytes_stream(), state.shutdown.clone());
    let mut reply = Response::new(Body::from_stream(body));
    *reply.status_mut() = status;
    if let Some(content_type) = content_type {
        reply
            .headers_mut()
            .insert(header::CONTENT_TYPE, content_type);
    }
    if status.is_success() {
        // Un flujo de eventos no se guarda en ningún sitio por el camino
        reply
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    }
    reply
}

/// Corta el cuerpo reenviado al apagar: un flujo SSE de otro nodo no acaba nunca y el apagado
/// limpio esperaría por él.
fn until_shutdown<S>(
    body: S,
    shutdown: watch::Receiver<bool>,
) -> impl futures_util::Stream<Item = Result<axum::body::Bytes, reqwest::Error>> + Send + 'static
where
    S: futures_util::Stream<Item = Result<axum::body::Bytes, reqwest::Error>> + Send + 'static,
{
    stream::unfold(
        (Box::pin(body), shutdown),
        |(mut body, mut shutdown)| async move {
            let next = tokio::select! {
                next = body.next() => next,
                () = crate::api::stopping(&mut shutdown) => return None,
            };
            next.map(|item| (item, (body, shutdown)))
        },
    )
}
