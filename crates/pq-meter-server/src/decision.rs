//! Device catalog and interchangeable power-change decision methods.

use crate::input::MeterReading;

/// Dummy device table with multi-feature electrical profiles (Active Power, Reactive Power, THD).
pub const DUMMY_DEVICE_CATALOG: &[Device] = &[
    Device::new("baseline-2-raspberry-pi", "Baseline", 23.0),
    Device::with_pq("noah-iphone", "Iphone", 65.0, -18.0, 65.0),
    Device::with_pq("noah-macbook", "Macbook Air", 145.0, -15.0, 22.0),
    Device::with_pq("chris-handy", "Smartphone", 60.0, -12.0, 75.0),
    Device::with_pq("peter-laptop", "Laptop", 68.0, -6.0, 25.0),
];

/// A device whose presence can be inferred from its electrical signature.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Device {
    pub id: &'static str,
    pub name: &'static str,
    pub power_watts: f32,
    pub reactive_power_var: Option<f32>,
    pub thd_current_pct: Option<f32>,
    pub cos_phi: Option<f32>,
}

impl Device {
    /// Creates a device defined only by its real power consumption.
    pub const fn new(id: &'static str, name: &'static str, power_watts: f32) -> Self {
        Self {
            id,
            name,
            power_watts,
            reactive_power_var: None,
            thd_current_pct: None,
            cos_phi: None,
        }
    }

    /// Creates a device with multi-feature power quality characteristics:
    /// active power ($P$), reactive power ($Q$), and total current harmonic distortion ($\text{THD}_I$).
    pub const fn with_pq(
        id: &'static str,
        name: &'static str,
        power_watts: f32,
        reactive_power_var: f32,
        thd_current_pct: f32,
    ) -> Self {
        Self {
            id,
            name,
            power_watts,
            reactive_power_var: Some(reactive_power_var),
            thd_current_pct: Some(thd_current_pct),
            cos_phi: None,
        }
    }

