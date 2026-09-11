//! Transport-independent, in-memory state. Only accepted readings update this store.

use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use chrono::{DateTime, Utc};

use crate::decision::{DecisionMethod, Device, DeviceChange, contains_device};
use crate::history::{History, HistoryEntry};
use crate::input::MeterReading;
use crate::labels::{DeviceLabels, RenameError};
use crate::transport::GatewayTransport;

/// How much of the recent past the dashboard charts can draw.
pub const HISTORY_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

/// Samples kept for that window. The gateway reads every 200 ms, so this holds the full
/// window even if every single reading crossed the reporting threshold.
const HISTORY_CAPACITY: usize = 600;

pub type SharedMeterState = Arc<Mutex<MeterState>>;

pub struct MeterState {
    labels: DeviceLabels,
    catalog: Vec<Device>,
    active_devices: Vec<Device>,
    latest_reading: Option<MeterReading>,
    readings_received: u64,
    last_received_at: Option<DateTime<Utc>>,
    last_change: Option<(DeviceChange, DateTime<Utc>)>,
    history: History<MeterReading>,
    /// What the gateway last reported about the link it delivers over.
    transport: GatewayTransport,
}

/// An owned, consistent copy lets readers release the lock before formatting a response.
pub struct MeterSnapshot {
    pub device_names: std::collections::BTreeMap<String, String>,
    pub catalog: Vec<Device>,
    pub active_devices: Vec<Device>,
    pub total_power_watts: Option<f32>,
    pub latest_reading: Option<MeterReading>,
    pub readings_received: u64,
    pub last_received_at: Option<DateTime<Utc>>,
    pub last_change: Option<(DeviceChange, DateTime<Utc>)>,
    pub transport: GatewayTransport,
}

impl MeterState {
    #[cfg(test)]
    pub fn new(catalog: Vec<Device>) -> Self {
        Self::with_labels(catalog, DeviceLabels::default())
    }

    pub fn with_labels(catalog: Vec<Device>, labels: DeviceLabels) -> Self {
        Self {
            labels,
            catalog,
            active_devices: Vec::new(),
            latest_reading: None,
            readings_received: 0,
            last_received_at: None,
            last_change: None,
            history: History::bounded(HISTORY_CAPACITY),
            transport: GatewayTransport::default(),
        }
    }

    pub fn rename_device(&mut self, id: &str, name: &str) -> Result<String, RenameError> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
            return Err(RenameError::InvalidName);
        }
        if !self.catalog.iter().any(|device| device.id == id) {
            return Err(RenameError::NotFound);
        }
        // Commit the file before exposing the change to any dashboard client.
        let mut labels = self.labels.clone();
        labels.names.insert(id.to_owned(), name.to_owned());
        labels.save().map_err(|error| {
            tracing::warn!(%error, "could not save device label");
            RenameError::Unavailable
        })?;
        self.labels = labels;
        Ok(name.to_owned())
    }

    /// Records what the gateway reported about its link. Only a report that carries at
    /// least one field replaces the previous one, so a sender that omits the headers does
    /// not blank out what an earlier batch established.
    pub fn record_transport(&mut self, reported: GatewayTransport) {
        if !reported.is_empty() {
            self.transport = reported;
        }
    }

    /// The readings of the last [`HISTORY_WINDOW`], for the dashboard charts.
    pub fn recent_history(&self, now: SystemTime) -> &[HistoryEntry<MeterReading>] {
        self.history
            .since(now.checked_sub(HISTORY_WINDOW).unwrap_or(now))
    }

    pub fn latest_power(&self) -> Option<f32> {
        self.latest_reading.map(|reading| reading.total_power)
    }

    pub fn snapshot(&self) -> MeterSnapshot {
        MeterSnapshot {
            device_names: self.labels.names.clone(),
            catalog: self.catalog.clone(),
            active_devices: self.active_devices.clone(),
            total_power_watts: self.latest_power(),
            latest_reading: self.latest_reading,
            readings_received: self.readings_received,
            last_received_at: self.last_received_at,
            last_change: self.last_change,
            transport: self.transport.clone(),
        }
    }

    pub fn apply_reading(
        &mut self,
        reading: MeterReading,
        decision_method: &mut dyn DecisionMethod,
    ) -> DeviceChange {
        // A keepalive says the gateway saw no change, and may carry a level it is still
        // settling on. Feeding one to the decision method would match an intermediate
        // value against the catalog and then measure the real change from it.
        let change = if reading.heartbeat {
            DeviceChange::None
        } else {
            decision_method.decide_reading(
                self.latest_reading.as_ref(),
                &reading,
                &self.catalog,
                &self.active_devices,
            )
        };

        // Reflect any devices the method has learned at runtime in the live catalog,
        // refreshing the signature of ones already known. Static-catalog methods
        // return nothing here, so this is a no-op for them.
        for learned in decision_method.learned_devices() {
            match self
                .catalog
                .iter_mut()
                .find(|device| device.id == learned.id)
            {
                Some(existing) => *existing = learned,
                None => self.catalog.push(learned),
            }
        }

        let profiles = decision_method.learned_profiles();
        if !profiles.is_empty() {
            self.labels.appliances = profiles;
        }

        let now = Utc::now();
        match change {
            DeviceChange::Added(device) => {
                if !contains_device(&self.catalog, device.id) {
                    self.catalog.push(device);
                }
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
    fn a_keepalive_is_recorded_but_never_infers_a_device() {
        use crate::input::MeterReading;

        // Production settling after the client's own filter: one reading is enough.
        let mut state = MeterState::new(DUMMY_DEVICE_CATALOG.to_vec());
        let mut method = crate::decision::SettledPowerMatch::new(5.0, 3.0, 1, 8.0);

        state.apply_reading(24.0.into(), &mut method);

        // A level the gateway is still settling on can sit anywhere between two levels.
        // Matching it would name the wrong device and, worse, leave the real change to be
        // measured from this intermediate value.
        let intermediate = MeterReading {
            heartbeat: true,
            ..MeterReading::from(89.0)
        };
        assert_eq!(
            state.apply_reading(intermediate, &mut method),
            DeviceChange::None
        );

        // It is still a measurement: it counts, and it is the latest reading.
        assert_eq!(state.latest_power(), Some(89.0));
        assert_eq!(state.snapshot().readings_received, 2);
        assert!(state.snapshot().active_devices.is_empty());

        // The settled change that follows is measured from the last real level (24 W), so
        // the 80 W device that actually switched on is the one that is found. Measured
        // from the intermediate value instead, the delta would have been 15 W and the
        // catalog would have named something else entirely.
        let eighty_watt = DUMMY_DEVICE_CATALOG
            .iter()
            .find(|device| device.id == "device-80w")
            .copied()
            .expect("the catalog carries the 80 W device");
        let change = state.apply_reading(104.0.into(), &mut method);
        assert_eq!(change, DeviceChange::Added(eighty_watt));
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
