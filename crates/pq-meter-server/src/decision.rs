//! Device catalog and interchangeable power-change decision methods.

/// Dummy device table. Change these entries without touching either decision algorithm.
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

/// The state change inferred from total-power readings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DeviceChange {
    Added(Device),
    Removed(Device),
    None,
}

/// Pluggable policy for translating total-power readings into device changes.
///
/// A method may keep internal state, which allows implementations such as
/// [`SettledPowerMatch`] to observe several readings before making a decision.
pub trait DecisionMethod: Send + Sync {
    fn decide(
        &mut self,
        previous_total_power: Option<f32>,
        total_power: f32,
        catalog: &[Device],
        active_devices: &[Device],
    ) -> DeviceChange;
}

/// Immediately selects the device closest to the change from the previous reading.
pub struct ClosestPowerMatch {
    device_match_tolerance_watts: f32,
}

impl ClosestPowerMatch {
    pub fn new(device_match_tolerance_watts: f32) -> Self {
        Self {
            device_match_tolerance_watts: device_match_tolerance_watts.max(0.0),
        }
    }
}

impl DecisionMethod for ClosestPowerMatch {
    fn decide(
        &mut self,
        previous_total_power: Option<f32>,
        total_power: f32,
        catalog: &[Device],
        active_devices: &[Device],
    ) -> DeviceChange {
        let Some(previous_total_power) = previous_total_power else {
            return DeviceChange::None;
        };

        match_power_delta(
            total_power - previous_total_power,
            self.device_match_tolerance_watts,
            catalog,
            active_devices,
        )
    }
}

/// Waits for a changed power level to settle before matching it to a device.
///
/// Consecutive readings count as stable when they stay within
/// `settle_tolerance_watts` of their running average. Once `required_stable_readings` have
/// arrived, the difference from the last settled level is matched against the catalog.
pub struct SettledPowerMatch {
    minimum_change_watts: f32,
    settle_tolerance_watts: f32,
    required_stable_readings: usize,
    device_match_tolerance_watts: f32,
    settled_total_power: Option<f32>,
    candidate: Option<SettlingCandidate>,
}

#[derive(Clone, Copy, Debug)]
struct SettlingCandidate {
    average_power: f32,
    sample_count: usize,
}

impl SettledPowerMatch {
    pub fn new(
        minimum_change_watts: f32,
        settle_tolerance_watts: f32,
        required_stable_readings: usize,
        device_match_tolerance_watts: f32,
    ) -> Self {
        Self {
            minimum_change_watts: minimum_change_watts.max(0.0),
            settle_tolerance_watts: settle_tolerance_watts.max(0.0),
            required_stable_readings: required_stable_readings.max(1),
            device_match_tolerance_watts: device_match_tolerance_watts.max(0.0),
            settled_total_power: None,
            candidate: None,
        }
    }

    fn update_candidate(&mut self, total_power: f32) {
        self.candidate = match self.candidate {
            Some(candidate)
                if (total_power - candidate.average_power).abs() <= self.settle_tolerance_watts =>
            {
                let sample_count = candidate.sample_count + 1;
                let average_power = candidate.average_power
                    + (total_power - candidate.average_power) / sample_count as f32;
                Some(SettlingCandidate {
                    average_power,
                    sample_count,
                })
            }
            _ => Some(SettlingCandidate {
                average_power: total_power,
                sample_count: 1,
            }),
        };
    }
}

impl DecisionMethod for SettledPowerMatch {
    fn decide(
        &mut self,
        _previous_total_power: Option<f32>,
        total_power: f32,
        catalog: &[Device],
        active_devices: &[Device],
    ) -> DeviceChange {
        let Some(settled_total_power) = self.settled_total_power else {
            self.settled_total_power = Some(total_power);
            return DeviceChange::None;
        };

        if (total_power - settled_total_power).abs() < self.minimum_change_watts {
            // Follow small background fluctuations without treating them as state changes.
            self.settled_total_power = Some(total_power);
            self.candidate = None;
            return DeviceChange::None;
        }

        self.update_candidate(total_power);
        let candidate = self.candidate.expect("candidate was just initialized");
        if candidate.sample_count < self.required_stable_readings {
            return DeviceChange::None;
        }

        self.candidate = None;
        self.settled_total_power = Some(candidate.average_power);
        match_power_delta(
            candidate.average_power - settled_total_power,
            self.device_match_tolerance_watts,
            catalog,
            active_devices,
        )
    }
}

fn match_power_delta(
    delta: f32,
    device_match_tolerance_watts: f32,
    catalog: &[Device],
    active_devices: &[Device],
) -> DeviceChange {
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
        Some(device)
            if (device.power_watts - expected_power).abs() <= device_match_tolerance_watts =>
        {
            if delta > 0.0 {
                DeviceChange::Added(device)
            } else {
                DeviceChange::Removed(device)
            }
        }
        _ => DeviceChange::None,
    }
}

pub fn contains_device(devices: &[Device], id: &str) -> bool {
    devices.iter().any(|device| device.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_DEVICE: Device = Device::new("test-device", "Test device", 60.0);
    const TEST_CATALOG: &[Device] = &[TEST_DEVICE];

    #[test]
    fn closest_match_uses_the_previous_reading_immediately() {
        let mut method = ClosestPowerMatch::new(5.0);

        assert_eq!(
            method.decide(Some(100.0), 162.0, TEST_CATALOG, &[]),
            DeviceChange::Added(TEST_DEVICE)
        );
    }

    #[test]
    fn settled_match_waits_for_three_readings_within_three_watts() {
        let mut method = SettledPowerMatch::new(5.0, 3.0, 3, 5.0);

        assert_eq!(
            method.decide(None, 100.0, TEST_CATALOG, &[]),
            DeviceChange::None
        );
        assert_eq!(
            method.decide(Some(100.0), 145.0, TEST_CATALOG, &[]),
            DeviceChange::None
        );
        assert_eq!(
            method.decide(Some(145.0), 159.0, TEST_CATALOG, &[]),
            DeviceChange::None
        );
        assert_eq!(
            method.decide(Some(159.0), 161.0, TEST_CATALOG, &[]),
            DeviceChange::None
        );
        assert_eq!(
            method.decide(Some(161.0), 160.0, TEST_CATALOG, &[]),
            DeviceChange::Added(TEST_DEVICE)
        );
    }

    #[test]
    fn settled_match_ignores_changes_below_five_watts() {
        let mut method = SettledPowerMatch::new(5.0, 3.0, 3, 5.0);

        assert_eq!(
            method.decide(None, 100.0, TEST_CATALOG, &[]),
            DeviceChange::None
        );
        for reading in [102.0, 104.0, 101.0, 103.0] {
            assert_eq!(
                method.decide(Some(100.0), reading, TEST_CATALOG, &[]),
                DeviceChange::None
            );
        }
    }

    #[test]
    fn settled_match_only_removes_an_active_device() {
        let mut method = SettledPowerMatch::new(5.0, 3.0, 2, 5.0);

        method.decide(None, 160.0, TEST_CATALOG, &[TEST_DEVICE]);
        assert_eq!(
            method.decide(Some(160.0), 101.0, TEST_CATALOG, &[TEST_DEVICE]),
            DeviceChange::None
        );
        assert_eq!(
            method.decide(Some(101.0), 100.0, TEST_CATALOG, &[TEST_DEVICE]),
            DeviceChange::Removed(TEST_DEVICE)
        );
    }
}
