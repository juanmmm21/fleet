//! Los otros `fleetd` de la tailnet (ADR 0002): se descubren con `tailscale status`, se
//! confirman con su `/v1/health` y se recuerdan para poder decir que el Mac está dormido en vez
//! de olvidarlo.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, watch};

use crate::tailnet::Tailscale;

/// Cada cuánto se mira la tailnet sin que nadie lo pida.
pub const REFRESH_EVERY: Duration = Duration::from_secs(30);
/// Una lista más vieja que esto se rehace al pedirla.
const FRESH_FOR: Duration = Duration::from_secs(5);
/// Lo que se espera a un `/v1/health`: un nodo que tarda más está, a efectos prácticos, fuera.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Un nodo tal como lo cuenta `GET /v1/nodes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeInfo {
    /// El nombre que da su `/v1/health`.
    pub name: String,
    /// Base de su API (sin `/v1`); vacía para este nodo, al que se llega sin red.
    pub url: String,
    pub version: String,
    /// `linux` o `macos`.
    #[serde(default)]
    pub os: String,
    /// Tiene GPU para Ollama (`fleetd --gpu`).
    #[serde(default)]
    pub gpu: bool,
    /// Tareas en marcha cuando se le preguntó por última vez.
    #[serde(default)]
    pub live_tasks: usize,
    pub online: bool,
    /// Este mismo `fleetd`.
    pub local: bool,
    /// Última vez que respondió, en milisegundos Unix.
    pub last_seen_ms: Option<u64>,
}

/// Lo que responde `/v1/health`; los campos nuevos con valor por defecto para hablar con un
/// `fleetd` más viejo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    pub node: String,
    pub version: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub gpu: bool,
    #[serde(default)]
    pub live_tasks: usize,
}

/// Cómo se buscan los nodos.
#[derive(Debug, Clone)]
pub struct NodesConfig {
    /// Nombre de este nodo.
    pub local: String,
    /// `None`: no se mira la tailnet (tests, máquinas sin Tailscale).
    pub tailscale: Option<Tailscale>,
    /// URLs de `--peer`, que se sondean siempre.
    pub peers: Vec<String>,
    /// Esta máquina tiene GPU para Ollama.
    pub gpu: bool,
}

#[derive(Debug, Default)]
struct Known {
    /// Por nombre de nodo. Los que dejan de responder se quedan, fuera de línea.
    nodes: BTreeMap<String, NodeInfo>,
    refreshed: Option<Instant>,
}

/// Registro de nodos, clonable y compartido por los manejadores.
#[derive(Debug, Clone)]
pub struct Nodes {
    config: Arc<NodesConfig>,
    client: reqwest::Client,
    known: Arc<Mutex<Known>>,
}

#[derive(Debug, thiserror::Error)]
pub enum NodesError {
    #[error("cannot build the HTTP client: {0}")]
    Client(#[from] reqwest::Error),
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// reqwest va sin proveedor de criptografía (el suyo, aws-lc, pide cmake para compilar): se
/// pone ring, una vez por proceso, antes de crear el primer cliente.
fn ensure_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        // Solo falla si otro hilo lo instaló entre medias, y entonces ya hay proveedor
        if let Err(other) = rustls::crypto::ring::default_provider().install_default() {
            drop(other);
        }
    }
}

impl Nodes {
    pub fn new(config: NodesConfig) -> Result<Self, NodesError> {
        ensure_crypto_provider();
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            // Los nodos se hablan por su nombre de MagicDNS; nada de proxies del sistema
            .no_proxy()
            .build()?;
        Ok(Self {
            config: Arc::new(config),
            client,
            known: Arc::new(Mutex::new(Known::default())),
        })
    }

    /// Un registro sin tailnet ni pares: solo este nodo.
    pub fn local_only(name: impl Into<String>) -> Result<Self, NodesError> {
        Self::new(NodesConfig {
            local: name.into(),
            tailscale: None,
            peers: Vec::new(),
            gpu: false,
        })
    }

