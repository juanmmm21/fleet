//! Defensa del demonio frente al navegador. `fleetd` solo escucha en loopback, pero cualquier
//! web abierta en el navegador de Juan puede mandar peticiones a `127.0.0.1:7171`, y crear una
//! tarea es lanzar un agente sin permisos que pedir. Dos comprobaciones:
//! - `Host` conocido: corta el DNS rebinding (un dominio ajeno que resuelve a 127.0.0.1).
//! - Peticiones que cambian algo con un `Origin` distinto del propio: corta el CSRF.
//!
//! Sin `Host` ni `Origin` no hay navegador por medio (la CLI, los tests): se deja pasar.
//!
//! Lo que llega por la tailnet (a través de `tailscale serve`) además tiene que traer la
//! identidad que pone Tailscale y ser del dueño de la máquina (ADR 0002): otro usuario de la
//! tailnet o un dispositivo etiquetado no lanza agentes aquí.

use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// Cabecera que pone `tailscale serve` con el login de quien llama (y que quita si la trae el
/// cliente, así que no se puede falsificar desde la tailnet).
pub const TAILSCALE_LOGIN: &str = "tailscale-user-login";

/// Qué nombres de máquina se aceptan en `Host` y quién puede entrar por la tailnet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostPolicy {
    /// Además de loopback y `*.ts.net` (MagicDNS de Tailscale): nombres sin puerto, en
    /// minúsculas, p. ej. el nombre corto de la máquina en la tailnet.
    pub extra: Vec<String>,
    /// Logins de Tailscale que pueden usar este `fleetd` por la tailnet: el dueño de la
    /// máquina y los de `--allow-login`. Vacío: por la tailnet no entra nadie.
    pub logins: Vec<String>,
}

/// Por dónde dice venir una petición según su `Host`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKind {
    Loopback,
    Tailnet,
}

impl HostPolicy {
    /// `host` es el valor de la cabecera, con o sin puerto; `None` si no se acepta.
    #[must_use]
    pub fn classify(&self, host: &str) -> Option<HostKind> {
        let name = host_name(host).to_ascii_lowercase();
        if name.is_empty() {
            return None;
        }
        let loopback = name == "localhost"
            || name.ends_with(".localhost")
            || name
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        if loopback {
            Some(HostKind::Loopback)
        } else if name.ends_with(".ts.net") || self.extra.iter().any(|extra| extra == &name) {
            Some(HostKind::Tailnet)
        } else {
            None
        }
    }

    #[must_use]
    pub fn allows(&self, host: &str) -> bool {
        self.classify(host).is_some()
    }
}

/// Quita el puerto de `host[:puerto]` o `[ipv6][:puerto]`.
fn host_name(host: &str) -> &str {
    let host = host.trim();
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split_once(']').map_or(host, |(inside, _)| inside);
    }
    host.rsplit_once(':')
        .filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit()))
        .map_or(host, |(name, _)| name)
}

/// `host[:puerto]` de un `Origin` como `https://maquina.tailnet.ts.net`.
fn origin_authority(origin: &str) -> Option<&str> {
    let (_, rest) = origin.split_once("://")?;
    Some(rest.split('/').next().unwrap_or(rest))
}

fn header_str<'a>(headers: &'a HeaderMap, name: &header::HeaderName) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// Por qué se rechaza una petición, o `None` si pasa.
#[must_use]
pub fn check(policy: &HostPolicy, method: &Method, headers: &HeaderMap) -> Option<String> {
    let host = header_str(headers, &header::HOST);
    let kind = match host {
        Some(host) => match policy.classify(host) {
            Some(kind) => Some(kind),
            None => {
                return Some(format!(
                    "host `{host}` is not allowed; pass --allow-host to fleetd"
                ));
            }
        },
        None => None,
    };
    // Si el proxy no conservara `Host`, lo delata que traiga X-Forwarded-For o la identidad
    let proxied = headers.contains_key("x-forwarded-for") || headers.contains_key(TAILSCALE_LOGIN);
    if kind == Some(HostKind::Tailnet) || proxied {
        match headers.get(TAILSCALE_LOGIN).and_then(|v| v.to_str().ok()) {
            Some(login) if policy.logins.iter().any(|allowed| allowed == login) => {}
            Some(login) => {
                return Some(format!(
                    "tailnet user `{login}` is not allowed; pass --allow-login to fleetd"
                ));
            }
            None => {
                return Some(
                    "requests over the tailnet must come through `tailscale serve` from a \
                     device of an allowed user"
                        .to_owned(),
                );
            }
        }
    }
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return None;
    }
    let origin = header_str(headers, &header::ORIGIN)?;
    let Some(authority) = origin_authority(origin) else {
        // `Origin: null` (iframes con sandbox, ficheros locales) nunca es la web de fleet
        return Some(format!("requests from origin `{origin}` are not allowed"));
    };
    // Detrás de un proxy (`tailscale serve`) el nombre público puede ir en X-Forwarded-Host
    let forwarded = headers
        .get("x-forwarded-host")
        .and_then(|value| value.to_str().ok());
    let same = [host, forwarded]
        .into_iter()
        .flatten()
        .any(|own| own.trim().eq_ignore_ascii_case(authority));
    if same {
        None
    } else {
        Some(format!("cross-origin request from `{origin}` refused"))
    }
}

