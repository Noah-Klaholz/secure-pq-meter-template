//! HTTP/3 transport and application state for gateway power readings.

use std::sync::{Arc, Mutex};

use anyhow::Context;
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use scion_h3_axum::ScionH3AxumServer;
use scion_quic::{quic::config::QuicConfig, reexport::squiche, socket::GenericScionUdpSocket};

use crate::{
    decision::{DecisionMethod, DeviceChange},
    input::SharedReadingDecoder,
    meter::SharedMeterState,
    transport::GatewayTransport,
};

/// Path the server accepts POST requests on.
pub const DEFAULT_PATH: &str = "/edh/v1/hello";

/// TLS name of the server. HTTP/3 always runs over TLS, and the certificate below is
/// issued for this name.
pub const SERVER_NAME: &str = "pq-meter-server";

pub type SharedDecisionMethod = Arc<Mutex<Box<dyn DecisionMethod>>>;

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) meter: SharedMeterState,
    pub(crate) decision_method: SharedDecisionMethod,
    pub(crate) reading_decoder: SharedReadingDecoder,
}

/// Serves the HTTP/3 application on `socket` until the process is stopped.
pub async fn serve(
    socket: Arc<dyn GenericScionUdpSocket>,
    path: &str,
    meter: SharedMeterState,
    decision_method: SharedDecisionMethod,
    reading_decoder: SharedReadingDecoder,
) -> anyhow::Result<()> {
    let app_state = AppState {
        meter,
        decision_method,
        reading_decoder,
    };
    let app = router(path, app_state);
    let config = quic_config().context("building the QUIC server configuration")?;

    ScionH3AxumServer::serve(socket, app, config)
        .await
        .map_err(|error| anyhow::anyhow!("HTTP/3 server stopped: {error}"))
}

pub(crate) fn router(path: &str, state: AppState) -> Router {
    Router::new().route(path, post(receive)).with_state(state)
}

/// Validates the whole request before applying each reading in order. Holding both
/// locks for the batch prevents other requests from interleaving its measurements.
///
/// TODO(security): the endpoint is unauthenticated. Anything that can reach the SNAP can
/// post readings and move the state this serves, and nothing ties a batch to the meter it
/// claims to come from. On a real deployment the network layer should reject an
/// unauthorized gateway before it gets here, and the reading itself should carry an
/// identity the receiver checks.
async fn receive(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, String) {
    // Read before validating the body: the gateway's own view of the link is worth having
    // even for a batch that turns out to be malformed.
    let reported_transport = GatewayTransport::from_headers(&headers);
    let readings = match state.reading_decoder.decode_readings(&body) {
        Ok(readings) => readings,
        Err(message) => return (StatusCode::BAD_REQUEST, format!("{message}\n")),
    };

    // File writes and fsync must not block a Tokio runtime worker.
    tokio::task::spawn_blocking(move || apply_batch(state, readings, reported_transport))
        .await
        .unwrap_or_else(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "measurement processing failed\n".to_owned(),
            )
        })
}

fn apply_batch(
    state: AppState,
    readings: Vec<crate::input::MeterReading>,
    reported_transport: GatewayTransport,
) -> (StatusCode, String) {
    let mut meter = match state.meter.lock() {
        Ok(meter) => meter,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "meter state is unavailable\n".to_owned(),
            );
        }
    };
    let mut decision_method = match state.decision_method.lock() {
        Ok(decision_method) => decision_method,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "decision method is unavailable\n".to_owned(),
            );
        }
    };

    let received_at = match meter.persist_readings(&readings) {
        Ok(at) => at,
        Err(error) => {
            tracing::error!(%error, "could not persist measurement batch");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "history storage is unavailable; batch was not accepted\n".to_owned(),
            );
        }
    };
    meter.record_transport(reported_transport);

    let count = readings.len();
    let mut additions = 0;
    let mut removals = 0;
    let mut response = String::new();
    for reading in readings {
        let was_first_reading = meter.latest_power().is_none();
        let change = meter.apply_reading_at(reading, decision_method.as_mut(), received_at);
        match change {
            DeviceChange::Added(_) => additions += 1,
            DeviceChange::Removed(_) => removals += 1,
            DeviceChange::None => {}
        }
        if count == 1 {
            response = reading_response(change, was_first_reading, reading.total_power);
        }
    }
    if count > 1 {
        // Keep the acknowledgement bounded: the client reads at most 4096 bytes.
        response = format!("accepted {count} readings: {additions} added, {removals} removed\n");
    }

    (StatusCode::OK, response)
}

