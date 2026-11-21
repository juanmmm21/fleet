//! Dos `fleetd` de verdad en puertos efímeros: el «PC» conoce al «Mac» por `--peer` y le
//! reenvía peticiones (ADR 0002). Sin Tailscale: los pares van por HTTP en loopback.

use std::time::Duration;

use fleet_core::{TaskId, TaskState};
use fleetd::api::{self, AppState};
use fleetd::nodes::{NodeInfo, Nodes, NodesConfig};
use fleetd::{Engine, guard::HostPolicy};
use serde_json::{Value, json};
use tokio::sync::watch;

mod common;
use common::{Fixture, TestResult, fixture, wait_for};

struct Node {
    url: String,
    engine: Engine,
    stop: watch::Sender<bool>,
    server: tokio::task::JoinHandle<()>,
}

async fn start(
    fx: &Fixture,
    name: &str,
    peers: Vec<String>,
) -> Result<Node, Box<dyn std::error::Error>> {
    let engine = Engine::start(fx.config()?).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let (stop, shutdown) = watch::channel(false);
    let nodes = Nodes::new(NodesConfig {
        local: name.to_owned(),
        tailscale: None,
        peers,
        // En los tests el «Mac» es el de la GPU: así el reparto tiene que salir de casa
        gpu: name == "mac",
    })?;
    let router = api::router(AppState {
        engine: engine.clone(),
        node: name.to_owned(),
        shutdown: shutdown.clone(),
        hosts: HostPolicy::default(),
        repos_root: fx.dir.path().to_path_buf(),
        nodes,
    });
    let server = tokio::spawn(async move {
        let mut shutdown = shutdown;
        let stopping = async move {
            if shutdown.wait_for(|stopping| *stopping).await.is_err() {
                std::future::pending::<()>().await;
            }
        };
        if let Err(err) = axum::serve(listener, router)
            .with_graceful_shutdown(stopping)
            .await
        {
            eprintln!("test node stopped: {err}");
        }
    });
    Ok(Node {
        url,
        engine,
        stop,
        server,
    })
}

async fn send(
    method: reqwest::Method,
    url: &str,
    body: Option<Value>,
) -> Result<(u16, Value), Box<dyn std::error::Error>> {
    // Ya construido un `Nodes`, hay proveedor de TLS para este cliente
    let client = reqwest::Client::new();
    let mut request = client.request(method, url).timeout(Duration::from_secs(20));
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await?;
    let status = response.status().as_u16();
    let text = response.text().await?;
    Ok((
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    ))
}

