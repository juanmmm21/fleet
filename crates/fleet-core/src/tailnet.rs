//! Lo que fleet necesita de `tailscale status --json` (ADR 0002): quién es esta máquina, de
//! quién es y qué otras máquinas podrían tener un `fleetd`. Aquí solo se analiza el JSON; lanzar
//! `tailscale` y preguntar a los pares es cosa de `fleetd`.

use std::collections::HashMap;

use serde::Deserialize;

/// Sistemas en los que corre `fleetd` (Linux y macOS, ver `lib.rs`), con el nombre que les
/// da Tailscale.
const FLEET_OSES: [&str; 2] = ["linux", "macOS"];

#[derive(Debug, thiserror::Error)]
pub enum TailnetError {
    #[error("cannot read `tailscale status --json`: {0}")]
    Json(#[from] serde_json::Error),
    #[error("tailscale is not running here (backend state `{0}`)")]
    NotRunning(String),
}

/// Una máquina de la tailnet, ya limpia.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    /// Nombre de la máquina según su sistema (`Excalibur`).
    pub host_name: String,
    /// Nombre de MagicDNS sin el punto final (`pc.tail1234.ts.net`); vacío si no tiene.
    pub dns_name: String,
    /// `linux`, `macOS`, `windows`, `iOS`…
    pub os: String,
    pub online: bool,
    /// Las máquinas etiquetadas no son de una persona: `tailscale serve` no les pone identidad.
    pub tagged: bool,
    /// Login del dueño (`juan@…`), si la tailnet lo cuenta.
    pub login: Option<String>,
}

impl Machine {
    /// URL con la que se llega a su `fleetd` a través de `tailscale serve`.
    #[must_use]
    pub fn url(&self) -> Option<String> {
        (!self.dns_name.is_empty()).then(|| format!("https://{}", self.dns_name))
    }
}

/// El estado de la tailnet visto desde esta máquina.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailnetStatus {
    pub this: Machine,
    pub peers: Vec<Machine>,
}

impl TailnetStatus {
    /// Analiza la salida de `tailscale status --json`. Con Tailscale parado o sin sesión no
    /// hay tailnet que mirar: es un error, y `fleetd` sigue como un solo nodo.
    pub fn parse(json: &[u8]) -> Result<Self, TailnetError> {
        let raw: RawStatus = serde_json::from_slice(json)?;
        if raw.backend_state != "Running" {
            return Err(TailnetError::NotRunning(raw.backend_state));
        }
        let users = raw.users;
        let machine = |node: RawNode| node.into_machine(&users);
        let mut peers: Vec<Machine> = raw.peers.into_values().map(machine).collect();
        // El JSON es un mapa: se ordena para que la lista sea estable entre vueltas
        peers.sort_by(|a, b| a.dns_name.cmp(&b.dns_name));
        Ok(Self {
            this: machine(raw.this),
            peers,
        })
    }

    /// Pares a los que merece la pena preguntar por `fleetd`: en línea, del mismo dueño que
    /// esta máquina, sin etiquetas, con un sistema donde corre fleet y con nombre de MagicDNS.
    #[must_use]
    pub fn candidates(&self) -> Vec<&Machine> {
        let Some(owner) = &self.this.login else {
            return Vec::new();
        };
        self.peers
            .iter()
            .filter(|peer| {
                peer.online
                    && !peer.tagged
                    && peer.login.as_ref() == Some(owner)
                    && FLEET_OSES.contains(&peer.os.as_str())
                    && !peer.dns_name.is_empty()
            })
            .collect()
    }
}

#[derive(Debug, Deserialize)]
struct RawStatus {
    #[serde(rename = "BackendState", default)]
    backend_state: String,
    #[serde(rename = "Self")]
    this: RawNode,
    #[serde(rename = "Peer", default)]
    peers: HashMap<String, RawNode>,
    #[serde(rename = "User", default)]
    users: HashMap<String, RawUser>,
}

