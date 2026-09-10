//! Local, read-only HTTP dashboard. Assets are embedded so the binary is self-contained.

pub mod model;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use anyhow::Context;
use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, header},
    middleware,
    response::IntoResponse,
    routing::get,
};
use tokio::net::TcpListener;

use model::SnapshotSource;

pub fn router(source: Arc<dyn SnapshotSource>) -> Router {
    Router::new()
        .route("/", get(|| async { asset("text/html; charset=utf-8", include_str!("assets/index.html")) }))
        .route("/assets/styles.css", get(|| async { asset("text/css; charset=utf-8", include_str!("assets/styles.css")) }))
        .route("/assets/app.js", get(|| async { asset("text/javascript; charset=utf-8", include_str!("assets/app.js")) }))
        .route("/assets/api.js", get(|| async { asset("text/javascript; charset=utf-8", include_str!("assets/api.js")) }))
        .route("/assets/overview.js", get(|| async { asset("text/javascript; charset=utf-8", include_str!("assets/overview.js")) }))
        .route("/api/v1/state", get(current_state))
        .with_state(source)
        .layer(middleware::map_response(|mut response: axum::response::Response| async move {
            let headers = response.headers_mut();
            headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
            headers.insert(header::CONTENT_SECURITY_POLICY, "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; object-src 'none'; frame-ancestors 'none'; base-uri 'none'".parse().unwrap());
            headers.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
            response
        }))
}

fn asset(content_type: &'static str, body: &'static str) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, content_type)], body)
}

async fn current_state(State(source): State<Arc<dyn SnapshotSource>>) -> impl IntoResponse {
    match source.snapshot() {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(message) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": message })),
        )
            .into_response(),
    }
}

pub async fn serve(listener: TcpListener, source: Arc<dyn SnapshotSource>) -> anyhow::Result<()> {
    axum::serve(listener, router(source))
        .await
        .context("dashboard HTTP server stopped")
}