#[tokio::test(flavor = "multi_thread")]
async fn one_node_drives_another_through_its_api() -> TestResult {
    let mac_fx = fixture()?;
    let mac = start(&mac_fx, "mac", Vec::new()).await?;
    let pc_fx = fixture()?;
    let pc = start(&pc_fx, "pc", vec![mac.url.clone()]).await?;

    // El PC ve al Mac
    let (status, body) = send(reqwest::Method::GET, &format!("{}/v1/nodes", pc.url), None).await?;
    assert_eq!(status, 200, "{body}");
    let nodes: Vec<NodeInfo> = serde_json::from_value(body)?;
    let names: Vec<(&str, bool, bool)> = nodes
        .iter()
        .map(|node| (node.name.as_str(), node.local, node.online))
        .collect();
    assert_eq!(names, [("pc", true, true), ("mac", false, true)]);

    // Crear en el Mac desde el PC, con el repo por nombre: se resuelve allí
    let mut mac_events = mac.engine.subscribe();
    let (status, task) = send(
        reqwest::Method::POST,
        &format!("{}/v1/tasks", pc.url),
        Some(json!({ "repo": "app", "prompt": "remote work", "agent": { "kind": "claude" }, "node": "mac" })),
    )
    .await?;
    assert_eq!(status, 201, "{task}");
    assert_eq!(task["repo"], json!(mac_fx.checkout));
    let id = TaskId::parse(task["id"].as_str().ok_or("no id")?)?;
    wait_for(&mut mac_events, &id, TaskState::Ready).await?;
    assert!(
        pc.engine.list().await?.is_empty(),
        "the task runs on the mac only"
    );

    // Verla e integrarla por el reenvío
    let (status, listed) = send(
        reqwest::Method::GET,
        &format!("{}/v1/nodes/mac/tasks", pc.url),
        None,
    )
    .await?;
    assert_eq!(status, 200);
    assert_eq!(listed[0]["id"], json!(id));
    let (status, diff) = send(
        reqwest::Method::GET,
        &format!("{}/v1/nodes/mac/tasks/{id}/diff", pc.url),
        None,
    )
    .await?;
    assert_eq!(status, 200);
    assert!(
        diff.as_str().is_some_and(|d| d.contains("work.txt")),
        "{diff}"
    );
    let (status, _) = send(
        reqwest::Method::POST,
        &format!("{}/v1/nodes/mac/tasks/{id}/merge", pc.url),
        None,
    )
    .await?;
    assert_eq!(status, 202);
    wait_for(&mut mac_events, &id, TaskState::Merged).await?;
    assert!(mac_fx.origin_has("work.txt")?);

    // Los eventos del Mac llegan en directo a través del PC
    let response = reqwest::Client::new()
        .get(format!("{}/v1/nodes/mac/events", pc.url))
        .send()
        .await?;
    assert_eq!(response.status().as_u16(), 200);
    assert!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"))
    );
    drop(response);

    // El propio nodo se atiende sin red; uno que no existe es 404
    let (status, health) = send(
        reqwest::Method::GET,
        &format!("{}/v1/nodes/pc/health", pc.url),
        None,
    )
    .await?;
    assert_eq!((status, health["node"].clone()), (200, json!("pc")));
    let (status, body) = send(
        reqwest::Method::GET,
        &format!("{}/v1/nodes/nas/tasks", pc.url),
        None,
    )
    .await?;
    assert_eq!(
        (status, body["error"]["code"].clone()),
        (404, json!("unknown_node"))
    );

    // El Mac se duerme: fuera de línea, no desaparece
    mac.stop.send_replace(true);
    mac.server.await?;
    let (status, body) = send(
        reqwest::Method::GET,
        &format!("{}/v1/nodes/mac/tasks", pc.url),
        None,
    )
    .await?;
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"]["code"], json!("node_offline"));
    tokio::time::sleep(Duration::from_secs(6)).await;
    let (_, body) = send(reqwest::Method::GET, &format!("{}/v1/nodes", pc.url), None).await?;
    let nodes: Vec<NodeInfo> = serde_json::from_value(body)?;
    assert!(nodes.iter().any(|node| node.name == "mac" && !node.online));
    let (status, body) = send(
        reqwest::Method::POST,
        &format!("{}/v1/tasks", pc.url),
        Some(json!({ "repo": "app", "prompt": "x", "agent": { "kind": "claude" }, "node": "mac" })),
    )
    .await?;
    assert_eq!(
        (status, body["error"]["code"].clone()),
        (503, json!("node_offline"))
    );

    pc.stop.send_replace(true);
    pc.server.await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_places_tasks_by_need_and_load() -> TestResult {
    let mac_fx = fixture()?;
    let mac = start(&mac_fx, "mac", Vec::new()).await?;
    let pc_fx = fixture()?;
    let pc = start(&pc_fx, "pc", vec![mac.url.clone()]).await?;
    let create = |agent: Value| {
        let url = format!("{}/v1/tasks", pc.url);
        async move {
            send(
                reqwest::Method::POST,
                &url,
                Some(json!({ "repo": "app", "prompt": "auto", "agent": agent, "node": "auto" })),
            )
            .await
        }
    };

    // Ollama, al nodo con GPU
    let (status, task) = create(json!({ "kind": "ollama", "model": "qwen3-coder:30b" })).await?;
    assert_eq!(status, 201, "{task}");
    assert_eq!(task["repo"], json!(mac_fx.checkout));
    let id = TaskId::parse(task["id"].as_str().ok_or("no id")?)?;
    assert!(mac.engine.get(&id).await.is_ok());

    // Lo demás, al menos ocupado: el Mac aún tiene la de Ollama en marcha o acaba de
    // terminarla; a igualdad se queda en casa, así que nunca va al Mac
    let (status, task) = create(json!({ "kind": "claude" })).await?;
    assert_eq!(status, 201, "{task}");
    assert_eq!(task["repo"], json!(pc_fx.checkout));

    // Un proyecto de Xcode sin ningún macOS en línea (aquí todo es Linux) no se lanza
    std::fs::create_dir_all(pc_fx.checkout.join("App.xcodeproj"))?;
    let (status, body) = create(json!({ "kind": "claude" })).await?;
    if std::env::consts::OS == "macos" {
        assert_eq!(status, 201, "{body}");
    } else {
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["error"]["code"], json!("no_capable_node"));
    }

    mac.stop.send_replace(true);
    pc.stop.send_replace(true);
    mac.server.await?;
    pc.server.await?;
    Ok(())
}