#[derive(Debug, Deserialize)]
struct RawNode {
    #[serde(rename = "HostName", default)]
    host_name: String,
    #[serde(rename = "DNSName", default)]
    dns_name: String,
    #[serde(rename = "OS", default)]
    os: String,
    #[serde(rename = "Online", default)]
    online: bool,
    #[serde(rename = "UserID", default)]
    user_id: u64,
    #[serde(rename = "Tags", default)]
    tags: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct RawUser {
    #[serde(rename = "LoginName", default)]
    login_name: String,
}

impl RawNode {
    fn into_machine(self, users: &HashMap<String, RawUser>) -> Machine {
        let login = users
            .get(&self.user_id.to_string())
            .map(|user| user.login_name.clone())
            .filter(|login| !login.is_empty());
        Machine {
            host_name: self.host_name,
            dns_name: self.dns_name.trim_end_matches('.').to_owned(),
            os: self.os,
            online: self.online,
            tagged: self.tags.is_some_and(|tags| !tags.is_empty()),
            login,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recortado de un `tailscale status --json` real (1.104), con otros nombres.
    const STATUS: &str = r#"{
      "Version": "1.104.7",
      "BackendState": "Running",
      "Self": {"HostName": "pc", "DNSName": "pc-wsl.tail1234.ts.net.", "OS": "linux",
               "Online": true, "UserID": 42, "Tags": null, "TailscaleIPs": ["100.64.0.1"]},
      "Peer": {
        "nodekey:3": {"HostName": "localhost", "DNSName": "phone.tail1234.ts.net.", "OS": "iOS",
                      "Online": true, "UserID": 42},
        "nodekey:1": {"HostName": "Mac", "DNSName": "mac.tail1234.ts.net.", "OS": "macOS",
                      "Online": true, "UserID": 42, "Tags": null},
        "nodekey:2": {"HostName": "win", "DNSName": "win.tail1234.ts.net.", "OS": "windows",
                      "Online": true, "UserID": 42},
        "nodekey:4": {"HostName": "builder", "DNSName": "builder.tail1234.ts.net.", "OS": "linux",
                      "Online": true, "UserID": 7, "Tags": ["tag:ci"]},
        "nodekey:5": {"HostName": "friend", "DNSName": "friend.tail1234.ts.net.", "OS": "linux",
                      "Online": true, "UserID": 9},
        "nodekey:6": {"HostName": "old", "DNSName": "old.tail1234.ts.net.", "OS": "linux",
                      "Online": false, "UserID": 42}
      },
      "User": {
        "42": {"ID": 42, "LoginName": "owner@example.com"},
        "9": {"ID": 9, "LoginName": "friend@example.com"},
        "7": {"ID": 7, "LoginName": "tagged-devices"}
      },
      "MagicDNSSuffix": "tail1234.ts.net"
    }"#;

    #[test]
    fn reads_this_machine_and_its_owner() -> Result<(), TailnetError> {
        let status = TailnetStatus::parse(STATUS.as_bytes())?;
        assert_eq!(status.this.host_name, "pc");
        assert_eq!(status.this.dns_name, "pc-wsl.tail1234.ts.net");
        assert_eq!(status.this.login.as_deref(), Some("owner@example.com"));
        assert_eq!(
            status.this.url().as_deref(),
            Some("https://pc-wsl.tail1234.ts.net")
        );
        assert_eq!(status.peers.len(), 6);
        assert!(status.peers.iter().any(|peer| peer.tagged));
        Ok(())
    }

    #[test]
    fn only_online_untagged_linux_and_mac_peers_of_the_same_owner_are_candidates()
    -> Result<(), TailnetError> {
        let status = TailnetStatus::parse(STATUS.as_bytes())?;
        let names: Vec<&str> = status
            .candidates()
            .iter()
            .map(|peer| peer.host_name.as_str())
            .collect();
        assert_eq!(names, ["Mac"]);
        Ok(())
    }

    #[test]
    fn a_stopped_tailscale_or_bad_json_is_an_error() {
        let stopped = STATUS.replace("\"Running\"", "\"Stopped\"");
        assert!(matches!(
            TailnetStatus::parse(stopped.as_bytes()),
            Err(TailnetError::NotRunning(state)) if state == "Stopped"
        ));
        assert!(matches!(
            TailnetStatus::parse(b"not json"),
            Err(TailnetError::Json(_))
        ));
    }

    #[test]
    fn without_a_known_owner_nobody_is_trusted() -> Result<(), TailnetError> {
        let anonymous = STATUS.replace(
            "\"UserID\": 42, \"Tags\": null, \"TailscaleIPs\"",
            "\"UserID\": 1, \"TailscaleIPs\"",
        );
        let status = TailnetStatus::parse(anonymous.as_bytes())?;
        assert_eq!(status.this.login, None);
        assert!(status.candidates().is_empty());
        Ok(())
    }
}