    #[must_use]
    pub fn local_name(&self) -> &str {
        &self.config.local
    }

    /// El cliente HTTP con el que se habla con los otros nodos.
    #[must_use]
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Si esta máquina tiene GPU para Ollama.
    #[must_use]
    pub fn local_gpu(&self) -> bool {
        self.config.gpu
    }

    /// Este nodo; `live_tasks` lo pone quien tiene el motor.
    fn local_info(&self) -> NodeInfo {
        NodeInfo {
            name: self.config.local.clone(),
            url: String::new(),
            version: fleet_core::VERSION.to_owned(),
            os: std::env::consts::OS.to_owned(),
            gpu: self.config.gpu,
            live_tasks: 0,
            online: true,
            local: true,
            last_seen_ms: Some(now_ms()),
        }
    }

    /// Este nodo primero y luego los demás por nombre; se rehace si tiene más de 5 s.
    pub async fn list(&self) -> Vec<NodeInfo> {
        let stale = {
            let known = self.known.lock().await;
            known
                .refreshed
                .is_none_or(|refreshed| refreshed.elapsed() > FRESH_FOR)
        };
        if stale {
            self.refresh().await;
        }
        let known = self.known.lock().await;
        std::iter::once(self.local_info())
            .chain(known.nodes.values().cloned())
            .collect()
    }

    /// El nodo con ese nombre, si se conoce (en línea o no).
    pub async fn find(&self, name: &str) -> Option<NodeInfo> {
        if name == self.config.local {
            return Some(self.local_info());
        }
        if let Some(node) = self.known.lock().await.nodes.get(name).cloned() {
            return Some(node);
        }
        // Quizá acaba de aparecer: una vuelta más antes de decir que no existe
        self.refresh().await;
        self.known.lock().await.nodes.get(name).cloned()
    }

    /// Mira la tailnet y sondea cada candidato y cada `--peer` a la vez.
    pub async fn refresh(&self) {
        let mut urls = self.config.peers.clone();
        if let Some(tailscale) = &self.config.tailscale {
            match tailscale.status().await {
                Ok(status) => urls.extend(status.candidates().iter().filter_map(|m| m.url())),
                Err(err) => eprintln!("fleetd: cannot read the tailnet: {err}"),
            }
        }
        urls.sort();
        urls.dedup();
        let probes = urls.iter().map(|url| self.probe(url));
        let answers = futures_util::future::join_all(probes).await;
        let seen_at = now_ms();

        let mut known = self.known.lock().await;
        for node in known.nodes.values_mut() {
            node.online = false;
        }
        for (url, health) in urls.into_iter().zip(answers) {
            let Some(health) = health else { continue };
            // Un `--peer` que resulta ser este mismo nodo no cuenta dos veces
            if health.node == self.config.local {
                continue;
            }
            known.nodes.insert(
                health.node.clone(),
                NodeInfo {
                    name: health.node,
                    url,
                    version: health.version,
                    os: health.os,
                    gpu: health.gpu,
                    live_tasks: health.live_tasks,
                    online: true,
                    local: false,
                    last_seen_ms: Some(seen_at),
                },
            );
        }
        known.refreshed = Some(Instant::now());
    }

    async fn probe(&self, url: &str) -> Option<Health> {
        let response = self
            .client
            .get(format!("{url}/v1/health"))
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        // Algo que responde pero no es fleet (otro servicio tras `tailscale serve`) no es un nodo
        response.json::<Health>().await.ok()
    }

    /// Mira la tailnet cada [`REFRESH_EVERY`] hasta el apagado.
    pub fn spawn_refresher(&self, mut shutdown: watch::Receiver<bool>) {
        let nodes = self.clone();
        tokio::spawn(async move {
            loop {
                nodes.refresh().await;
                tokio::select! {
                    () = tokio::time::sleep(REFRESH_EVERY) => {}
                    () = crate::api::stopping(&mut shutdown) => return,
                }
            }
        });
    }
}
