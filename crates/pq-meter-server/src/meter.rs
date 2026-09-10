//! Transport-independent, in-memory state. Only accepted readings update this store.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};

use crate::decision::{DecisionMethod, Device, DeviceChange, contains_device};
use crate::input::MeterReading;

pub type SharedMeterState = Arc<Mutex<MeterState>>;

pub struct MeterState {
    catalog: Vec<Device>,
    active_devices: Vec<Device>,
    latest_reading: Option<MeterReading>,
    readings_received: u64,
    last_received_at: Option<DateTime<Utc>>,
    last_change: Option<(DeviceChange, DateTime<Utc>)>,
}

/// An owned, consistent copy lets readers release the lock before formatting a response.
pub struct MeterSnapshot {
    pub catalog: Vec<Device>,
    pub active_devices: Vec<Device>,
    pub total_power_watts: Option<f32>,
    pub latest_reading: Option<MeterReading>,
    pub readings_received: u64,
    pub last_received_at: Option<DateTime<Utc>>,
    pub last_change: Option<(DeviceChange, DateTime<Utc>)>,
}

impl MeterState {
    pub fn new(catalog: Vec<Device>) -> Self {
        Self {
            catalog,
            active_devices: Vec::new(),
            latest_reading: None,
            readings_received: 0,
            last_received_at: None,
            last_change: None,
        }
    }

    pub fn latest_power(&self) -> Option<f32> {
        self.latest_reading.map(|reading| reading.total_power)
    }

    pub fn snapshot(&self) -> MeterSnapshot {
        MeterSnapshot {
            catalog: self.catalog.clone(),
            active_devices: self.active_devices.clone(),
            total_power_watts: self.latest_power(),
            latest_reading: self.latest_reading,
            readings_received: self.readings_received,
            last_received_at: self.last_received_at,
            last_change: self.last_change,
        }
    }

    pub fn apply_reading(
        &mut self,
        reading: MeterReading,
        decision_method: &mut dyn DecisionMethod,
    ) -> DeviceChange {
        let change = decision_method.decide(
            self.latest_power(),
            reading.total_power,
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
        self.latest_reading = Some(reading);
        self.readings_received += 1;
        self.last_received_at = Some(now);
        change
    }
}
