use axum::{
    body::Body,
    http::{Response, StatusCode, header},
    response::Json,
};
use serde_json::json;

const SECURITY_HEADERS: &[(&str, &str)] = &[
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "no-referrer"),
    (
        "content-security-policy",
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self' ws: wss:; img-src 'self' data:; font-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'",
    ),
];

fn response(content_type: &'static str, body: &'static str) -> Response<Body> {
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-cache");
    for (name, value) in SECURITY_HEADERS {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::from(body)).expect("static response")
}

pub async fn index_handler() -> Response<Body> {
    response("text/html; charset=utf-8", INDEX_HTML)
}

pub async fn app_js_handler() -> Response<Body> {
    response("text/javascript; charset=utf-8", APP_JS)
}

pub async fn app_css_handler() -> Response<Body> {
    response("text/css; charset=utf-8", APP_CSS)
}

pub async fn proto_style_handler() -> Response<Body> {
    response("text/css; charset=utf-8", PROTO_STYLE)
}

pub async fn proto_tokens_handler() -> Response<Body> {
    response("text/css; charset=utf-8", PROTO_TOKENS)
}

pub async fn shadcn_theme_handler() -> Response<Body> {
    response("text/css; charset=utf-8", SHADCN_THEME)
}

pub async fn build_manifest_handler() -> Response<Body> {
    response("application/json; charset=utf-8", BUILD_MANIFEST)
}

pub async fn proto_license_handler() -> Response<Body> {
    response("text/plain; charset=utf-8", PROTO_LICENSE)
}

pub async fn shadcn_notices_handler() -> Response<Body> {
    response("text/plain; charset=utf-8", SHADCN_NOTICES)
}

pub async fn react_license_handler() -> Response<Body> {
    response("text/plain; charset=utf-8", REACT_LICENSE)
}

pub async fn react_dom_license_handler() -> Response<Body> {
    response("text/plain; charset=utf-8", REACT_DOM_LICENSE)
}

pub async fn health_handler() -> Json<serde_json::Value> {
    Json(json!({"status": "ok", "product": "Shenko", "version": "0.1.0"}))
}

/// The one served UI. Rust embeds only actual artifacts built from the pinned
/// Proto-UI source; no Node server or runtime package download is required.
const INDEX_HTML: &str = include_str!("index.html");
const APP_JS: &str = include_str!("static/app.js");
const APP_CSS: &str = include_str!("static/app.css");
const PROTO_STYLE: &str = include_str!("static/proto-ui-style.css");
const PROTO_TOKENS: &str = include_str!("static/proto-ui-tokens.generated.css");
const SHADCN_THEME: &str = include_str!("static/shadcn-theme.css");
const BUILD_MANIFEST: &str = include_str!("static/build-manifest.json");
const PROTO_LICENSE: &str = include_str!("static/licenses/PROTO-UI-LICENSE");
const SHADCN_NOTICES: &str = include_str!("static/licenses/SHADCN-NOTICES.md");
const REACT_LICENSE: &str = include_str!("static/licenses/REACT-LICENSE");
const REACT_DOM_LICENSE: &str = include_str!("static/licenses/REACT-DOM-LICENSE");
