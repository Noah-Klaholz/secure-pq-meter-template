//! HTTP/3 transport and application state for gateway power readings.

use std::sync::{Arc, Mutex};

use anyhow::Context;
use axum::{Router, body::Bytes, extract::State, http::StatusCode, routing::post};
use scion_h3_axum::ScionH3AxumServer;
use scion_quic::{quic::config::QuicConfig, reexport::squiche, socket::GenericScionUdpSocket};

use crate::{
    decision::{DecisionMethod, Device, DeviceChange, contains_device},
    input::SharedReadingDecoder,
};

/// Path the server accepts POST requests on.
pub const DEFAULT_PATH: &str = "/edh/v1/hello";

/// TLS name of the server. HTTP/3 always runs over TLS, and the certificate below is
/// issued for this name.
pub const SERVER_NAME: &str = "pq-meter-server";

/// Mutable meter state shared by all requests.
pub struct MeterState {
    catalog: Vec<Device>,
    active_devices: Vec<Device>,
    previous_total_power: Option<f32>,
}

impl MeterState {
    pub fn new(catalog: Vec<Device>) -> Self {
        Self {
            catalog,
            active_devices: Vec::new(),
            previous_total_power: None,
        }
    }

    fn apply_reading(
        &mut self,
        total_power: f32,
        decision_method: &mut dyn DecisionMethod,
    ) -> DeviceChange {
        let change = decision_method.decide(
            self.previous_total_power,
            total_power,
            &self.catalog,
            &self.active_devices,
        );

        match change {
            DeviceChange::Added(device) => {
                if !contains_device(&self.active_devices, device.id) {
                    self.active_devices.push(device);
                }
            }
            DeviceChange::Removed(device) => {
                self.active_devices
                    .retain(|active_device| active_device.id != device.id);
            }
            DeviceChange::None => {}
        }

        self.previous_total_power = Some(total_power);
        change
    }
}

pub type SharedMeterState = Arc<Mutex<MeterState>>;
pub type SharedDecisionMethod = Arc<Mutex<Box<dyn DecisionMethod>>>;

#[derive(Clone)]
struct AppState {
    meter: SharedMeterState,
    decision_method: SharedDecisionMethod,
    reading_decoder: SharedReadingDecoder,
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
    let app = Router::new()
        .route(path, post(receive))
        .with_state(app_state);
    let config = quic_config().context("building the QUIC server configuration")?;

    ScionH3AxumServer::serve(socket, app, config)
        .await
        .map_err(|error| anyhow::anyhow!("HTTP/3 server stopped: {error}"))
}

/// Decodes one reading, asks the selected method for a decision, and updates shared state.
async fn receive(State(state): State<AppState>, body: Bytes) -> (StatusCode, String) {
    let total_power = match state.reading_decoder.decode_total_power(&body) {
        Ok(total_power) => total_power,
        Err(message) => return (StatusCode::BAD_REQUEST, format!("{message}\n")),
    };

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

    let was_first_reading = meter.previous_total_power.is_none();
    let change = meter.apply_reading(total_power, decision_method.as_mut());
    let response = match change {
        DeviceChange::Added(device) => {
            format!("added {} ({:.1} W)\n", device.name, device.power_watts)
        }
        DeviceChange::Removed(device) => {
            format!("removed {} ({:.1} W)\n", device.name, device.power_watts)
        }
        DeviceChange::None if was_first_reading => {
            format!("baseline recorded at {total_power:.1} W\n")
        }
        DeviceChange::None => "no device state change\n".to_owned(),
    };

    (StatusCode::OK, response)
}

/// Builds the QUIC configuration of the server, with a self-signed certificate that is
/// generated on every start.
///
/// The client does not verify this certificate. That keeps the setup short, but it means
/// the connection is encrypted without the client knowing who it talks to. Use a real
/// certificate before taking anything like this outside of a hackathon.
fn quic_config() -> anyhow::Result<squiche::Config> {
    let mut config = QuicConfig::builder()
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
    use crate::decision::{ClosestPowerMatch, DUMMY_DEVICE_CATALOG};

    #[test]
    fn applies_addition_and_removal_decisions_to_state() {
        let mut state = MeterState::new(DUMMY_DEVICE_CATALOG.to_vec());
        let mut method = ClosestPowerMatch::new(3.0);

        assert_eq!(state.apply_reading(100.0, &mut method), DeviceChange::None);
        assert_eq!(
            state.apply_reading(123.0, &mut method),
            DeviceChange::Added(DUMMY_DEVICE_CATALOG[0])
        );
        assert_eq!(
            state.apply_reading(100.0, &mut method),
            DeviceChange::Removed(DUMMY_DEVICE_CATALOG[0])
        );
        assert!(state.active_devices.is_empty());
    }
}
