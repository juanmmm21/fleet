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
        hosts: fleetd::guard::HostPolicy::default(),
        nodes: fleetd::nodes::Nodes::local_only("pc")?,
        repos_root: fx.dir.path().to_path_buf(),
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
        json!({
            "version": fleet_core::VERSION,
            "node": "pc",
            "os": std::env::consts::OS,
            "gpu": false,
            "live_tasks": 0
        })
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
            StatusCode::NOT_FOUND,
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
            StatusCode::NOT_FOUND,
            "not_found",
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
    let (status, body) =
        call_json(&router, "POST", &format!("/v1/tasks/{id}/cancel"), None).await?;
    assert_eq!(
        (status, error_code(&body)),
        (StatusCode::CONFLICT, Some("not_running"))
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

#[tokio::test(flavor = "multi_thread")]
async fn serves_the_web_with_relative_paths_and_a_strict_csp() -> TestResult {
    let fx = fixture()?;
    let (router, _) = app(&fx).await?;
    for (uri, content_type) in [
        ("/", "text/html"),
        ("/app.js", "text/javascript"),
        ("/app.css", "text/css"),
    ] {
        let response = router
            .clone()
            .oneshot(Request::get(uri).body(Body::empty())?)
            .await?;
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        let headers = response.headers();
        let header = |name: header::HeaderName| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned()
        };
        assert!(
            header(header::CONTENT_TYPE).starts_with(content_type),
            "{uri}"
        );
        assert!(header(header::CONTENT_SECURITY_POLICY).contains("script-src 'self'"));
        let body = response.into_body().collect().await?.to_bytes();
        let text = String::from_utf8_lossy(&body);
        // Nada absoluto: detrás de `tailscale serve` con prefijo tiene que seguir funcionando
        assert!(!text.contains("\"/v1"), "{uri} uses an absolute API path");
        assert!(
            !text.contains("src=\"/") && !text.contains("href=\"/"),
            "{uri}"
        );
    }
    let (status, body) = call(&router, "GET", "/app.js", None).await?;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("new EventSource(apiBase() + \"events\")"));
    assert!(
        !body.contains("innerHTML"),
        "task text must never be parsed as HTML"
    );
    Ok(())
}

async fn with_headers(
    router: &Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
) -> Result<(StatusCode, Value), Box<dyn std::error::Error>> {
    let mut request = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::from(
            json!({ "repo": "/nowhere", "prompt": "x", "agent": { "kind": "claude" } }).to_string(),
        ))?)
        .await?;
    let status = response.status();
    let bytes = response.into_body().collect().await?.to_bytes();
    Ok((
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    ))
}

#[tokio::test(flavor = "multi_thread")]
async fn other_websites_cannot_drive_the_daemon() -> TestResult {
    let fx = fixture()?;
    let (router, engine) = app(&fx).await?;
    // CSRF: una web cualquiera manda un POST simple a 127.0.0.1
    let (status, body) = with_headers(
        &router,
        "POST",
        "/v1/tasks",
        &[
            ("host", "127.0.0.1:7171"),
            ("origin", "https://evil.example"),
            ("content-type", "text/plain"),
        ],
    )
    .await?;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&body), Some("forbidden"));
    // DNS rebinding: un dominio ajeno que resuelve a 127.0.0.1, incluso para leer
    let (status, _) = with_headers(
        &router,
        "GET",
        "/v1/tasks",
        &[("host", "rebind.evil.example:7171")],
    )
    .await?;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(engine.list().await?.is_empty());

    // La propia web sí: mismo origen (aquí falla después, porque el repo no existe)
    let (status, body) = with_headers(
        &router,
        "POST",
        "/v1/tasks",
        &[
            ("host", "127.0.0.1:7171"),
            ("origin", "http://127.0.0.1:7171"),
        ],
    )
    .await?;
    assert_ne!(status, StatusCode::FORBIDDEN, "{body}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repo_can_be_named_instead_of_a_path() -> TestResult {
    let fx = fixture()?;
    let (router, engine) = app(&fx).await?;
    let mut events = engine.subscribe();
    let name = fx
        .checkout
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("checkout without a name")?
        .to_owned();
    let (status, body) = call_json(
        &router,
        "POST",
        "/v1/tasks",
        Some(json!({ "repo": name, "prompt": "by name", "agent": { "kind": "claude" } })),
    )
    .await?;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // Guarda la ruta ya resuelta, la de verdad en esta máquina
    assert_eq!(body["repo"], json!(fx.checkout));
    let id = TaskId::parse(body["id"].as_str().ok_or("no id")?)?;
    wait_for(&mut events, &id, TaskState::Ready).await?;

    for bad in ["../app", "a/b", ""] {
        let (status, body) = call_json(
            &router,
            "POST",
            "/v1/tasks",
            Some(json!({ "repo": bad, "prompt": "x", "agent": { "kind": "claude" } })),
        )
        .await?;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(error_code(&body), Some("invalid_request"));
    }
    Ok(())
}

