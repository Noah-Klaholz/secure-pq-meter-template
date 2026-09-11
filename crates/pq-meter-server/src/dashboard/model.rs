//! Versioned dashboard read model and its data-source boundary.
//!
//! A future database-backed source can implement `SnapshotSource`. Historical queries
//! should get a separate endpoint and model rather than growing an unbounded snapshot.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    decision::DeviceChange,
    input::{MeterReading, PhaseReading},
    labels::RenameError,
    meter::{HISTORY_WINDOW, SharedMeterState},
    quality::{self, Limits, Violation},
    transport::GatewayTransport,
};

/// A multi-step projected forecast of power consumption.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ForecastSeries {
    pub generated_at: String,
    pub horizon_seconds: f32,
    pub model_name: String,
    pub points: Vec<ForecastPoint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mae: Option<f32>,
}

/// One predicted point in time.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ForecastPoint {
    pub at: String,
    pub predicted_watts: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lower_bound_watts: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upper_bound_watts: Option<f32>,
}

pub trait SnapshotSource: Send + Sync {
    fn rename_device(&self, _id: &str, _name: &str) -> Result<String, RenameError> {
        Err(RenameError::Unavailable)
    }

    fn snapshot(&self) -> Result<Snapshot, &'static str>;

    /// The recent series behind the dashboard charts.
    ///
    /// Kept apart from [`SnapshotSource::snapshot`] on purpose: the live view is polled
    /// every second and has to stay small, while the series grows with the window.
    fn history(&self) -> Result<HistorySeries, &'static str>;

    fn update_forecast(&self, _forecast: ForecastSeries) -> Result<(), &'static str> {
        Ok(())
    }

    fn forecast(&self) -> Result<Option<ForecastSeries>, &'static str> {
        Ok(None)
    }
}

pub struct LiveMeterSource {
    pub meter: SharedMeterState,
    pub decision_method: &'static str,
}

#[derive(Serialize)]
pub struct Snapshot {
    pub schema_version: u8,
    pub generated_at: String,
    pub decision_method: &'static str,
    pub stale_after_seconds: u64,
    pub readings_received: u64,
    pub last_received_at: Option<String>,
    pub total_power_watts: Option<f32>,
    pub latest_reading: Option<MeterReading>,
    pub inferred_power_watts: f64,
    pub devices: Vec<DeviceStatus>,
    pub last_change: Option<Change>,
    /// The measured state of the supply, phase by phase.
    pub power_quality: PowerQuality,
    /// The limits `power_quality.violations` was judged against, so the dashboard draws
    /// the same bands the receiver applies.
    pub limits: Limits,
    /// The SCION link, as far as either end can see it.
    pub transport: Transport,
}