/// Capa de `axum` que aplica [`check`].
pub async fn layer(State(policy): State<HostPolicy>, request: Request, next: Next) -> Response {
    match check(&policy, request.method(), request.headers()) {
        None => next.run(request).await,
        Some(message) => {
            let body = json!({ "error": { "code": "forbidden", "message": message } });
            (StatusCode::FORBIDDEN, axum::Json(body)).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_static(value));
        }
        map
    }

    const OWNER: &str = "owner@example.com";

    fn owner_policy() -> HostPolicy {
        HostPolicy {
            extra: Vec::new(),
            logins: vec![OWNER.to_owned()],
        }
    }

    #[test]
    fn loopback_tailnet_and_extra_hosts_are_allowed() {
        let policy = HostPolicy {
            extra: vec!["pc".to_owned()],
            logins: Vec::new(),
        };
        assert_eq!(policy.classify("127.0.0.1:7171"), Some(HostKind::Loopback));
        assert_eq!(policy.classify("pc:7171"), Some(HostKind::Tailnet));
        assert_eq!(
            policy.classify("pc.tail1234.ts.net"),
            Some(HostKind::Tailnet)
        );
        for host in [
            "127.0.0.1:7171",
            "localhost:7171",
            "LOCALHOST",
            "[::1]:7171",
            "127.0.0.2",
            "pc.tail1234.ts.net",
            "pc:7171",
        ] {
            assert!(policy.allows(host), "{host}");
        }
        for host in [
            "evil.example",
            "evil.example:7171",
            "192.168.1.10",
            "ts.net.evil.example",
            "",
        ] {
            assert!(!policy.allows(host), "{host}");
        }
    }

    #[test]
    fn reads_are_only_checked_for_the_host() {
        let policy = HostPolicy::default();
        assert_eq!(check(&policy, &Method::GET, &HeaderMap::new()), None);
        assert_eq!(
            check(
                &policy,
                &Method::GET,
                &headers(&[
                    ("host", "127.0.0.1:7171"),
                    ("origin", "https://evil.example")
                ])
            ),
            None
        );
        assert!(
            check(
                &policy,
                &Method::GET,
                &headers(&[("host", "rebind.evil.example:7171")])
            )
            .is_some()
        );
    }

    #[test]
    fn writes_need_the_same_origin() {
        let policy = owner_policy();
        let post = Method::POST;
        assert_eq!(
            check(&policy, &post, &headers(&[("host", "127.0.0.1:7171")])),
            None
        );
        assert_eq!(
            check(
                &policy,
                &post,
                &headers(&[
                    ("host", "127.0.0.1:7171"),
                    ("origin", "http://127.0.0.1:7171")
                ])
            ),
            None
        );
        assert_eq!(
            check(
                &policy,
                &post,
                &headers(&[
                    ("host", "127.0.0.1:7171"),
                    ("x-forwarded-host", "pc.tail1234.ts.net"),
                    ("tailscale-user-login", OWNER),
                    ("origin", "https://pc.tail1234.ts.net")
                ])
            ),
            None
        );
        for origin in ["https://evil.example", "http://localhost:7171", "null"] {
            let map = {
                let mut map = headers(&[("host", "127.0.0.1:7171")]);
                map.insert("origin", HeaderValue::from_static(origin));
                map
            };
            assert!(check(&policy, &post, &map).is_some(), "{origin}");
        }
    }

    #[test]
    fn the_tailnet_needs_the_owner_identity() {
        let policy = owner_policy();
        let get = Method::GET;
        let via_serve = |login: Option<&'static str>| {
            let mut map = headers(&[("host", "pc.tail1234.ts.net")]);
            if let Some(login) = login {
                map.insert(TAILSCALE_LOGIN, HeaderValue::from_static(login));
            }
            map
        };
        assert_eq!(check(&policy, &get, &via_serve(Some(OWNER))), None);
        // Sin identidad: un dispositivo etiquetado o algo que no pasó por `tailscale serve`
        assert!(check(&policy, &get, &via_serve(None)).is_some());
        assert!(check(&policy, &get, &via_serve(Some("friend@example.com"))).is_some());
        // Nadie de la tailnet si no se sabe de quién es la máquina
        assert!(check(&HostPolicy::default(), &get, &via_serve(Some(OWNER))).is_some());
        // Un proxy que reescribiera `Host` a loopback no se salta la comprobación
        let rewritten = headers(&[
            ("host", "127.0.0.1:7171"),
            ("x-forwarded-for", "100.64.0.2"),
        ]);
        assert!(check(&policy, &get, &rewritten).is_some());
        // En loopback, sin proxy por medio, como siempre
        assert_eq!(
            check(&policy, &get, &headers(&[("host", "localhost:7171")])),
            None
        );
    }
}