/// Los campos de una tarea son el contrato con los clientes (ADR 0001 y 0003, `docs/api.md`):
/// dentro de `/v1` solo se añaden, nunca se quitan ni se renombran.
const TASK_FIELDS: &[&str] = &[
    "agent",
    "base",
    "branch",
    "client",
    "created_at_ms",
    "diff",
    "failure",
    "id",
    "node",
    "preflight",
    "prompt",
    "question",
    "repo",
    "session_id",
    "state",
    "updated_at_ms",
    "worktree",
];

#[tokio::test(flavor = "multi_thread")]
async fn a_client_runs_its_own_agent_and_finds_its_tasks() -> TestResult {
    let fx = fixture()?;
    let (router, engine) = app(&fx).await?;
    let mut events = engine.subscribe();

    let (status, created) = call_json(
        &router,
        "POST",
        "/v1/tasks",
        Some(json!({
            "repo": "app",
            "prompt": "palo's own agent does this",
            "agent": { "kind": "external" },
            "client": "palo:e-7"
        })),
    )
    .await?;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let mut fields: Vec<&str> = created
        .as_object()
        .ok_or("not an object")?
        .keys()
        .map(String::as_str)
        .collect();
    fields.sort_unstable();
    assert_eq!(fields, TASK_FIELDS);
    assert_eq!(created["client"], "palo:e-7");
    let id = TaskId::parse(created["id"].as_str().ok_or("no id")?)?;
    let working = wait_for(&mut events, &id, TaskState::Agent).await?;
    let worktree = working.worktree.ok_or("no worktree")?;

    // Otra tarea de otro cliente no sale en el filtro
    call_json(
        &router,
        "POST",
        "/v1/tasks",
        Some(json!({
            "repo": "app",
            "prompt": "someone else",
            "agent": { "kind": "external" },
            "client": "juan"
        })),
    )
    .await?;
    let (status, mine) = call_json(&router, "GET", "/v1/tasks?client=palo:e-7", None).await?;
    assert_eq!(status, StatusCode::OK);
    let mine = mine.as_array().ok_or("not a list")?;
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0]["id"], id.as_str());
    let (status, body) = call_json(&router, "GET", "/v1/tasks?owner=palo", None).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_code(&body), Some("invalid_request"));

    std::fs::write(worktree.path.join("work.txt"), "by palo\n")?;
    common::git(&worktree.path, &["add", "work.txt"])?;
    common::git(&worktree.path, &["commit", "--quiet", "-m", "work"])?;
    let (status, body) = call_json(&router, "POST", &format!("/v1/tasks/{id}/done"), None).await?;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["state"], "preflight");
    wait_for(&mut events, &id, TaskState::Ready).await?;

    let (status, body) = call_json(&router, "POST", &format!("/v1/tasks/{id}/done"), None).await?;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error_code(&body), Some("invalid_transition"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_client_labels_are_refused() -> TestResult {
    let fx = fixture()?;
    let (router, _) = app(&fx).await?;
    for client in ["", "has space", &"x".repeat(65)] {
        let (status, body) = call_json(
            &router,
            "POST",
            "/v1/tasks",
            Some(json!({
                "repo": "app",
                "prompt": "p",
                "agent": { "kind": "external" },
                "client": client
            })),
        )
        .await?;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{client}: {body}");
        assert_eq!(error_code(&body), Some("invalid_request"));
    }
    Ok(())
}