fn reading_response(change: DeviceChange, was_first_reading: bool, total_power: f32) -> String {
    match change {
        DeviceChange::Added(device) => {
            if let (Some(q), Some(thd)) = (device.reactive_power_var, device.thd_current_pct) {
                format!(
                    "added {} ({:.1} W, {:+.1} var, {:.1}% THD)\n",
                    device.name, device.power_watts, q, thd
                )
            } else {
                format!("added {} ({:.1} W)\n", device.name, device.power_watts)
            }
        }
        DeviceChange::Removed(device) => {
            if let (Some(q), Some(thd)) = (device.reactive_power_var, device.thd_current_pct) {
                format!(
                    "removed {} ({:.1} W, {:+.1} var, {:.1}% THD)\n",
                    device.name, device.power_watts, q, thd
                )
            } else {
                format!("removed {} ({:.1} W)\n", device.name, device.power_watts)
            }
        }
        DeviceChange::None if was_first_reading => {
            format!("baseline recorded at {total_power:.1} W\n")
        }
        DeviceChange::None => "no device state change\n".to_owned(),
    }
}

/// Builds the QUIC configuration of the server, with a self-signed certificate that is
/// generated on every start.
///
/// The client does not verify this certificate. That keeps the setup short, but it means
/// the connection is encrypted without the client knowing who it talks to. Use a real
/// certificate before taking anything like this outside of a hackathon.
///
/// TODO(security): a certificate regenerated on every start cannot be pinned by anything,
/// which is what forces the gateway to skip verification. Issue a stable certificate the
/// gateway is provisioned to expect, then turn its `verify_peer` back on.
fn quic_config() -> anyhow::Result<squiche::Config> {
    let mut config = QuicConfig::builder()
        // TODO(security): the receiver does not authenticate gateways either, so it cannot
        // tell one meter from another. Client certificates would let it, and would pair
        // with a SNAP token that stops an unknown gateway at the network layer.
        .verify_peer(false)
        .build()
        .to_quiche_config()
        .context("creating the QUIC configuration")?;

    let certificate = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_string()])
        .context("generating a self-signed certificate")?;

    // squiche reads the certificate and the key from files, so write them to temporary
    // files that are removed again when this function returns.
    let certificate_file = write_temporary_file(certificate.cert.pem().as_bytes(), "certificate")?;
    let key_file = write_temporary_file(certificate.signing_key.serialize_pem().as_bytes(), "key")?;

    config
        .load_cert_chain_from_pem_file(path_of(&certificate_file)?)
        .context("loading the certificate")?;
    config
        .load_priv_key_from_pem_file(path_of(&key_file)?)
        .context("loading the private key")?;

    Ok(config)
}

/// Writes `contents` to a temporary file.
fn write_temporary_file(contents: &[u8], what: &str) -> anyhow::Result<tempfile::NamedTempFile> {
    use std::io::Write;

    let mut file = tempfile::NamedTempFile::new()
        .with_context(|| format!("creating a temporary file for the {what}"))?;
    file.write_all(contents)
        .with_context(|| format!("writing the {what}"))?;
    Ok(file)
}