    /// Creates a device with full power quality characteristics including power factor ($\cos\phi$).
    #[allow(dead_code)]
    pub const fn with_pq_full(
        id: &'static str,
        name: &'static str,
        power_watts: f32,
        reactive_power_var: f32,
        thd_current_pct: f32,
        cos_phi: f32,
    ) -> Self {
        Self {
            id,
            name,
            power_watts,
            reactive_power_var: Some(reactive_power_var),
            thd_current_pct: Some(thd_current_pct),
            cos_phi: Some(cos_phi),
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

/// Multi-feature electrical delta between two measurement states.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PqDelta {
    pub delta_power_watts: f32,
    pub delta_reactive_var: Option<f32>,
    pub current_thd_pct: Option<f32>,
    pub current_cos_phi: Option<f32>,
}

/// Pluggable policy for translating meter readings into device changes.
///
/// A method may keep internal state, which allows implementations such as
/// [`SettledPowerMatch`] to observe several readings before making a decision.
pub trait DecisionMethod: Send + Sync {
    /// Convenience method for power-only callers and unit tests.
    #[allow(dead_code)]
    fn decide(
        &mut self,
        previous_total_power: Option<f32>,
        total_power: f32,
        catalog: &[Device],
        active_devices: &[Device],
    ) -> DeviceChange {
        let prev = previous_total_power.map(MeterReading::from);
        let curr = MeterReading::from(total_power);
        self.decide_reading(prev.as_ref(), &curr, catalog, active_devices)
    }

    /// Full multi-dimensional decision method receiving complete meter readings.
    fn decide_reading(
        &mut self,
        previous: Option<&MeterReading>,
        current: &MeterReading,
        catalog: &[Device],
        active_devices: &[Device],
    ) -> DeviceChange;
}

/// Immediately selects the device closest in (P, Q, THD) space to the change from the previous reading.
pub struct ClosestPowerMatch {
    device_match_tolerance_watts: f32,
    reactive_match_tolerance_var: f32,
    thd_match_tolerance_pct: f32,
}

impl ClosestPowerMatch {
    pub fn new(device_match_tolerance_watts: f32) -> Self {
        Self::with_pq_tolerances(device_match_tolerance_watts, 15.0, 30.0)
    }

    pub fn with_pq_tolerances(
        device_match_tolerance_watts: f32,
        reactive_match_tolerance_var: f32,
        thd_match_tolerance_pct: f32,
    ) -> Self {
        Self {
            device_match_tolerance_watts: device_match_tolerance_watts.max(0.0),
            reactive_match_tolerance_var: reactive_match_tolerance_var.max(0.0),
            thd_match_tolerance_pct: thd_match_tolerance_pct.max(0.0),
        }
    }
}

impl DecisionMethod for ClosestPowerMatch {
    fn decide_reading(
        &mut self,
        previous: Option<&MeterReading>,
        current: &MeterReading,
        catalog: &[Device],
        active_devices: &[Device],
    ) -> DeviceChange {
        let Some(previous) = previous else {
            return DeviceChange::None;
        };

        let delta_power = current.total_power - previous.total_power;
        let delta_reactive = match (current.l1, previous.l1) {
            (Some(curr_l1), Some(prev_l1)) => {
                Some(curr_l1.reactive_power_var - prev_l1.reactive_power_var)
            }
            _ => None,
        };
        let current_thd = current.l1.map(|l| l.thd_current_pct);
        let current_cos_phi = current.l1.map(|l| l.cos_phi);

        let delta = PqDelta {
            delta_power_watts: delta_power,
            delta_reactive_var: delta_reactive,
            current_thd_pct: current_thd,
            current_cos_phi,
        };

        match_pq_delta(
            delta,
            self.device_match_tolerance_watts,
            self.reactive_match_tolerance_var,
            self.thd_match_tolerance_pct,
            catalog,
            active_devices,
        )
    }
}

/// Waits for changed power quality levels to settle before matching against the catalog.
///
/// Consecutive readings count as stable when they stay within
/// `settle_tolerance_watts` of their running average. Once `required_stable_readings` have
/// arrived, the differences in active power, reactive power, and harmonic distortion
/// are matched against multi-dimensional device signatures.
pub struct SettledPowerMatch {
    minimum_change_watts: f32,
    settle_tolerance_watts: f32,
    required_stable_readings: usize,
    device_match_tolerance_watts: f32,
    reactive_match_tolerance_var: f32,
    thd_match_tolerance_pct: f32,
    settled_reading: Option<SettledSnapshot>,
    candidate: Option<SettlingCandidate>,
}

#[derive(Clone, Copy, Debug)]
struct SettledSnapshot {
    total_power: f32,
    reactive_power: Option<f32>,
}

#[derive(Clone, Copy, Debug)]
struct SettlingCandidate {
    average_power: f32,
    average_reactive: Option<f32>,
    average_thd: Option<f32>,
    average_cos_phi: Option<f32>,
    sample_count: usize,
}

impl SettledPowerMatch {
    pub fn new(
        minimum_change_watts: f32,
        settle_tolerance_watts: f32,
        required_stable_readings: usize,
        device_match_tolerance_watts: f32,
    ) -> Self {
        Self::with_pq_tolerances(
            minimum_change_watts,
            settle_tolerance_watts,
            required_stable_readings,
            device_match_tolerance_watts,
            15.0,
            30.0,
        )
    }

    pub fn with_pq_tolerances(
        minimum_change_watts: f32,
        settle_tolerance_watts: f32,
        required_stable_readings: usize,
        device_match_tolerance_watts: f32,
        reactive_match_tolerance_var: f32,
        thd_match_tolerance_pct: f32,
    ) -> Self {
        Self {
            minimum_change_watts: minimum_change_watts.max(0.0),
            settle_tolerance_watts: settle_tolerance_watts.max(0.0),
            required_stable_readings: required_stable_readings.max(1),
            device_match_tolerance_watts: device_match_tolerance_watts.max(0.0),
            reactive_match_tolerance_var: reactive_match_tolerance_var.max(0.0),
            thd_match_tolerance_pct: thd_match_tolerance_pct.max(0.0),
            settled_reading: None,
            candidate: None,
        }
    }

    #[allow(dead_code)]
    pub fn settled_total_power(&self) -> Option<f32> {
        self.settled_reading.map(|s| s.total_power)
    }

    fn update_candidate(&mut self, reading: &MeterReading) {
        let total_power = reading.total_power;
        let reactive = reading.l1.map(|l| l.reactive_power_var);
        let thd = reading.l1.map(|l| l.thd_current_pct);
        let cos_phi = reading.l1.map(|l| l.cos_phi);

        self.candidate = match self.candidate {
            Some(candidate)
                if (total_power - candidate.average_power).abs() <= self.settle_tolerance_watts =>
            {
                let sample_count = candidate.sample_count + 1;
                let count_f32 = sample_count as f32;
                let average_power =
                    candidate.average_power + (total_power - candidate.average_power) / count_f32;
                let average_reactive = match (candidate.average_reactive, reactive) {
                    (Some(avg_q), Some(curr_q)) => Some(avg_q + (curr_q - avg_q) / count_f32),
                    (None, curr) => curr,
                    (curr, None) => curr,
                };
                let average_thd = match (candidate.average_thd, thd) {
                    (Some(avg_thd), Some(curr_thd)) => {
                        Some(avg_thd + (curr_thd - avg_thd) / count_f32)
                    }
                    (None, curr) => curr,
                    (curr, None) => curr,
                };
                let average_cos_phi = match (candidate.average_cos_phi, cos_phi) {
                    (Some(avg_pf), Some(curr_pf)) => Some(avg_pf + (curr_pf - avg_pf) / count_f32),
                    (None, curr) => curr,
                    (curr, None) => curr,
                };
                Some(SettlingCandidate {
                    average_power,
                    average_reactive,
                    average_thd,
                    average_cos_phi,
                    sample_count,
                })
            }
            _ => Some(SettlingCandidate {
                average_power: total_power,
                average_reactive: reactive,
                average_thd: thd,
                average_cos_phi: cos_phi,
                sample_count: 1,
            }),
        };
    }
}

impl DecisionMethod for SettledPowerMatch {
    fn decide_reading(
        &mut self,
        _previous: Option<&MeterReading>,
        current: &MeterReading,
        catalog: &[Device],
        active_devices: &[Device],
    ) -> DeviceChange {
        let Some(settled) = self.settled_reading else {
            self.settled_reading = Some(SettledSnapshot {
                total_power: current.total_power,
                reactive_power: current.l1.map(|l| l.reactive_power_var),
            });
            return DeviceChange::None;
        };

        if (current.total_power - settled.total_power).abs() < self.minimum_change_watts {
            // Follow small background fluctuations without treating them as state changes.
            self.settled_reading = Some(SettledSnapshot {
                total_power: current.total_power,
                reactive_power: current
                    .l1
                    .map(|l| l.reactive_power_var)
                    .or(settled.reactive_power),
            });
            self.candidate = None;
            return DeviceChange::None;
        }

        self.update_candidate(current);
        let candidate = self.candidate.expect("candidate was just initialized");
        if candidate.sample_count < self.required_stable_readings {
            return DeviceChange::None;
        }

        self.candidate = None;
        let delta_power = candidate.average_power - settled.total_power;
        let delta_reactive = match (candidate.average_reactive, settled.reactive_power) {
            (Some(curr_q), Some(prev_q)) => Some(curr_q - prev_q),
            _ => None,
        };

        self.settled_reading = Some(SettledSnapshot {
            total_power: candidate.average_power,
            reactive_power: candidate.average_reactive,
        });

        let delta = PqDelta {
            delta_power_watts: delta_power,
            delta_reactive_var: delta_reactive,
            current_thd_pct: candidate.average_thd,
            current_cos_phi: candidate.average_cos_phi,
        };

        match_pq_delta(
            delta,
            self.device_match_tolerance_watts,
            self.reactive_match_tolerance_var,
            self.thd_match_tolerance_pct,
            catalog,
            active_devices,
        )
    }
}

/// Matches a multi-dimensional electrical delta against catalog candidates using P, Q, and THD.
pub fn match_pq_delta(
    delta: PqDelta,
    power_tolerance_watts: f32,
    reactive_tolerance_var: f32,
    thd_tolerance_pct: f32,
    catalog: &[Device],
    active_devices: &[Device],
) -> DeviceChange {
    let is_addition = delta.delta_power_watts > 0.0;
    let is_removal = delta.delta_power_watts < 0.0;

    if !is_addition && !is_removal {
        return DeviceChange::None;
    }

    let candidates: Box<dyn Iterator<Item = Device> + '_> = if is_addition {
        Box::new(
            catalog
                .iter()
                .copied()
                .filter(|candidate| !contains_device(active_devices, candidate.id)),
        )
    } else {
        Box::new(active_devices.iter().copied())
    };

    let expected_power = delta.delta_power_watts.abs();
    let mut best_match: Option<(Device, f32)> = None;

    for candidate in candidates {
        let power_diff = (candidate.power_watts - expected_power).abs();
        if power_diff > power_tolerance_watts {
            continue;
        }

        let norm_power = if power_tolerance_watts > 0.0 {
            power_diff / power_tolerance_watts
        } else {
            0.0
        };

        let mut norm_reactive = 0.0;
        if let (Some(expected_q), Some(measured_delta_q)) =
            (candidate.reactive_power_var, delta.delta_reactive_var)
        {
            let target_delta_q = if is_addition { expected_q } else { -expected_q };
            let q_diff = (measured_delta_q - target_delta_q).abs();
            norm_reactive = if reactive_tolerance_var > 0.0 {
                q_diff / reactive_tolerance_var
            } else {
                0.0
            };
        }

        let mut norm_thd = 0.0;
        if let (true, Some(expected_thd), Some(measured_thd)) = (
            is_addition,
            candidate.thd_current_pct,
            delta.current_thd_pct,
        ) {
            let thd_diff = (measured_thd - expected_thd).abs();
            norm_thd = if thd_tolerance_pct > 0.0 {
                thd_diff / thd_tolerance_pct
            } else {
                0.0
            };
        }

        let composite_score = norm_power + 0.4 * norm_reactive + 0.2 * norm_thd;

        match best_match {
            Some((_, best_score)) if composite_score < best_score => {
                best_match = Some((candidate, composite_score));
            }
            None => {
                best_match = Some((candidate, composite_score));
            }
            _ => {}
        }
    }

    match best_match {
        Some((device, _)) => {
            if is_addition {
                DeviceChange::Added(device)
            } else {
                DeviceChange::Removed(device)
            }
        }
        None => DeviceChange::None,
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

        assert_eq!(
            method.decide(None, 100.0, TEST_CATALOG, &[]),
            DeviceChange::None
        );
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
        assert_eq!(
            method.decide(None, 100.0, TEST_CATALOG, &[]),
            DeviceChange::None
        );

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

    fn make_reading(power: f32, reactive: f32, thd: f32) -> MeterReading {
        MeterReading {
            total_power: power,
            systime: Some(100),
            frequency_hz: Some(50.0),
            l1: Some(crate::input::L1Reading {
                voltage_v: 230.0,
                current_a: power / 230.0,
                real_power_w: power,
                apparent_power_va: (power * power + reactive * reactive).sqrt(),
                reactive_power_var: reactive,
                cos_phi: power / (power * power + reactive * reactive).sqrt().max(0.001),
                real_energy_consumed_wh: 1000.0,
                thd_current_pct: thd,
            }),
        }
    }

    #[test]
    fn multi_feature_distinguishes_devices_with_identical_wattage_using_reactive_power() {
        const CHARGER: Device = Device::with_pq("charger", "Charger", 65.0, -18.0, 65.0);
        const RESISTIVE_LAMP: Device = Device::with_pq("lamp", "Lamp", 65.0, 0.0, 2.0);
        let catalog = &[CHARGER, RESISTIVE_LAMP];

        let mut method = ClosestPowerMatch::new(5.0);

        let base = make_reading(100.0, 0.0, 2.0);
        let charger_on = make_reading(165.0, -18.0, 65.0);
        let lamp_on = make_reading(165.0, 0.0, 2.0);

        // Power delta is identical (+65.0 W in both cases), but Q and THD disambiguate
        let change1 = method.decide_reading(Some(&base), &charger_on, catalog, &[]);
        assert_eq!(change1, DeviceChange::Added(CHARGER));

        let change2 = method.decide_reading(Some(&base), &lamp_on, catalog, &[]);
        assert_eq!(change2, DeviceChange::Added(RESISTIVE_LAMP));
    }

    #[test]
    fn multi_feature_disambiguates_laptop_vs_iphone_based_on_thd_and_q() {
        const IPHONE: Device = Device::with_pq("iphone", "Iphone", 65.0, -18.0, 65.0);
        const LAPTOP: Device = Device::with_pq("laptop", "Laptop", 68.0, -6.0, 25.0);
        let catalog = &[IPHONE, LAPTOP];

        let mut method = ClosestPowerMatch::new(5.0);

        let base = make_reading(100.0, -10.0, 5.0);
        // Delta power = +66.0 W (closer to 65.0 W iPhone than 68.0 W Laptop in Watts)
        // BUT delta Q = -6.0 var and THD = 25.0% match the Laptop profile!
        let reading = make_reading(166.0, -16.0, 25.0);

        let change = method.decide_reading(Some(&base), &reading, catalog, &[]);
        assert_eq!(change, DeviceChange::Added(LAPTOP));
    }

    #[test]
    fn multi_feature_removal_matches_negative_reactive_delta() {
        const IPHONE: Device = Device::with_pq("iphone", "Iphone", 65.0, -18.0, 65.0);
        let catalog = &[IPHONE];

        let mut method = ClosestPowerMatch::new(5.0);

        let active_state = make_reading(165.0, -28.0, 65.0);
        // iPhone turns off: power drops by 65 W, reactive power rises by +18 var back to -10 var
        let turned_off = make_reading(100.0, -10.0, 5.0);

        let change = method.decide_reading(Some(&active_state), &turned_off, catalog, &[IPHONE]);
        assert_eq!(change, DeviceChange::Removed(IPHONE));
    }

    #[test]
    fn multi_feature_settled_averages_pq_metrics_and_matches() {
        const IPHONE: Device = Device::with_pq("iphone", "Iphone", 65.0, -18.0, 65.0);
        const LAPTOP: Device = Device::with_pq("laptop", "Laptop", 68.0, -6.0, 25.0);
        let catalog = &[IPHONE, LAPTOP];

        let mut method = SettledPowerMatch::new(5.0, 3.0, 2, 5.0);

        let baseline = make_reading(100.0, 0.0, 2.0);
        assert_eq!(
            method.decide_reading(None, &baseline, catalog, &[]),
            DeviceChange::None
        );

        // First reading of laptop turn-on: ~167 W, -6 var, 24% THD
        let r1 = make_reading(167.0, -6.0, 24.0);
        assert_eq!(
            method.decide_reading(Some(&baseline), &r1, catalog, &[]),
            DeviceChange::None
        );

        // Second stable reading: ~168 W, -6.2 var, 25% THD (settles)
        let r2 = make_reading(168.0, -6.2, 25.0);
        let change = method.decide_reading(Some(&r1), &r2, catalog, &[]);
        assert_eq!(change, DeviceChange::Added(LAPTOP));
    }
}
