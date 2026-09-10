//! The HTTP/3 endpoint that receives data from the gateway.
//!
//! The application is a plain [`axum::Router`]. The SDK's [`ScionH3AxumServer`] serves it
//! over HTTP/3 on a SCION socket, so everything you know about axum applies — add routes,
//! extractors and state as you like.

use std::sync::{Arc, Mutex};

use anyhow::Context;
use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use scion_h3_axum::ScionH3AxumServer;
use scion_quic::{quic::config::QuicConfig, reexport::squiche, socket::GenericScionUdpSocket};
use serde::Deserialize;

/// Path the server accepts POST requests on.
pub const DEFAULT_PATH: &str = "/edh/v1/hello";

/// TLS name of the server. HTTP/3 always runs over TLS, and the certificate below is
/// issued for this name.
pub const SERVER_NAME: &str = "pq-meter-server";

/// Dummy device table. Change these entries without touching the decision algorithm.
pub const DUMMY_DEVICE_CATALOG: &[Device] = &[
    Device::new("baseline-2-raspberry-pi", "Baseline", 23.0),
    Device::new("noah-iphone", "Iphone", 65.0),
    Device::new("noah-macbook", "Macbook Air", 145.0),
    Device::new("chris-handy", "Smartphone", 60.0),
    Device::new("peter-laptop", "Laptop", 68.0),
];

/// A device whose presence can be inferred from its power consumption.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Device {
    pub id: &'static str,
    pub name: &'static str,
    pub power_watts: f32,
}

impl Device {
    pub const fn new(id: &'static str, name: &'static str, power_watts: f32) -> Self {
        Self {
            id,
            name,
            power_watts,
        }
    }
}

/// The state change inferred from two consecutive total-power readings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DeviceChange {
    Added(Device),
    Removed(Device),
    None,
}

/// Pluggable policy for translating a total-power change into a device change.
///
/// Implement this trait and pass the implementation to [`serve`] to replace the dummy
/// closest-power matcher without changing the HTTP or state-management code.
pub trait DecisionMethod: Send + Sync {
    fn decide(
        &self,
        previous_total_power: Option<f32>,
        total_power: f32,
        catalog: &[Device],
        active_devices: &[Device],
    ) -> DeviceChange;
}

/// Dummy decision method that selects the device closest to the measured power delta.
pub struct ClosestPowerMatch {
    tolerance_watts: f32,
}

impl ClosestPowerMatch {
    pub fn new(tolerance_watts: f32) -> Self {
        Self {
            tolerance_watts: tolerance_watts.max(0.0),
        }
    }
}

impl DecisionMethod for ClosestPowerMatch {
    fn decide(
        &self,
        previous_total_power: Option<f32>,
        total_power: f32,
        catalog: &[Device],
        active_devices: &[Device],
    ) -> DeviceChange {
        let Some(previous_total_power) = previous_total_power else {
            return DeviceChange::None;
        };

        let delta = total_power - previous_total_power;
        let candidates: Box<dyn Iterator<Item = Device> + '_> = if delta > 0.0 {
            Box::new(
                catalog
                    .iter()
                    .copied()
                    .filter(|candidate| !contains_device(active_devices, candidate.id)),
            )
        } else if delta < 0.0 {
            Box::new(active_devices.iter().copied())
        } else {
            return DeviceChange::None;
        };

        let expected_power = delta.abs();
        let closest = candidates.min_by(|left, right| {
            (left.power_watts - expected_power)
                .abs()
                .total_cmp(&(right.power_watts - expected_power).abs())
        });

        match closest {
            Some(device) if (device.power_watts - expected_power).abs() <= self.tolerance_watts => {
                if delta > 0.0 {
                    DeviceChange::Added(device)
                } else {
                    DeviceChange::Removed(device)
                }
            }
            _ => DeviceChange::None,
        }
    }
}

fn contains_device(devices: &[Device], id: &str) -> bool {
    devices.iter().any(|device| device.id == id)
}

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
        decision_method: &dyn DecisionMethod,
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
pub type SharedDecisionMethod = Arc<dyn DecisionMethod>;

#[derive(Clone)]
struct AppState {
    meter: SharedMeterState,
    decision_method: SharedDecisionMethod,
}

