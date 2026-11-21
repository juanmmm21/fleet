//! Web mínima de `fleetd`: un HTML con su JS y CSS embebidos en el binario, sin build de
//! JavaScript. Habla con la misma API que la CLI y usa rutas relativas para funcionar igual
//! en `127.0.0.1` que detrás de `tailscale serve`.

use axum::Router;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const APP_CSS: &str = include_str!("../web/app.css");

/// Sin scripts ni estilos en línea y sin nada de fuera: si algún texto de una tarea (salida de
/// un agente, un diff) se colara como HTML, el navegador no lo ejecutaría.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; \
                   img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

pub fn router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route(
            "/",
            get(|| async { asset(INDEX_HTML, "text/html; charset=utf-8") }),
        )
        .route(
            "/app.js",
            get(|| async { asset(APP_JS, "text/javascript; charset=utf-8") }),
        )
        .route(
            "/app.css",
            get(|| async { asset(APP_CSS, "text/css; charset=utf-8") }),
        )
}

fn asset(body: &'static str, content_type: &'static str) -> Response {
    let mut response = body.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    // Va en el binario: tras actualizar fleetd, la web nueva tiene que verse sin limpiar caché
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}
