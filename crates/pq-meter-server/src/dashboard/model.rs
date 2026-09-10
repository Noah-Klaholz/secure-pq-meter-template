//! Versioned dashboard read model and its data-source boundary.
//!
//! A future database-backed source can implement `SnapshotSource`. Historical queries
//! should get a separate endpoint and model rather than growing an unbounded snapshot.

use chrono::Utc;
use serde::Serialize;

use crate::{decision::DeviceChange, meter::SharedMeterState};

pub trait SnapshotSource: Send + Sync {
    fn snapshot(&self) -> Result<Snapshot, &'static str>;
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
    pub inferred_power_watts: f64,
    pub devices: Vec<DeviceStatus>,
    pub last_change: Option<Change>,
}

#[derive(Serialize)]
pub struct DeviceStatus {
    pub id: &'static str,
    pub name: &'static str,
    pub nominal_power_watts: f32,
    pub active: bool,
}

#[derive(Serialize)]
pub struct Change {
    pub kind: &'static str,
    pub device_id: &'static str,
    pub device_name: &'static str,
    pub nominal_power_watts: f32,
    pub received_at: String,
}

impl SnapshotSource for LiveMeterSource {
    fn snapshot(&self) -> Result<Snapshot, &'static str> {
        let meter = self
            .meter
            .lock()
            .map_err(|_| "meter state is unavailable")?
            .snapshot();
        Ok(Snapshot {
            schema_version: 1,
            generated_at: Utc::now().to_rfc3339(),
            decision_method: self.decision_method,
            stale_after_seconds: 10,
            readings_received: meter.readings_received,
            last_received_at: meter.last_received_at.map(|time| time.to_rfc3339()),
            total_power_watts: meter.total_power_watts,
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
                    name: device.name,
                    nominal_power_watts: device.power_watts,
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
                    device_name: device.name,
                    nominal_power_watts: device.power_watts,
                    received_at: time.to_rfc3339(),
                })
            }),
        })
    }
}