/// Serves the HTTP/3 application on `socket` until the process is stopped.
pub async fn serve(
    socket: Arc<dyn GenericScionUdpSocket>,
    path: &str,
    meter: SharedMeterState,
    decision_method: SharedDecisionMethod,
) -> anyhow::Result<()> {
    let app_state = AppState {
        meter,
        decision_method,
    };
    let app = Router::new()
        .route(path, post(receive))
        .with_state(app_state);
    let config = quic_config().context("building the QUIC server configuration")?;

    ScionH3AxumServer::serve(socket, app, config)
        .await
        .map_err(|error| anyhow::anyhow!("HTTP/3 server stopped: {error}"))
}

/// A total active-power measurement received from the gateway.
#[derive(Debug, Deserialize)]
struct PowerReading {
    /// Total active power in watts. The aliases make common meter/client names acceptable.
    #[serde(alias = "power", alias = "power_watts", alias = "power_l1_n")]
    total_power: f32,
}

/// Accepted wire formats. `ClientMessage` keeps the repository's existing CLI client useful:
/// `--message 860` is serialized by that client as `{"message":"860"}`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum IncomingReading {
    Reading(PowerReading),
    ClientMessage { message: String },
}

impl IncomingReading {
    fn total_power(self) -> Result<f32, &'static str> {
        match self {
            Self::Reading(reading) => Ok(reading.total_power),
            Self::ClientMessage { message } => message
                .parse()
                .map_err(|_| "message must contain a power value in watts"),
        }
    }
}

/// Deserializes one reading, decides whether a device changed, and updates shared state.
async fn receive(
    State(state): State<AppState>,
    Json(reading): Json<IncomingReading>,
) -> (StatusCode, String) {
    let total_power = match reading.total_power() {
        Ok(total_power) => total_power,
        Err(message) => return (StatusCode::BAD_REQUEST, format!("{message}\n")),
    };

    if !total_power.is_finite() || total_power < 0.0 {
        return (
            StatusCode::BAD_REQUEST,
            "total_power must be a finite, non-negative number\n".to_owned(),
        );
    }

    let mut meter = match state.meter.lock() {
        Ok(meter) => meter,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "meter state is unavailable\n".to_owned(),
            );
        }
    };

    let was_first_reading = meter.previous_total_power.is_none();
    let change = meter.apply_reading(total_power, state.decision_method.as_ref());
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
        DeviceChange::None => "no matching device change\n".to_owned(),
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

    #[test]
    fn first_reading_only_sets_the_baseline() {
        let mut state = MeterState::new(DUMMY_DEVICE_CATALOG.to_vec());
        let method = ClosestPowerMatch::new(40.0);

        assert_eq!(state.apply_reading(500.0, &method), DeviceChange::None);
        assert_eq!(state.previous_total_power, Some(500.0));
        assert!(state.active_devices.is_empty());
    }

    #[test]
    fn adds_and_removes_the_closest_device() {
        let mut state = MeterState::new(DUMMY_DEVICE_CATALOG.to_vec());
        let method = ClosestPowerMatch::new(40.0);

        state.apply_reading(500.0, &method);
        assert_eq!(
            state.apply_reading(1_320.0, &method),
            DeviceChange::Added(DUMMY_DEVICE_CATALOG[2])
        );
        assert_eq!(state.active_devices, vec![DUMMY_DEVICE_CATALOG[2]]);

        assert_eq!(
            state.apply_reading(500.0, &method),
            DeviceChange::Removed(DUMMY_DEVICE_CATALOG[2])
        );
        assert!(state.active_devices.is_empty());
    }

    #[test]
    fn ignores_a_delta_outside_the_tolerance() {
        let mut state = MeterState::new(DUMMY_DEVICE_CATALOG.to_vec());
        let method = ClosestPowerMatch::new(10.0);

        state.apply_reading(500.0, &method);
        assert_eq!(state.apply_reading(800.0, &method), DeviceChange::None);
        assert!(state.active_devices.is_empty());
    }

    #[test]
    fn accepts_direct_and_existing_client_payloads() {
        let direct: IncomingReading = serde_json::from_str(r#"{"total_power":860.0}"#).unwrap();
        let client: IncomingReading = serde_json::from_str(r#"{"message":"860"}"#).unwrap();

        assert_eq!(direct.total_power(), Ok(860.0));
        assert_eq!(client.total_power(), Ok(860.0));
    }
}
