//! La API sobre el motor de verdad, sin abrir un puerto: `tower::ServiceExt::oneshot`.

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use fleet_core::{TaskId, TaskState};
use fleetd::Engine;
use fleetd::api::{self, AppState};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

mod common;
use common::{Fixture, TestResult, fixture, wait_for};

async fn app(fx: &Fixture) -> Result<(Router, Engine), Box<dyn std::error::Error>> {
    let engine = Engine::start(fx.config()?).await?;
    // En los tests nadie apaga; soltar el emisor no cierra los flujos
    let (_stopping, shutdown) = tokio::sync::watch::channel(false);
    let router = api::router(AppState {
        engine: engine.clone(),
        node: "pc".to_owned(),
        shutdown,
    });
    Ok((router, engine))
}

async fn call(
    router: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> Result<(StatusCode, String), Box<dyn std::error::Error>> {
    let request = Request::builder().method(method).uri(uri);
    let request = match body {
        Some(body) => request
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))?,
        None => request.body(Body::empty())?,
    };
    let response = router.clone().oneshot(request).await?;
    let status = response.status();
    let bytes = response.into_body().collect().await?.to_bytes();
    Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
}

async fn call_json(
    router: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> Result<(StatusCode, Value), Box<dyn std::error::Error>> {
    let (status, text) = call(router, method, uri, body).await?;
    Ok((status, serde_json::from_str(&text)?))
}

fn error_code(body: &Value) -> Option<&str> {
    body.pointer("/error/code").and_then(Value::as_str)
}

#[tokio::test(flavor = "multi_thread")]
async fn health_says_version_and_node() -> TestResult {
    let fx = fixture()?;
    let (router, _) = app(&fx).await?;
    let (status, body) = call_json(&router, "GET", "/v1/health", None).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({ "version": fleet_core::VERSION, "node": "pc" })
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_through_the_api_from_create_to_merge() -> TestResult {
    let fx = fixture()?;
    let (router, engine) = app(&fx).await?;
    let mut events = engine.subscribe();

    // Sin `base`: se toma la rama por defecto de `origin`
    let (status, created) = call_json(
        &router,
        "POST",
        "/v1/tasks",
        Some(json!({
            "repo": fx.checkout,
            "prompt": "write the work file",
            "agent": { "kind": "claude" }
        })),
    )
    .await?;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["base"], "main");
    assert_eq!(created["state"], "provisioning");
    let id = TaskId::parse(created["id"].as_str().ok_or("no id")?)?;
    wait_for(&mut events, &id, TaskState::Ready).await?;

    let (status, task) = call_json(&router, "GET", &format!("/v1/tasks/{id}"), None).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(task["state"], "ready");
    assert_eq!(task["diff"]["files"][0]["path"], "work.txt");

    let (_, list) = call_json(&router, "GET", "/v1/tasks", None).await?;
    assert_eq!(list.as_array().map(Vec::len), Some(1));

    let (status, log) = call(&router, "GET", &format!("/v1/tasks/{id}/log"), None).await?;
    assert_eq!(status, StatusCode::OK);
    assert!(log.contains("== preflight"), "{log}");
    let (status, patch) = call(&router, "GET", &format!("/v1/tasks/{id}/diff"), None).await?;
    assert_eq!(status, StatusCode::OK);
    assert!(patch.contains("+++ b/work.txt"), "{patch}");

    let (status, overlaps) = call_json(&router, "GET", "/v1/overlaps", None).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(overlaps, json!([]));

    let (status, _) = call_json(&router, "POST", &format!("/v1/tasks/{id}/merge"), None).await?;
    assert_eq!(status, StatusCode::ACCEPTED);
    wait_for(&mut events, &id, TaskState::Merged).await?;
    assert!(fx.origin_has("work.txt")?);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn errors_have_a_status_and_a_stable_code() -> TestResult {
    let fx = fixture()?;
    let (router, engine) = app(&fx).await?;
    let mut events = engine.subscribe();
    let repo = fx.checkout.clone();

    let cases = [
        (
            "POST",
            "/v1/tasks".to_owned(),
            Some(json!("not an object")),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            "POST",
            "/v1/tasks".to_owned(),
            Some(json!({ "repo": repo, "prompt": "x", "agent": { "kind": "claude" }, "typo": 1 })),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            "POST",
            "/v1/tasks".to_owned(),
            Some(
                json!({ "repo": repo, "prompt": "x", "agent": { "kind": "claude" }, "node": "mac" }),
            ),
            StatusCode::BAD_REQUEST,
            "unknown_node",
        ),
        (
            "POST",
            "/v1/tasks".to_owned(),
            Some(
                json!({ "repo": repo, "prompt": " ", "agent": { "kind": "claude" }, "base": "main" }),
            ),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            "GET",
            "/v1/tasks/..%2Fetc".to_owned(),
            None,
            StatusCode::BAD_REQUEST,
            "invalid_id",
        ),
        (
            "GET",
            "/v1/tasks/nope".to_owned(),
            None,
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            "POST",
            "/v1/tasks/nope/cancel".to_owned(),
            None,
            StatusCode::CONFLICT,
            "not_running",
        ),
        (
            "GET",
            "/v2/anything".to_owned(),
            None,
            StatusCode::NOT_FOUND,
            "not_found",
        ),
    ];
    for (method, uri, body, status, code) in cases {
        let (got, body) = call_json(&router, method, &uri, body).await?;
        assert_eq!(
            (got, error_code(&body)),
            (status, Some(code)),
            "{method} {uri}: {body}"
        );
    }

    // Una tarea lista no acepta respuesta ni tiene diff sin worktree antes de crearlo
    let (_, created) = call_json(
        &router,
        "POST",
        "/v1/tasks",
        Some(json!({ "repo": repo, "prompt": "write", "agent": { "kind": "claude" } })),
    )
    .await?;
    let id = TaskId::parse(created["id"].as_str().ok_or("no id")?)?;
    wait_for(&mut events, &id, TaskState::Ready).await?;
    let (status, body) = call_json(
        &router,
        "POST",
        &format!("/v1/tasks/{id}/answer"),
        Some(json!({ "answer": "yes" })),
    )
    .await?;
    assert_eq!(
        (status, error_code(&body)),
        (StatusCode::CONFLICT, Some("invalid_transition"))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn events_stream_over_sse() -> TestResult {
    let fx = fixture()?;
    let (router, engine) = app(&fx).await?;
    let request = Request::get("/v1/events").body(Body::empty())?;
    let response = router.clone().oneshot(request).await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .map(|v| v.as_bytes()),
        Some(&b"text/event-stream"[..])
    );
    let mut body = response.into_body();

    engine.create(fx.spec("write the work file")).await?;
    let frame = tokio::time::timeout(Duration::from_secs(10), body.frame())
        .await?
        .ok_or("the stream ended")??;
    let chunk = frame.into_data().map_err(|_| "not a data frame")?;
    let text = String::from_utf8_lossy(&chunk);
    assert!(text.starts_with("event: task\ndata: {"), "{text}");
    assert!(text.contains("\"state\":\"provisioning\""), "{text}");
    Ok(())
}
