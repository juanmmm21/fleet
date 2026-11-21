//! Cliente de la API de `fleetd`. La CLI no toca git ni la base de datos: todo va por aquí.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::time::Duration;

use fleet_core::{AgentProvider, Overlap, Task, TaskId};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use ureq::Agent;

pub const DEFAULT_URL: &str = "http://127.0.0.1:7171";
/// Lo que se espera a abrir la conexión: el demonio está en esta máquina o en la tailnet.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Lo que puede tardar una respuesta normal (integrar espera a que acabe el trabajo previo).
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("cannot reach fleetd at {url} ({source}); start it with `fleetd`")]
    Unreachable {
        url: String,
        #[source]
        source: ureq::Error,
    },
    /// La API respondió con un error (`{"error": {"code", "message"}}`).
    #[error("{message} ({code})")]
    Api {
        status: u16,
        code: String,
        message: String,
    },
    #[error("unexpected answer from fleetd: {0}")]
    Protocol(String),
}

/// Lo que se manda para crear una tarea.
#[derive(Debug, Clone, Serialize)]
pub struct NewTask {
    pub repo: PathBuf,
    pub prompt: String,
    pub agent: AgentProvider,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
}

/// Un evento de `GET /v1/events`.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamEvent {
    pub name: String,
    pub data: Value,
}

pub struct Client {
    base: String,
    agent: Agent,
    /// Sin plazo de lectura: los eventos pueden tardar en llegar lo que tarde un agente.
    streaming: Agent,
}

impl Client {
    #[must_use]
    pub fn new(base_url: &str) -> Self {
        let base = base_url.trim_end_matches('/').to_owned();
        let build = |call: Option<Duration>| -> Agent {
            Agent::config_builder()
                .http_status_as_error(false)
                .timeout_connect(Some(CONNECT_TIMEOUT))
                .timeout_global(call)
                .build()
                .into()
        };
        Self {
            base,
            agent: build(Some(CALL_TIMEOUT)),
            streaming: build(None),
        }
    }

    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn unreachable(&self, source: ureq::Error) -> ClientError {
        ClientError::Unreachable {
            url: self.base.clone(),
            source,
        }
    }

    /// Convierte una respuesta en texto, o en el error de la API si no es 2xx.
    fn text(&self, mut response: ureq::http::Response<ureq::Body>) -> Result<String, ClientError> {
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|err| ClientError::Protocol(format!("cannot read the body: {err}")))?;
        if (200..300).contains(&status) {
            return Ok(body);
        }
        let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let field = |name: &str| {
            parsed
                .pointer(&format!("/error/{name}"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        Err(ClientError::Api {
            status,
            code: field("code").unwrap_or_else(|| "unknown".to_owned()),
            message: field("message").unwrap_or(body),
        })
    }

    fn get_text(&self, path: &str) -> Result<String, ClientError> {
        let response = self
            .agent
            .get(&self.url(path))
            .call()
            .map_err(|err| self.unreachable(err))?;
        self.text(response)
    }

    fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        decode(&self.get_text(path)?)
    }

    fn post_json<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, ClientError> {
        let response = self
            .agent
            .post(&self.url(path))
            .send_json(body)
            .map_err(|err| self.unreachable(err))?;
        decode(&self.text(response)?)
    }

    pub fn health(&self) -> Result<Value, ClientError> {
        self.get_json("/v1/health")
    }

    pub fn create(&self, task: &NewTask) -> Result<Task, ClientError> {
        self.post_json("/v1/tasks", task)
    }

    pub fn list(&self) -> Result<Vec<Task>, ClientError> {
        self.get_json("/v1/tasks")
    }

    pub fn get(&self, id: &TaskId) -> Result<Task, ClientError> {
        self.get_json(&format!("/v1/tasks/{id}"))
    }

    pub fn log(&self, id: &TaskId) -> Result<String, ClientError> {
        self.get_text(&format!("/v1/tasks/{id}/log"))
    }

    pub fn diff(&self, id: &TaskId) -> Result<String, ClientError> {
        self.get_text(&format!("/v1/tasks/{id}/diff"))
    }

    pub fn overlaps(&self) -> Result<Vec<Overlap>, ClientError> {
        self.get_json("/v1/overlaps")
    }

    pub fn answer(&self, id: &TaskId, answer: &str) -> Result<Task, ClientError> {
        self.post_json(
            &format!("/v1/tasks/{id}/answer"),
            &serde_json::json!({ "answer": answer }),
        )
    }

    pub fn merge(&self, id: &TaskId) -> Result<Task, ClientError> {
        self.post_json(&format!("/v1/tasks/{id}/merge"), &serde_json::json!({}))
    }

    pub fn drop_task(&self, id: &TaskId) -> Result<Task, ClientError> {
        self.post_json(&format!("/v1/tasks/{id}/drop"), &serde_json::json!({}))
    }

    pub fn cancel(&self, id: &TaskId) -> Result<(), ClientError> {
        let _: Value = self.post_json(&format!("/v1/tasks/{id}/cancel"), &serde_json::json!({}))?;
        Ok(())
    }

    /// Escucha `/v1/events`. Con el flujo ya abierto (el demonio se suscribe antes de mandar
    /// las cabeceras) llama a `on_open`, que sirve para mirar el estado sin perder eventos; luego
    /// pasa cada evento a `on_event`. Para en cuanto alguno devuelve `false` o se cierra el flujo.
    pub fn follow<O, F>(&self, on_open: O, mut on_event: F) -> Result<(), ClientError>
    where
        O: FnOnce() -> Result<bool, ClientError>,
        F: FnMut(StreamEvent) -> bool,
    {
        let response = self
            .streaming
            .get(&self.url("/v1/events"))
            .call()
            .map_err(|err| self.unreachable(err))?;
        if !response.status().is_success() {
            return self.text(response).map(|_| ());
        }
        if !on_open()? {
            return Ok(());
        }
        let reader = BufReader::new(response.into_body().into_reader());
        let mut name = String::new();
        let mut data = String::new();
        for line in reader.lines() {
            let line = line.map_err(|err| ClientError::Protocol(format!("event stream: {err}")))?;
            if line.is_empty() {
                if !data.is_empty() {
                    let event = StreamEvent {
                        name: std::mem::take(&mut name),
                        data: serde_json::from_str(&data).unwrap_or(Value::String(data.clone())),
                    };
                    data.clear();
                    if !on_event(event) {
                        return Ok(());
                    }
                }
                name.clear();
            } else if let Some(value) = line.strip_prefix("event:") {
                name = value.trim().to_owned();
            } else if let Some(value) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(value.strip_prefix(' ').unwrap_or(value));
            }
        }
        Ok(())
    }
}

fn decode<T: DeserializeOwned>(text: &str) -> Result<T, ClientError> {
    serde_json::from_str(text).map_err(|err| ClientError::Protocol(err.to_string()))
}
