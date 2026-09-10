//! Transport-independent, in-memory state. Only accepted readings update this store.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};

use crate::decision::{DecisionMethod, Device, DeviceChange, contains_device};

pub type SharedMeterState = Arc<Mutex<MeterState>>;

pub struct MeterState {
    catalog: Vec<Device>,
    active_devices: Vec<Device>,
    previous_total_power: Option<f32>,
    readings_received: u64,
    last_received_at: Option<DateTime<Utc>>,
    last_change: Option<(DeviceChange, DateTime<Utc>)>,
}

/// An owned, consistent copy lets readers release the lock before formatting a response.
pub struct MeterSnapshot {
    pub catalog: Vec<Device>,
    pub active_devices: Vec<Device>,
    pub total_power_watts: Option<f32>,
    pub readings_received: u64,
    pub last_received_at: Option<DateTime<Utc>>,
    pub last_change: Option<(DeviceChange, DateTime<Utc>)>,
}

impl MeterState {
    pub fn new(catalog: Vec<Device>) -> Self {
        Self {
            catalog,
            active_devices: Vec::new(),
            previous_total_power: None,
            readings_received: 0,
            last_received_at: None,
            last_change: None,
        }
    }

    pub fn latest_power(&self) -> Option<f32> {
        self.previous_total_power
    }

    pub fn snapshot(&self) -> MeterSnapshot {
        MeterSnapshot {
            catalog: self.catalog.clone(),
            active_devices: self.active_devices.clone(),
            total_power_watts: self.previous_total_power,
            readings_received: self.readings_received,
            last_received_at: self.last_received_at,
            last_change: self.last_change,
        }
    }

    pub fn apply_reading(
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
        let now = Utc::now();
        match change {
            DeviceChange::Added(device) => {
                if !contains_device(&self.active_devices, device.id) {
                    self.active_devices.push(device);
                }
            }
            DeviceChange::Removed(device) => {
                self.active_devices.retain(|active| active.id != device.id);
            }
            DeviceChange::None => {}
        }
        if change != DeviceChange::None {
            self.last_change = Some((change, now));
        }
        self.previous_total_power = Some(total_power);
        self.readings_received += 1;
        self.last_received_at = Some(now);
        change
    }
}