/// The supply as the meter last measured it.
#[derive(Serialize)]
pub struct PowerQuality {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anomaly: Option<AnomalyStatus>,
    pub frequency_hz: Option<f32>,
    /// Signed three-phase real power: negative means the site is exporting.
    pub total_power_watts: Option<f32>,
    pub apparent_power_va: Option<f32>,
    pub reactive_power_var: Option<f32>,
    /// Which way the energy flows, or `None` before the first reading.
    pub flow: Option<&'static str>,
    pub phases: Vec<PhaseStatus>,
    pub violations: Vec<Violation>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct AnomalyStatus {
    pub is_anomaly: bool,
    pub score: f64,
    pub strongest_feature: Option<String>,
    pub timestamp: String,
}

/// One phase of the latest reading, flattened for display.
#[derive(Serialize)]
pub struct PhaseStatus {
    pub name: &'static str,
    pub voltage_v: Option<f32>,
    pub current_a: Option<f32>,
    pub real_power_w: Option<f32>,
    pub reactive_power_var: Option<f32>,
    pub cos_phi: Option<f32>,
    pub thd_voltage_pct: Option<f32>,
    pub thd_current_pct: Option<f32>,
    /// False for a phase with nothing wired to it, which is not a fault.
    pub connected: bool,
}

/// The link the readings arrive over.
#[derive(Serialize)]
pub struct Transport {
    /// `live`, `stale`, or `waiting` — judged by the receiver from arrival times, not
    /// taken from what the gateway claims about itself.
    pub state: &'static str,
    pub seconds_since_last_reading: Option<i64>,
    pub readings_received: u64,
    /// What the gateway reported about its own side of the link.
    #[serde(flatten)]
    pub gateway: GatewayTransport,
    /// Whether the gateway reported anything at all.
    pub gateway_reporting: bool,
}

/// A window of recent readings, oldest first.
#[derive(Serialize)]
pub struct HistorySeries {
    pub schema_version: u8,
    pub generated_at: String,
    pub window_seconds: u64,
    pub persistent: bool,
    pub stored_readings: u64,
    pub samples: Vec<HistorySample>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forecast: Option<ForecastSeries>,
}

/// One point on the dashboard charts. Deliberately narrower than a full reading: this is
/// sent for every sample in the window, so it carries only what is plotted.
#[derive(Serialize)]
pub struct HistorySample {
    pub at: String,
    pub total_power_watts: f32,
    pub frequency_hz: Option<f32>,
    pub voltage_v: [Option<f32>; 3],
    pub current_a: [Option<f32>; 3],
    pub thd_voltage_pct: [Option<f32>; 3],
    pub thd_current_pct: [Option<f32>; 3],
}

#[derive(Serialize)]
pub struct DeviceStatus {
    pub id: &'static str,
    pub name: String,
    pub nominal_power_watts: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reactive_power_var: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thd_current_pct: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cos_phi: Option<f32>,
    pub active: bool,
}

#[derive(Serialize)]
pub struct Change {
    pub kind: &'static str,
    pub device_id: &'static str,
    pub device_name: String,
    pub nominal_power_watts: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reactive_power_var: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thd_current_pct: Option<f32>,
    pub received_at: String,
}

const PHASE_NAMES: [&str; 3] = ["L1", "L2", "L3"];

/// How long after the last reading the link stops counting as live.
const STALE_AFTER_SECONDS: u64 = 10;

fn phase_status(name: &'static str, phase: Option<PhaseReading>) -> PhaseStatus {
    let connected = phase.is_some_and(|phase| {
        phase
            .current_a
            .is_some_and(|current| current.abs() >= quality::CONNECTED_PHASE_CURRENT_A)
    });
    PhaseStatus {
        name,
        voltage_v: phase.and_then(|phase| phase.voltage_v),
        current_a: phase.and_then(|phase| phase.current_a),
        real_power_w: phase.and_then(|phase| phase.real_power_w),
        reactive_power_var: phase.and_then(|phase| phase.reactive_power_var),
        cos_phi: phase.and_then(|phase| phase.cos_phi),
        thd_voltage_pct: phase.and_then(|phase| phase.thd_voltage_pct),
        thd_current_pct: phase.and_then(|phase| phase.thd_current_pct),
        connected,
    }
}

fn power_quality(reading: Option<MeterReading>) -> PowerQuality {
    let Some(reading) = reading else {
        return PowerQuality {
            anomaly: None,
            frequency_hz: None,
            total_power_watts: None,
            apparent_power_va: None,
            reactive_power_var: None,
            flow: None,
            phases: PHASE_NAMES
                .into_iter()
                .map(|name| phase_status(name, None))
                .collect(),
            violations: Vec::new(),
        };
    };

    let totals = reading.totals;
    PowerQuality {
        anomaly: None,
        frequency_hz: reading.frequency_hz,
        total_power_watts: Some(reading.total_power),
        apparent_power_va: totals.and_then(|totals| totals.apparent_power_va),
        reactive_power_var: totals.and_then(|totals| totals.reactive_power_var),
        // A meter that reads zero is neither importing nor exporting.
        flow: Some(if reading.total_power > 0.0 {
            "import"
        } else if reading.total_power < 0.0 {
            "export"
        } else {
            "balanced"
        }),
        phases: PHASE_NAMES
            .into_iter()
            .zip([reading.l1, reading.l2, reading.l3])
            .map(|(name, phase)| phase_status(name, phase))
            .collect(),
        violations: quality::violations(&reading),
    }
}

impl SnapshotSource for LiveMeterSource {
    fn rename_device(&self, id: &str, name: &str) -> Result<String, RenameError> {
        self.meter
            .lock()
            .map_err(|_| RenameError::Unavailable)?
            .rename_device(id, name)
    }

