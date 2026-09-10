//! Transport-independent, in-memory state. Only accepted readings update this store.

use std::{sync::{Arc, Mutex}, time::SystemTime};

use chrono::{DateTime, Utc};

use crate::{
    decision::{DecisionMethod, Device, DeviceChange, contains_device},
    history::History,
    input::MeterReading,
};

pub type SharedMeterState = Arc<Mutex<MeterState>>;

pub struct MeterState {
    catalog: Vec<Device>,
    active_devices: Vec<Device>,
    latest_reading: Option<MeterReading>,
    history: History<MeterReading>,
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
            history: History::new(),
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
        let change = decision_method.decide_reading(
            self.latest_reading.as_ref(),
            &reading,
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
        self.history.push(reading, SystemTime::from(now));
        self.readings_received += 1;
        self.last_received_at = Some(now);
        change
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decision::{ClosestPowerMatch, DUMMY_DEVICE_CATALOG};

    #[test]
    fn initial_state_is_empty() {
        let state = MeterState::new(DUMMY_DEVICE_CATALOG.to_vec());
        assert_eq!(state.latest_power(), None);
        let snapshot = state.snapshot();
        assert_eq!(snapshot.readings_received, 0);
        assert_eq!(snapshot.last_received_at, None);
        assert_eq!(snapshot.last_change, None);
        assert_eq!(snapshot.total_power_watts, None);
        assert!(snapshot.active_devices.is_empty());
        assert_eq!(snapshot.catalog.len(), DUMMY_DEVICE_CATALOG.len());
    }

    #[test]
    fn apply_reading_tracks_power_and_increments_count() {
        let mut state = MeterState::new(DUMMY_DEVICE_CATALOG.to_vec());
        let mut method = ClosestPowerMatch::new(3.0);

        state.apply_reading(100.0.into(), &mut method);
        assert_eq!(state.latest_power(), Some(100.0));
        assert_eq!(state.snapshot().readings_received, 1);
        assert!(state.snapshot().last_received_at.is_some());

        state.apply_reading(150.0.into(), &mut method);
        assert_eq!(state.latest_power(), Some(150.0));
        assert_eq!(state.snapshot().readings_received, 2);
    }

    #[test]
    fn preserves_last_change_when_subsequent_reading_produces_no_change() {
        let mut state = MeterState::new(DUMMY_DEVICE_CATALOG.to_vec());
        let mut method = ClosestPowerMatch::new(3.0);

        // Baseline (DeviceChange::None)
        state.apply_reading(100.0.into(), &mut method);
        assert!(state.snapshot().last_change.is_none());

        // Addition
        let change = state.apply_reading(123.0.into(), &mut method);
        assert!(matches!(change, DeviceChange::Added(_)));
        let added_at = state.snapshot().last_change.unwrap().1;

        // No change reading: last_change timestamp and change must be preserved
        state.apply_reading(123.0.into(), &mut method);
        let current_last_change = state.snapshot().last_change.unwrap();
        assert_eq!(current_last_change.1, added_at);
        assert_eq!(current_last_change.0, change);
    }
}
