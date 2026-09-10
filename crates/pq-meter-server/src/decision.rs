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

    #[test]
    fn closest_match_returns_none_on_first_reading_or_zero_delta() {
        let mut method = ClosestPowerMatch::new(5.0);

        assert_eq!(method.decide(None, 100.0, TEST_CATALOG, &[]), DeviceChange::None);
        assert_eq!(
            method.decide(Some(100.0), 100.0, TEST_CATALOG, &[]),
            DeviceChange::None
        );
    }

    #[test]
    fn closest_match_picks_closest_device_among_multiple() {
        const D1: Device = Device::new("d1", "Device 1", 50.0);
        const D2: Device = Device::new("d2", "Device 2", 70.0);
        let catalog = &[D1, D2];
        let mut method = ClosestPowerMatch::new(10.0);

        // Delta is 52.0 (closer to 50.0 than 70.0)
        assert_eq!(
            method.decide(Some(100.0), 152.0, catalog, &[]),
            DeviceChange::Added(D1)
        );
        // Delta is 68.0 (closer to 70.0 than 50.0)
        assert_eq!(
            method.decide(Some(100.0), 168.0, catalog, &[]),
            DeviceChange::Added(D2)
        );
    }

    #[test]
    fn closest_match_ignores_delta_exceeding_tolerance() {
        let mut method = ClosestPowerMatch::new(5.0);
        // Delta is 70.0, target is 60.0, diff 10.0 > tolerance 5.0
        assert_eq!(
            method.decide(Some(100.0), 170.0, TEST_CATALOG, &[]),
            DeviceChange::None
        );
        // Delta is 50.0, target is 60.0, diff 10.0 > tolerance 5.0
        assert_eq!(
            method.decide(Some(100.0), 150.0, TEST_CATALOG, &[]),
            DeviceChange::None
        );
    }

    #[test]
    fn closest_match_does_not_re_add_active_device_and_removes_it() {
        let mut method = ClosestPowerMatch::new(5.0);

        // Already active: should not be re-added even if delta matches
        assert_eq!(
            method.decide(Some(100.0), 160.0, TEST_CATALOG, &[TEST_DEVICE]),
            DeviceChange::None
        );

        // Inactive: should not be removed even if negative delta matches
        assert_eq!(
            method.decide(Some(160.0), 100.0, TEST_CATALOG, &[]),
            DeviceChange::None
        );

        // Active: removed when negative delta matches
        assert_eq!(
            method.decide(Some(160.0), 100.0, TEST_CATALOG, &[TEST_DEVICE]),
            DeviceChange::Removed(TEST_DEVICE)
        );
    }

    #[test]
    fn settled_match_resets_candidate_on_unstable_fluctuations() {
        let mut method = SettledPowerMatch::new(5.0, 3.0, 3, 5.0);

        // Baseline
        assert_eq!(method.decide(None, 100.0, TEST_CATALOG, &[]), DeviceChange::None);

        // Jump to 160 (reading 1)
        assert_eq!(
            method.decide(Some(100.0), 160.0, TEST_CATALOG, &[]),
            DeviceChange::None
        );
        // Reading 2 within +/- 3W
        assert_eq!(
            method.decide(Some(160.0), 162.0, TEST_CATALOG, &[]),
            DeviceChange::None
        );
        // Reading 3 jumps far away to 180 (exceeds settle_tolerance_watts 3.0), resets candidate!
        assert_eq!(
            method.decide(Some(162.0), 180.0, TEST_CATALOG, &[]),
            DeviceChange::None
        );
        // Reading 4 (now count 2 at ~180)
        assert_eq!(
            method.decide(Some(180.0), 181.0, TEST_CATALOG, &[]),
            DeviceChange::None
        );
        // Reading 5 (count 3 at ~180, settled delta = 80.33, TEST_DEVICE is 60W, tolerance 5W -> no match!)
        assert_eq!(
            method.decide(Some(181.0), 180.0, TEST_CATALOG, &[]),
            DeviceChange::None
        );
    }

    #[test]
    fn contains_device_helper() {
        assert!(!contains_device(&[], "test-device"));
        assert!(contains_device(&[TEST_DEVICE], "test-device"));
        assert!(!contains_device(&[TEST_DEVICE], "other-device"));
    }

    #[test]
    fn threshold_exact_boundaries() {
        let mut method = SettledPowerMatch::new(5.0, 3.0, 2, 5.0);
        method.decide(None, 100.0, TEST_CATALOG, &[]); // baseline = 100.0

        // Delta of 4.9 W (< 5.0 W minimum_change_watts) should be absorbed as baseline drift
        assert_eq!(
            method.decide(Some(100.0), 104.9, TEST_CATALOG, &[]),
            DeviceChange::None
        );
        // Settled baseline is now 104.9.
        // A delta of exactly 5.0 W (109.9 W) is NOT < 5.0, so it initiates candidate settling
        assert_eq!(
            method.decide(Some(104.9), 109.9, TEST_CATALOG, &[]),
            DeviceChange::None
        );
    }

    #[test]
    fn threshold_settle_tolerance_boundary() {
        let mut method = SettledPowerMatch::new(5.0, 3.0, 3, 5.0);
        method.decide(None, 100.0, TEST_CATALOG, &[]);

        // Reading 1: 160.0 (jump of 60 W, candidate average 160.0, count 1)
        method.decide(Some(100.0), 160.0, TEST_CATALOG, &[]);

        // Reading 2: 163.0 (diff from 160.0 is exactly 3.0, <= settle_tolerance_watts 3.0)
        // Stays within tolerance: running average = 160 + 3/2 = 161.5, count 2
        method.decide(Some(160.0), 163.0, TEST_CATALOG, &[]);

        // Reading 3: 164.5 (diff from 161.5 is 3.0, <= 3.0)
        // Reaches 3 stable readings: average = 161.5 + 3/3 = 162.5
        // Settled delta = 162.5 - 100.0 = 62.5 W
        // TEST_DEVICE is 60.0 W, diff 2.5 <= 5.0 W match tolerance -> Added!
        assert_eq!(
            method.decide(Some(163.0), 164.5, TEST_CATALOG, &[]),
            DeviceChange::Added(TEST_DEVICE)
        );
    }

    #[test]
    fn threshold_device_match_tolerance_boundary() {
        let mut method = SettledPowerMatch::new(5.0, 1.0, 2, 5.0);
        method.decide(None, 100.0, TEST_CATALOG, &[]);

        // Delta is 65.0 W (diff from 60.0 W is exactly 5.0 W == tolerance 5.0) -> MATCHES
        method.decide(Some(100.0), 165.0, TEST_CATALOG, &[]);
        assert_eq!(
            method.decide(Some(165.0), 165.0, TEST_CATALOG, &[]),
            DeviceChange::Added(TEST_DEVICE)
        );

        // Reset with new method
        let mut method2 = SettledPowerMatch::new(5.0, 1.0, 2, 5.0);
        method2.decide(None, 100.0, TEST_CATALOG, &[]);

        // Delta is 65.1 W (diff from 60.0 W is 5.1 W > tolerance 5.0) -> FAILS TO MATCH
        method2.decide(Some(100.0), 165.1, TEST_CATALOG, &[]);
        assert_eq!(
            method2.decide(Some(165.1), 165.1, TEST_CATALOG, &[]),
            DeviceChange::None
        );
    }

    #[test]
    fn threshold_creeping_power_drift_is_absorbed_without_detection() {
        let mut method = SettledPowerMatch::new(5.0, 3.0, 2, 5.0);
        method.decide(None, 100.0, TEST_CATALOG, &[]);

        // Slow ramp 2 W at a time from 100 to 160 W (cumulative +60 W, matching TEST_DEVICE)
        // Since every individual jump is 2 W < 5.0 W (minimum_change_watts), it is absorbed as baseline drift
        for p in (102..=160).step_by(2) {
            let change = method.decide(Some((p - 2) as f32), p as f32, TEST_CATALOG, &[]);
            assert_eq!(change, DeviceChange::None);
        }
    }

    #[test]
    fn threshold_distinguishes_close_catalog_devices() {
        let mut method = ClosestPowerMatch::new(5.0);

        // Delta of 63.5 W is within 5W of both Handy (60W, diff 3.5) and iPhone (65W, diff 1.5)
        // Must pick the closest (iPhone)
        let change = method.decide(Some(100.0), 163.5, DUMMY_DEVICE_CATALOG, &[]);
        assert_eq!(change, DeviceChange::Added(DUMMY_DEVICE_CATALOG[1])); // noah-iphone

        // If iPhone is already active, subsequent delta of 63.5W picks the next closest (Handy)
        let active = [DUMMY_DEVICE_CATALOG[1]];
        let change2 = method.decide(Some(163.5), 227.0, DUMMY_DEVICE_CATALOG, &active);
        assert_eq!(change2, DeviceChange::Added(DUMMY_DEVICE_CATALOG[3])); // chris-handy
    }

    #[test]
    fn threshold_device_power_below_minimum_change_cannot_trigger() {
        const LOW_POWER_DEVICE: Device = Device::new("low-power", "Low Power", 3.0);
        let catalog = &[LOW_POWER_DEVICE];
        let mut method = SettledPowerMatch::new(5.0, 1.0, 2, 5.0);

        method.decide(None, 100.0, catalog, &[]);
        // Device turns on with +3.0 W delta (103.0 W)
        // 3.0 < 5.0 (minimum_change_watts), so it is absorbed as baseline fluctuation
        assert_eq!(
            method.decide(Some(100.0), 103.0, catalog, &[]),
            DeviceChange::None
        );
        assert_eq!(
            method.decide(Some(103.0), 103.0, catalog, &[]),
            DeviceChange::None
        );
    }
}