    fn snapshot(&self) -> Result<Snapshot, &'static str> {
        let meter = self
            .meter
            .lock()
            .map_err(|_| "meter state is unavailable")?
            .snapshot();
        let now = Utc::now();
        let seconds_since_last_reading = meter
            .last_received_at
            .map(|at| (now - at).num_seconds().max(0));
        Ok(Snapshot {
            schema_version: 1,
            generated_at: now.to_rfc3339(),
            decision_method: self.decision_method,
            stale_after_seconds: STALE_AFTER_SECONDS,
            readings_received: meter.readings_received,
            last_received_at: meter.last_received_at.map(|time| time.to_rfc3339()),
            total_power_watts: meter.total_power_watts,
            latest_reading: meter.latest_reading,
            inferred_power_watts: meter
                .active_devices
                .iter()
                .map(|device| f64::from(device.power_watts))
                .sum(),
            devices: meter
                .catalog
                .iter()
                .map(|device| DeviceStatus {
                    id: device.id,
                    name: meter
                        .device_names
                        .get(device.id)
                        .cloned()
                        .unwrap_or_else(|| device.name.to_owned()),
                    nominal_power_watts: device.power_watts,
                    reactive_power_var: device.reactive_power_var,
                    thd_current_pct: device.thd_current_pct,
                    cos_phi: device.cos_phi,
                    active: meter
                        .active_devices
                        .iter()
                        .any(|active| active.id == device.id),
                })
                .collect(),
            last_change: meter.last_change.and_then(|(change, time)| {
                let (kind, device) = match change {
                    DeviceChange::Added(device) => ("added", device),
                    DeviceChange::Removed(device) => ("removed", device),
                    DeviceChange::None => return None,
                };
                Some(Change {
                    kind,
                    device_id: device.id,
                    device_name: meter
                        .device_names
                        .get(device.id)
                        .cloned()
                        .unwrap_or_else(|| device.name.to_owned()),
                    nominal_power_watts: device.power_watts,
                    reactive_power_var: device.reactive_power_var,
                    thd_current_pct: device.thd_current_pct,
                    received_at: time.to_rfc3339(),
                })
            }),
            power_quality: power_quality(meter.latest_reading),
            limits: Limits::en50160(),
            transport: Transport {
                state: match seconds_since_last_reading {
                    None => "waiting",
                    Some(seconds) if seconds as u64 > STALE_AFTER_SECONDS => "stale",
                    Some(_) => "live",
                },
                seconds_since_last_reading,
                readings_received: meter.readings_received,
                gateway_reporting: !meter.transport.is_empty(),
                gateway: meter.transport,
            },
        })
    }

    fn history(&self) -> Result<HistorySeries, &'static str> {
        let meter = self
            .meter
            .lock()
            .map_err(|_| "meter state is unavailable")?;
        let samples = meter
            .recent_history()
            .map_err(|_| "history is unavailable")?
            .iter()
            .map(|entry| {
                let reading = entry.data;
                let phases = [reading.l1, reading.l2, reading.l3];
                HistorySample {
                    at: DateTime::<Utc>::from(entry.timestamp).to_rfc3339(),
                    total_power_watts: reading.total_power,
                    frequency_hz: reading.frequency_hz,
                    voltage_v: phases.map(|phase| phase.and_then(|phase| phase.voltage_v)),
                    current_a: phases.map(|phase| phase.and_then(|phase| phase.current_a)),
                    thd_voltage_pct: phases
                        .map(|phase| phase.and_then(|phase| phase.thd_voltage_pct)),
                    thd_current_pct: phases
                        .map(|phase| phase.and_then(|phase| phase.thd_current_pct)),
                }
            })
            .collect();
        Ok(HistorySeries {
            schema_version: 1,
            generated_at: Utc::now().to_rfc3339(),
            window_seconds: HISTORY_WINDOW.as_secs(),
            persistent: meter.history_is_persistent(),
            stored_readings: meter.stored_readings(),
            samples,
            forecast: meter.latest_forecast(),
        })
    }

    fn update_forecast(&self, forecast: ForecastSeries) -> Result<(), &'static str> {
        let mut meter = self
            .meter
            .lock()
            .map_err(|_| "meter state is unavailable")?;
        meter.update_forecast(forecast);
        Ok(())
    }

    fn forecast(&self) -> Result<Option<ForecastSeries>, &'static str> {
        let meter = self
            .meter
            .lock()
            .map_err(|_| "meter state is unavailable")?;
        Ok(meter.latest_forecast())
    }
}
