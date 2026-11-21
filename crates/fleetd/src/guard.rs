//! Defensa del demonio frente al navegador. `fleetd` solo escucha en loopback, pero cualquier
//! web abierta en el navegador de Juan puede mandar peticiones a `127.0.0.1:7171`, y crear una
//! tarea es lanzar un agente sin permisos que pedir. Dos comprobaciones:
//! - `Host` conocido: corta el DNS rebinding (un dominio ajeno que resuelve a 127.0.0.1).
//! - Peticiones que cambian algo con un `Origin` distinto del propio: corta el CSRF.
//!
//! Sin `Host` ni `Origin` no hay navegador por medio (la CLI, los tests): se deja pasar.

use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// Qué nombres de máquina se aceptan en `Host`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostPolicy {
    /// Además de loopback y `*.ts.net` (MagicDNS de Tailscale): nombres sin puerto, en
    /// minúsculas, p. ej. el nombre corto de la máquina en la tailnet.
    pub extra: Vec<String>,
}

impl HostPolicy {
    /// `host` es el valor de la cabecera, con o sin puerto.
    #[must_use]
    pub fn allows(&self, host: &str) -> bool {
        let name = host_name(host).to_ascii_lowercase();
        if name.is_empty() {
            return false;
        }
        name == "localhost"
            || name.ends_with(".localhost")
            || name.ends_with(".ts.net")
            || name
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
            || self.extra.iter().any(|extra| extra == &name)
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
    if let Some(host) = host
        && !policy.allows(host)
    {
        return Some(format!(
            "host `{host}` is not allowed; pass --allow-host to fleetd"
        ));
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

    #[test]
    fn loopback_tailnet_and_extra_hosts_are_allowed() {
        let policy = HostPolicy {
            extra: vec!["pc".to_owned()],
        };
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
        let policy = HostPolicy::default();
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
}
