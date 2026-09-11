//! Local HTTP dashboard. Assets are embedded so the binary is self-contained.
//!
//! TODO(security): the dashboard has no authentication. It is bound to `127.0.0.1` by the
//! caller, independently of `--bind-ip`, so reaching it means already being on this machine;
//! that is the only thing protecting it. Exposing it on a network interface needs
//! authentication and TLS first.

pub mod model;

#[cfg(test)]
mod tests;

use std::sync::{Arc, Mutex};

use anyhow::Context;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    http::{StatusCode, header},
    middleware,
    response::IntoResponse,
    routing::{get, post, put},
};
use tokio::net::TcpListener;

use crate::quality::ClientThresholdConfig;
use model::{AnomalyStatus, SnapshotSource};

#[derive(Clone)]
struct DashboardAppState {
    source: Arc<dyn SnapshotSource>,
    pending_config: Arc<Mutex<Option<ClientThresholdConfig>>>,
    anomaly: Arc<Mutex<Option<AnomalyStatus>>>,
}

pub fn router(
    source: Arc<dyn SnapshotSource>,
    pending_config: Arc<Mutex<Option<ClientThresholdConfig>>>,
) -> Router {
    let state = DashboardAppState {
        source,
        pending_config,
        anomaly: Arc::new(Mutex::new(None)),
    };
    Router::new()
        .route("/", get(|| async { asset("text/html; charset=utf-8", include_str!("assets/index.html")) }))
        .route("/assets/styles.css", get(|| async { asset("text/css; charset=utf-8", include_str!("assets/styles.css")) }))
        .route("/assets/app.js", get(|| async { asset("text/javascript; charset=utf-8", include_str!("assets/app.js")) }))
        .route("/assets/api.js", get(|| async { asset("text/javascript; charset=utf-8", include_str!("assets/api.js")) }))
        .route("/assets/overview.js", get(|| async { asset("text/javascript; charset=utf-8", include_str!("assets/overview.js")) }))
        .route("/assets/charts.js", get(|| async { asset("text/javascript; charset=utf-8", include_str!("assets/charts.js")) }))
        .route("/api/v1/state", get(current_state))
        .route("/api/v1/history", get(recent_history))
        .route("/api/v1/anomaly", get(current_anomaly).post(update_anomaly))
        .route("/api/v1/forecast", get(current_forecast).post(update_forecast).put(update_forecast))
        .route("/api/v1/devices/{id}/label", put(rename_device))
        .route("/api/v1/settings", get(get_settings))
        .route("/api/v1/settings", post(update_settings))
        .layer(DefaultBodyLimit::max(65536))
        .with_state(state)
        .layer(middleware::map_response(|mut response: axum::response::Response| async move {
            let headers = response.headers_mut();
            headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
            headers.insert(header::CONTENT_SECURITY_POLICY, "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; object-src 'none'; frame-ancestors 'none'; base-uri 'none'".parse().unwrap());
            headers.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
            response
        }))
}

#[derive(serde::Deserialize)]
struct RenameRequest {
    name: String,
}

async fn rename_device(
    State(state): State<DashboardAppState>,
    Path(id): Path<String>,
    Json(request): Json<RenameRequest>,
) -> impl IntoResponse {
    let source = state.source;
    // Disk persistence runs off the async executor. JSON + PUT also prevents a
    // cross-origin HTML form from mutating this loopback-only dashboard.
    let result =
        tokio::task::spawn_blocking(move || source.rename_device(&id, &request.name)).await;
    use crate::labels::RenameError;
    let (status, message) = match result {
        Ok(Ok(name)) => return Json(serde_json::json!({ "name": name })).into_response(),
        Ok(Err(RenameError::InvalidName)) => (
            StatusCode::BAD_REQUEST,
            "Use a name of 1–80 characters without control characters.",
        ),
        Ok(Err(RenameError::NotFound)) => (
            StatusCode::NOT_FOUND,
            "This device is no longer in the catalog.",
        ),
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Could not save the device name. Please retry.",
        ),
    };
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

fn asset(content_type: &'static str, body: &'static str) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, content_type)], body)
}

async fn current_state(State(state): State<DashboardAppState>) -> impl IntoResponse {
    match state.source.snapshot() {
        Ok(mut snapshot) => {
            snapshot.power_quality.anomaly =
                state.anomaly.lock().ok().and_then(|status| status.clone());
            Json(snapshot).into_response()
        }
        Err(message) => unavailable(message),
    }
}

async fn current_anomaly(State(state): State<DashboardAppState>) -> impl IntoResponse {
    match state.anomaly.lock() {
        Ok(status) => match status.clone() {
            Some(status) => Json(status).into_response(),
            None => StatusCode::NO_CONTENT.into_response(),
        },
        Err(_) => unavailable("anomaly state is unavailable"),
    }
}

async fn update_anomaly(
    State(state): State<DashboardAppState>,
    Json(anomaly): Json<Option<AnomalyStatus>>,
) -> impl IntoResponse {
    match state.anomaly.lock() {
        Ok(mut status) => {
            *status = anomaly;
            StatusCode::ACCEPTED.into_response()
        }
        Err(_) => unavailable("anomaly state is unavailable"),
    }
}

/// The recent series behind the charts. Separate from the live snapshot so the
/// once-a-second view stays small however long the window grows.
async fn recent_history(State(state): State<DashboardAppState>) -> impl IntoResponse {
    match state.source.history() {
        Ok(series) => Json(series).into_response(),
        Err(message) => unavailable(message),
    }
}

async fn current_forecast(State(state): State<DashboardAppState>) -> impl IntoResponse {
    match state.source.forecast() {
        Ok(Some(forecast)) => Json(forecast).into_response(),
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Err(message) => unavailable(message),
    }
}

async fn update_forecast(
    State(state): State<DashboardAppState>,
    Json(forecast): Json<model::ForecastSeries>,
) -> impl IntoResponse {
    match state.source.update_forecast(forecast) {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(message) => unavailable(message),
    }
}

/// Returns the current threshold settings that will be synchronized to the client.
async fn get_settings(State(state): State<DashboardAppState>) -> impl IntoResponse {
    let config = state
        .pending_config
        .lock()
        .ok()
        .and_then(|guard| guard.clone());
    Json(serde_json::json!({ "pending": config })).into_response()
}

/// Stores new threshold settings to be sent to the client on the next batch.
async fn update_settings(
    State(state): State<DashboardAppState>,
    Json(config): Json<ClientThresholdConfig>,
) -> impl IntoResponse {
    match state.pending_config.lock() {
        Ok(mut pending) => {
            *pending = Some(config);
            Json(serde_json::json!({ "status": "queued" })).into_response()
        }
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "settings state is unavailable" })),
        )
            .into_response(),
    }
}

fn unavailable(message: &'static str) -> axum::response::Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "error": message })),
    )
        .into_response()
}

pub async fn serve(
    listener: TcpListener,
    source: Arc<dyn SnapshotSource>,
    pending_config: Arc<Mutex<Option<ClientThresholdConfig>>>,
) -> anyhow::Result<()> {
    axum::serve(listener, router(source, pending_config))
        .await
        .context("dashboard HTTP server stopped")
}