/// Returns the path of a temporary file as a string.
fn path_of(file: &tempfile::NamedTempFile) -> anyhow::Result<&str> {
    file.path()
        .to_str()
        .context("temporary file path is not valid UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        decision::{ClosestPowerMatch, DUMMY_DEVICE_CATALOG},
        meter::MeterState,
    };

    fn test_state(method: Box<dyn DecisionMethod>) -> AppState {
        AppState {
            meter: Arc::new(Mutex::new(MeterState::new(DUMMY_DEVICE_CATALOG.to_vec()))),
            decision_method: Arc::new(Mutex::new(method)),
            reading_decoder: Arc::new(crate::input::JsonReadingDecoder),
        }
    }

    #[tokio::test]
    async fn accepted_batches_survive_restart_and_rejected_batches_never_reach_disk() {
        use crate::{history::History, history_store::HistoryStore, labels::DeviceLabels};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.jsonl");
        let state = AppState {
            meter: Arc::new(Mutex::new(
                MeterState::with_labels(DUMMY_DEVICE_CATALOG.to_vec(), DeviceLabels::default())
                    .with_history_file(&path)
                    .unwrap(),
            )),
            decision_method: Arc::new(Mutex::new(Box::new(ClosestPowerMatch::new(3.0)))),
            reading_decoder: Arc::new(crate::input::JsonReadingDecoder),
        };
        let (status, _) = post_json(
            state.clone(),
            serde_json::json!([
                { "total_power": 100.0 }, { "total_power": 123.0, "heartbeat": true }
            ]),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let committed = std::fs::read(&path).unwrap();
        let (status, _) = post_json(
            state.clone(),
            serde_json::json!([
                { "total_power": 200.0 }, { "total_power": "invalid" }
            ]),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(std::fs::read(&path).unwrap(), committed);
        assert_eq!(state.meter.lock().unwrap().snapshot().readings_received, 2);
        drop(state);
        let mut history = History::bounded(600);
        let (_, count) = HistoryStore::open(&path, &mut history).unwrap();
        assert_eq!(count, 2);
        assert_eq!(history.all()[0].data.total_power, 100.0);
        assert!(history.all()[1].data.heartbeat);
    }

    async fn post_json(state: AppState, body: serde_json::Value) -> (StatusCode, String) {
        use axum::{
            body::{Body, to_bytes},
            http::Request,
        };
        use tower::ServiceExt;

        let response = router(DEFAULT_PATH, state)
            .oneshot(
                Request::post(DEFAULT_PATH)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn client_batch_applies_every_reading_in_order_and_exposes_context() {
        use crate::{
            dashboard::model::{LiveMeterSource, SnapshotSource},
            input::tests::client_reading,
        };

        let state = test_state(Box::new(ClosestPowerMatch::new(3.0)));
        let batch = serde_json::json!([
            client_reading(100.0, 10),
            client_reading(123.0, 11),
            client_reading(100.0, 12),
            client_reading(165.0, 13),
        ]);
        let (status, response) = post_json(state.clone(), batch.clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response, "accepted 4 readings: 2 added, 1 removed\n");
        let source = LiveMeterSource {
            meter: state.meter.clone(),
            decision_method: "immediate",
        };
        let snapshot = source.snapshot().unwrap();
        assert_eq!(snapshot.readings_received, 4);
        assert_eq!(snapshot.total_power_watts, Some(165.0));
        assert!(!snapshot.devices[0].active);
        assert!(snapshot.devices[1].active);
        assert_eq!(
            serde_json::to_value(snapshot.latest_reading).unwrap(),
            batch[3]
        );
        assert_eq!(
            snapshot.last_change.unwrap().device_id,
            DUMMY_DEVICE_CATALOG[1].id
        );

        // A later legacy reading must not inherit context from an older measurement.
        let (status, _) = post_json(state, serde_json::json!({"total_power": 165})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            serde_json::to_value(source.snapshot().unwrap().latest_reading).unwrap(),
            serde_json::json!({"total_power": 165.0})
        );
    }

    #[tokio::test]
    async fn settled_decisions_count_samples_within_and_across_batches() {
        use crate::decision::SettledPowerMatch;
        for batches in [
            vec![vec![100, 123, 123, 123]],
            vec![vec![100, 123], vec![123, 123]],
            vec![vec![100], vec![123], vec![123], vec![123]],
        ] {
            let state = test_state(Box::new(SettledPowerMatch::new(5.0, 3.0, 3, 5.0)));
            for powers in batches {
                let readings: Vec<_> = powers
                    .into_iter()
                    .map(|power| serde_json::json!({"total_power": power}))
                    .collect();
                assert_eq!(
                    post_json(state.clone(), serde_json::json!(readings))
                        .await
                        .0,
                    StatusCode::OK
                );
            }
            let meter = state.meter.lock().unwrap().snapshot();
            assert_eq!(meter.readings_received, 4);
            assert_eq!(meter.active_devices, vec![DUMMY_DEVICE_CATALOG[0]]);
        }
    }

    #[tokio::test]
    async fn invalid_batches_leave_meter_and_stateful_decisions_untouched() {
        use crate::{decision::SettledPowerMatch, input::tests::client_reading};
        let state = test_state(Box::new(SettledPowerMatch::new(5.0, 3.0, 3, 5.0)));
        assert_eq!(
            post_json(state.clone(), client_reading(100.0, 10)).await.0,
            StatusCode::OK
        );
        let before = state.meter.lock().unwrap().snapshot();
        for invalid in [
            serde_json::json!([]),
            serde_json::json!([client_reading(123.0, 11), client_reading(123.0, 12), {"total_power": 1e100}]),
            serde_json::json!([client_reading(123.0, 11), {"total_power": 123, "l1": {"voltage_v": "bad"}}]),
        ] {
            assert_eq!(
                post_json(state.clone(), invalid).await.0,
                StatusCode::BAD_REQUEST
            );
            let after = state.meter.lock().unwrap().snapshot();
            assert_eq!(after.latest_reading, before.latest_reading);
            assert_eq!(after.readings_received, before.readings_received);
            assert_eq!(after.last_received_at, before.last_received_at);
            assert_eq!(after.last_change, before.last_change);
            assert_eq!(after.active_devices, before.active_devices);
        }
        assert_eq!(
            post_json(state.clone(), client_reading(123.0, 13)).await.0,
            StatusCode::OK
        );
        let after = state.meter.lock().unwrap().snapshot();
        assert_eq!(after.readings_received, 2);
        assert!(
            after.active_devices.is_empty(),
            "rejected samples must not advance settling"
        );
    }

    #[tokio::test]
    async fn single_element_batches_and_legacy_messages_keep_single_reading_responses() {
        let state = test_state(Box::new(ClosestPowerMatch::new(3.0)));
        assert_eq!(
            post_json(state.clone(), serde_json::json!([{"total_power": 100}])).await,
            (StatusCode::OK, "baseline recorded at 100.0 W\n".to_owned())
        );
        assert_eq!(
            post_json(state.clone(), serde_json::json!({"message": "123"})).await,
            (StatusCode::OK, "added Baseline (23.0 W)\n".to_owned())
        );
        assert_eq!(state.meter.lock().unwrap().snapshot().readings_received, 2);
    }

    #[tokio::test]
    async fn exported_power_is_accepted_and_detected_as_a_change() {
        // A site that feeds into the grid reports a negative total. Detection works on the
        // change between readings, so it has to keep working below zero: the meter never
        // leaves export here, and switching the 145 W device on and off still registers.
        let state = test_state(Box::new(ClosestPowerMatch::new(3.0)));
        assert_eq!(
            post_json(state.clone(), serde_json::json!({"total_power": -1000.0})).await,
            (
                StatusCode::OK,
                "baseline recorded at -1000.0 W\n".to_owned()
            )
        );

        let (status, response) =
            post_json(state.clone(), serde_json::json!({"total_power": -855.0})).await;
        assert_eq!(status, StatusCode::OK);
        assert!(response.starts_with("added Macbook Air"), "{response}");

        let (status, response) =
            post_json(state.clone(), serde_json::json!({"total_power": -1000.0})).await;
        assert_eq!(status, StatusCode::OK);
        assert!(response.starts_with("removed Macbook Air"), "{response}");

        let meter = state.meter.lock().unwrap().snapshot();
        assert_eq!(meter.total_power_watts, Some(-1000.0));
        assert_eq!(meter.readings_received, 3);
    }

    #[tokio::test]
    async fn large_batches_fit_the_clients_response_limit() {
        let state = test_state(Box::new(ClosestPowerMatch::new(3.0)));
        let readings = vec![serde_json::json!({"total_power": 100}); 1000];
        assert_eq!(
            post_json(state.clone(), serde_json::json!(readings)).await,
            (
                StatusCode::OK,
                "accepted 1000 readings: 0 added, 0 removed\n".to_owned()
            )
        );
        assert_eq!(
            state.meter.lock().unwrap().snapshot().readings_received,
            1000
        );
    }

    #[tokio::test]
    async fn only_accepted_requests_update_the_dashboard_state() {
        use crate::dashboard::model::{LiveMeterSource, SnapshotSource};
        let meter = Arc::new(Mutex::new(MeterState::new(DUMMY_DEVICE_CATALOG.to_vec())));
        let state = AppState {
            meter: meter.clone(),
            decision_method: Arc::new(Mutex::new(Box::new(ClosestPowerMatch::new(3.0)))),
            reading_decoder: Arc::new(crate::input::JsonReadingDecoder),
        };
        let source = LiveMeterSource {
            meter,
            decision_method: "immediate",
        };
        for body in [r#"{"total_power":100}"#, r#"{"total_power":123}"#] {
            assert_eq!(
                receive(State(state.clone()), HeaderMap::new(), Bytes::from(body))
                    .await
                    .0,
                StatusCode::OK
            );
        }
        let before = source.snapshot().unwrap();
        assert_eq!(before.readings_received, 2);
        assert!(before.devices[0].active);
        assert_eq!(
            receive(
                State(state),
                HeaderMap::new(),
                Bytes::from_static(br#"{"total_power":1e100}"#)
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        let after = source.snapshot().unwrap();
        assert_eq!(after.readings_received, before.readings_received);
        assert_eq!(after.last_received_at, before.last_received_at);
        assert_eq!(after.total_power_watts, Some(123.0));
    }

    #[test]
    fn applies_addition_and_removal_decisions_to_state() {
        let mut state = MeterState::new(DUMMY_DEVICE_CATALOG.to_vec());
        let mut method = ClosestPowerMatch::new(3.0);

        assert_eq!(
            state.apply_reading(100.0.into(), &mut method),
            DeviceChange::None
        );
        assert_eq!(
            state.apply_reading(123.0.into(), &mut method),
            DeviceChange::Added(DUMMY_DEVICE_CATALOG[0])
        );
        assert_eq!(
            state.apply_reading(100.0.into(), &mut method),
            DeviceChange::Removed(DUMMY_DEVICE_CATALOG[0])
        );
        assert!(state.snapshot().active_devices.is_empty());
    }

    #[tokio::test]
    async fn returns_informative_response_messages_for_all_device_changes() {
        let meter = Arc::new(Mutex::new(MeterState::new(DUMMY_DEVICE_CATALOG.to_vec())));
        let state = AppState {
            meter,
            decision_method: Arc::new(Mutex::new(Box::new(ClosestPowerMatch::new(3.0)))),
            reading_decoder: Arc::new(crate::input::JsonReadingDecoder),
        };

        // First reading: baseline
        let (status, resp) = receive(
            State(state.clone()),
            HeaderMap::new(),
            Bytes::from(r#"{"total_power":100.0}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(resp, "baseline recorded at 100.0 W\n");

        // Second reading: unchanged power -> no device state change
        let (status, resp) = receive(
            State(state.clone()),
            HeaderMap::new(),
            Bytes::from(r#"{"total_power":100.0}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(resp, "no device state change\n");

        // Third reading: add 23 W device
        let (status, resp) = receive(
            State(state.clone()),
            HeaderMap::new(),
            Bytes::from(r#"{"total_power":123.0}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(resp.starts_with("added Baseline"));

        // Fourth reading: remove 23 W device
        let (status, resp) = receive(
            State(state.clone()),
            HeaderMap::new(),
            Bytes::from(r#"{"total_power":100.0}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(resp.starts_with("removed Baseline"));

        // Invalid reading
        let (status, resp) = receive(
            State(state),
            HeaderMap::new(),
            Bytes::from(r#"{"message":"invalid"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(resp.contains("message must contain a power value"));
    }
}
