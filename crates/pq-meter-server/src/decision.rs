//! Device catalog and interchangeable power-change decision methods.

use crate::input::MeterReading;

/// Dummy device table with multi-feature electrical profiles (Active Power, Reactive Power, THD).
pub const DUMMY_DEVICE_CATALOG: &[Device] = &[
    Device::new("baseline-2-raspberry-pi", "Baseline", 23.0),
    Device::with_pq("noah-iphone", "Iphone", 65.0, -18.0, 65.0),
    Device::with_pq("noah-macbook", "Macbook Air", 145.0, -15.0, 22.0),
    Device::with_pq("chris-handy", "Smartphone", 60.0, -12.0, 75.0),
    Device::with_pq("peter-laptop", "Laptop", 68.0, -6.0, 25.0),
    Device::with_pq("device-80w", "80W Device", 80.0, -10.0, 122.8),
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

    /// Devices discovered at runtime that should appear in the live catalog.
    ///
    /// Static-catalog methods keep the default (empty). [`AdaptiveNilm`] returns the
    /// learned background load plus every appliance it has fingerprinted so far, with
    /// up-to-date signatures, so the dashboard reflects them as soon as they are seen.
    fn learned_devices(&self) -> Vec<Device> {
        Vec::new()
    }
}

/// Identifier of the synthetic, always-on device that represents the idle background
/// load (for this deployment, the two permanently connected Raspberry Pis).
pub const BACKGROUND_ID: &str = "background";

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
        // A reactive delta needs the value on both sides. A phase block that is absent, and
        // a value the meter reported as unavailable, both leave the dimension unusable.
        let delta_reactive = current.l1.zip(previous.l1).and_then(|(curr_l1, prev_l1)| {
            Some(curr_l1.reactive_power_var? - prev_l1.reactive_power_var?)
        });
        let current_thd = current.l1.and_then(|l| l.thd_current_pct);
        let current_cos_phi = current.l1.and_then(|l| l.cos_phi);

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
        let reactive = reading.l1.and_then(|l| l.reactive_power_var);
        let thd = reading.l1.and_then(|l| l.thd_current_pct);
        let cos_phi = reading.l1.and_then(|l| l.cos_phi);

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
                reactive_power: current.l1.and_then(|l| l.reactive_power_var),
            });
            return DeviceChange::None;
        };

        if (current.total_power - settled.total_power).abs() < self.minimum_change_watts {
            // Follow small background fluctuations without treating them as state changes.
            self.settled_reading = Some(SettledSnapshot {
                total_power: current.total_power,
                reactive_power: current
                    .l1
                    .and_then(|l| l.reactive_power_var)
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

/// A point in the load-signature space: real power, reactive power, and the
/// distortion current implied by THD_I (`I_rms * THD / 100`).
///
/// These three quantities are, unlike a raw THD percentage, approximately additive
/// across parallel loads. That additivity is what lets [`AdaptiveNilm`] subtract a
/// running reference from each settled reading and treat the remainder as one
/// appliance's contribution.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Signature {
    /// Real power, W.
    p: f32,
    /// Reactive power, var (sign preserved: leading is negative on this meter).
    q: f32,
    /// Distortion (harmonic) current, A.
    idist: f32,
}

impl Signature {
    fn from_reading(reading: &MeterReading) -> Self {
        // Each L1 quantity is individually optional; a missing axis degrades to zero
        // so the method still tracks power-only, just with less to disambiguate on.
        let l1 = reading.l1;
        let q = l1.and_then(|l1| l1.reactive_power_var).unwrap_or(0.0);
        let idist = l1
            .and_then(|l1| Some((l1.current_a? * l1.thd_current_pct? / 100.0).abs()))
            .unwrap_or(0.0);
        Self {
            p: reading.total_power,
            q,
            idist,
        }
    }

    /// This signature relative to `base`, component-wise.
    fn delta(self, base: Signature) -> Signature {
        Signature {
            p: self.p - base.p,
            q: self.q - base.q,
            idist: self.idist - base.idist,
        }
    }

    fn negated(self) -> Signature {
        Signature {
            p: -self.p,
            q: -self.q,
            idist: -self.idist,
        }
    }

    /// Running-mean update toward `sample` for the `n`-th sample (`n >= 1`).
    fn accumulate(self, sample: Signature, n: usize) -> Signature {
        let k = n as f32;
        Signature {
            p: self.p + (sample.p - self.p) / k,
            q: self.q + (sample.q - self.q) / k,
            idist: self.idist + (sample.idist - self.idist) / k,
        }
    }

    /// Exponential update toward `sample` with weight `alpha`.
    fn blended(self, sample: Signature, alpha: f32) -> Signature {
        Signature {
            p: self.p + alpha * (sample.p - self.p),
            q: self.q + alpha * (sample.q - self.q),
            idist: self.idist + alpha * (sample.idist - self.idist),
        }
    }

    /// Euclidean distance to `other` with each axis divided by `scale`.
    fn distance(self, other: Signature, scale: Signature) -> f32 {
        let dp = (self.p - other.p) / scale.p;
        let dq = (self.q - other.q) / scale.q;
        let di = (self.idist - other.idist) / scale.idist;
        (dp * dp + dq * dq + di * di).sqrt()
    }
}

#[derive(Clone, Copy, Debug)]
struct SettlingLevel {
    signature: Signature,
    samples: usize,
}

#[derive(Clone, Copy, Debug)]
struct LearnedAppliance {
    id: &'static str,
    name: &'static str,
    /// The turn-on edge: the signature delta observed when this appliance switched on.
    edge: Signature,
}

/// Training-free online NILM that learns appliances at runtime instead of matching
/// against a predefined catalog.
///
/// The first accepted reading is taken as the always-on **background** (here, the two
/// permanently connected Raspberry Pis) and is never reported as an event. After that,
/// every *settled* step change is an **edge** in `(dP, dQ, dI_dist)` space. An edge is
/// matched by normalised nearest-neighbour distance to a previously learned appliance;
/// an unmatched turn-on mints a new one (`Device N`).
///
/// This is Hart's residential load-monitoring approach (P–Q signature clustering),
/// extended with a distortion-current axis and adapted to the gateway's
/// already-settled, roughly one-sample-per-event stream — hence `required_samples`
/// defaults to 1 and the method trusts the client's settling window.
pub struct AdaptiveNilm {
    background: Option<Signature>,
    /// Last settled absolute operating point; edges are measured against this.
    reference: Option<Signature>,
    settling: Option<SettlingLevel>,
    appliances: Vec<LearnedAppliance>,
    /// Monotonic mint counter, for stable naming.
    minted: usize,
    required_samples: usize,
    settle_tolerance_watts: f32,
    min_edge_watts: f32,
    gate_distance: f32,
    max_appliances: usize,
    scale: Signature,
}

impl Default for AdaptiveNilm {
    fn default() -> Self {
        Self::new()
    }
}

impl AdaptiveNilm {
    pub fn new() -> Self {
        Self {
            background: None,
            reference: None,
            settling: None,
            appliances: Vec::new(),
            minted: 0,
            required_samples: 1,
            settle_tolerance_watts: 6.0,
            min_edge_watts: 8.0,
            gate_distance: 1.8,
            max_appliances: 32,
            scale: Signature {
                p: 18.0,
                q: 18.0,
                idist: 0.25,
            },
        }
    }

    /// Tuning hook for tests and future CLI flags.
    #[allow(dead_code)]
    pub fn with_params(
        required_samples: usize,
        settle_tolerance_watts: f32,
        min_edge_watts: f32,
        gate_distance: f32,
        max_appliances: usize,
    ) -> Self {
        Self {
            required_samples: required_samples.max(1),
            settle_tolerance_watts: settle_tolerance_watts.max(0.0),
            min_edge_watts: min_edge_watts.max(0.0),
            gate_distance: gate_distance.max(0.0),
            max_appliances,
            ..Self::new()
        }
    }

    fn background_device(signature: Signature) -> Device {
        Device {
            id: BACKGROUND_ID,
            name: "Background (idle)",
            power_watts: signature.p.max(0.0),
            reactive_power_var: Some(signature.q),
            thd_current_pct: None,
            cos_phi: None,
        }
    }

    fn appliance_device(appliance: &LearnedAppliance) -> Device {
        Device {
            id: appliance.id,
            name: appliance.name,
            power_watts: appliance.edge.p.abs(),
            reactive_power_var: Some(appliance.edge.q),
            thd_current_pct: None,
            cos_phi: None,
        }
    }

    /// Registers a new appliance for `edge` and returns its index.
    ///
    /// Ids and names outlive the method by design: the learned set is bounded by
    /// `max_appliances`, and the server runs for the length of one session.
    fn mint(&mut self, edge: Signature) -> usize {
        self.minted += 1;
        let watts = edge.p.round().max(0.0) as i32;
        let id: &'static str = Box::leak(format!("learned-{}", self.minted).into_boxed_str());
        let name: &'static str =
            Box::leak(format!("Device {} (~{} W)", self.minted, watts).into_boxed_str());
        self.appliances.push(LearnedAppliance { id, name, edge });
        self.appliances.len() - 1
    }

    fn match_turn_on(&mut self, edge: Signature, active: &[Device]) -> DeviceChange {
        let nearest_inactive = self
            .appliances
            .iter()
            .enumerate()
            .filter(|(_, appliance)| !contains_device(active, appliance.id))
            .map(|(index, appliance)| (index, edge.distance(appliance.edge, self.scale)))
            .min_by(|left, right| left.1.total_cmp(&right.1));

        if let Some((index, distance)) = nearest_inactive
            && distance <= self.gate_distance
        {
            return DeviceChange::Added(Self::appliance_device(&self.appliances[index]));
        }

        if self.appliances.len() >= self.max_appliances {
            return DeviceChange::None;
        }
        let index = self.mint(edge);
        DeviceChange::Added(Self::appliance_device(&self.appliances[index]))
    }

    fn match_turn_off(&mut self, edge: Signature, active: &[Device]) -> DeviceChange {
        let target = edge.negated();
        let nearest_active = self
            .appliances
            .iter()
            .enumerate()
            .filter(|(_, appliance)| contains_device(active, appliance.id))
            .map(|(index, appliance)| (index, target.distance(appliance.edge, self.scale)))
            .min_by(|left, right| left.1.total_cmp(&right.1));

        match nearest_active {
            Some((index, distance)) if distance <= self.gate_distance => {
                DeviceChange::Removed(Self::appliance_device(&self.appliances[index]))
            }
            // A negative edge that matches nothing active is left unexplained rather
            // than forcing a removal; the reference still advances to the new level.
            _ => DeviceChange::None,
        }
    }
}

impl DecisionMethod for AdaptiveNilm {
    fn decide_reading(
        &mut self,
        _previous: Option<&MeterReading>,
        current: &MeterReading,
        _catalog: &[Device],
        active_devices: &[Device],
    ) -> DeviceChange {
        let sample = Signature::from_reading(current);

        let Some(reference) = self.reference else {
            // First reading defines the always-on background.
            self.background = Some(sample);
            self.reference = Some(sample);
            return DeviceChange::Added(Self::background_device(sample));
        };

        // Close to the running reference: steady state. Drop any settling candidate and
        // let the background drift slowly while nothing but the background is on.
        if (sample.p - reference.p).abs() < self.min_edge_watts {
            self.settling = None;
            let only_background = active_devices
                .iter()
                .all(|device| device.id == BACKGROUND_ID);
            if only_background {
                self.reference = Some(reference.blended(sample, 0.05));
                if let Some(background) = self.background {
                    self.background = Some(background.blended(sample, 0.05));
                }
            }
            return DeviceChange::None;
        }

        // Accumulate a settled level at the new operating point.
        self.settling = match self.settling {
            Some(level) if (sample.p - level.signature.p).abs() <= self.settle_tolerance_watts => {
                let samples = level.samples + 1;
                Some(SettlingLevel {
                    signature: level.signature.accumulate(sample, samples),
                    samples,
                })
            }
            _ => Some(SettlingLevel {
                signature: sample,
                samples: 1,
            }),
        };

        let level = self.settling.expect("settling level was just set");
        if level.samples < self.required_samples {
            return DeviceChange::None;
        }
        self.settling = None;

        let edge = level.signature.delta(reference);
        self.reference = Some(level.signature);

        if edge.p >= 0.0 {
            self.match_turn_on(edge, active_devices)
        } else {
            self.match_turn_off(edge, active_devices)
        }
    }

    fn learned_devices(&self) -> Vec<Device> {
        let mut devices = Vec::with_capacity(self.appliances.len() + 1);
        if let Some(background) = self.background {
            devices.push(Self::background_device(background));
        }
        devices.extend(self.appliances.iter().map(Self::appliance_device));
        devices
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{PhaseReading, TotalsReading};

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

    /// A reading in the shape the gateway sends: the load sits on L1, and the phases with
    /// nothing connected to them read zero and carry no distortion figure at all.
    fn make_reading(power: f32, reactive: f32, thd: f32) -> MeterReading {
        let apparent = (power * power + reactive * reactive).sqrt();
        let unloaded = PhaseReading {
            voltage_v: Some(0.0),
            current_a: Some(0.0),
            real_power_w: Some(0.0),
            apparent_power_va: Some(0.0),
            reactive_power_var: Some(0.0),
            cos_phi: Some(1.0),
            real_energy_consumed_wh: Some(0.0),
            thd_voltage_pct: None,
            thd_current_pct: None,
        };
        MeterReading {
            total_power: power,
            systime: Some(100),
            frequency_hz: Some(50.0),
            l1: Some(PhaseReading {
                voltage_v: Some(230.0),
                current_a: Some(power / 230.0),
                real_power_w: Some(power),
                apparent_power_va: Some(apparent),
                reactive_power_var: Some(reactive),
                cos_phi: Some(power / apparent.max(0.001)),
                real_energy_consumed_wh: Some(1000.0),
                thd_voltage_pct: Some(1.9),
                thd_current_pct: Some(thd),
            }),
            l2: Some(unloaded),
            l3: Some(unloaded),
            totals: Some(TotalsReading {
                real_power_w: Some(power),
                apparent_power_va: Some(apparent),
                reactive_power_var: Some(reactive),
            }),
            heartbeat: false,
        }
    }

    /// Blanks out the fingerprint dimensions the meter can fail to deliver, the way it does
    /// for a phase it has no current to measure.
    fn without_pq_context(mut reading: MeterReading) -> MeterReading {
        if let Some(l1) = reading.l1.as_mut() {
            l1.reactive_power_var = None;
            l1.thd_current_pct = None;
            l1.cos_phi = None;
        }
        reading
    }

    #[test]
    fn falls_back_to_power_matching_when_the_meter_reports_no_pq_context() {
        // The meter answers with null for quantities its wiring gives it no way to
        // determine. That has to cost the extra fingerprint dimensions and nothing more:
        // a device at a matching wattage must still be detected, not dropped.
        const CHARGER: Device = Device::with_pq("charger", "Charger", 65.0, -18.0, 65.0);
        let catalog = &[CHARGER];

        let mut method = ClosestPowerMatch::new(5.0);
        let base = without_pq_context(make_reading(100.0, 0.0, 2.0));
        let charger_on = without_pq_context(make_reading(165.0, -18.0, 65.0));

        assert_eq!(
            method.decide_reading(Some(&base), &charger_on, catalog, &[]),
            DeviceChange::Added(CHARGER)
        );
    }

    #[test]
    fn a_missing_value_on_one_side_only_still_leaves_power_matching_intact() {
        // The values can also stop being available part way through, which leaves a delta
        // with a known end and an unknown one. That is not a measured change of zero.
        const CHARGER: Device = Device::with_pq("charger", "Charger", 65.0, -18.0, 65.0);
        let catalog = &[CHARGER];

        let mut method = ClosestPowerMatch::new(5.0);
        let base = make_reading(100.0, 0.0, 2.0);
        let charger_on = without_pq_context(make_reading(165.0, -18.0, 65.0));

        assert_eq!(
            method.decide_reading(Some(&base), &charger_on, catalog, &[]),
            DeviceChange::Added(CHARGER)
        );
    }

    #[test]
    fn settled_matching_survives_readings_without_pq_context() {
        // The settling path averages the same dimensions across several readings, so it
        // has to tolerate them being absent for the whole run.
        const CHARGER: Device = Device::with_pq("charger", "Charger", 65.0, -18.0, 65.0);
        let catalog = &[CHARGER];

        let mut method = SettledPowerMatch::new(5.0, 3.0, 2, 5.0);
        method.decide_reading(
            None,
            &without_pq_context(make_reading(100.0, 0.0, 2.0)),
            catalog,
            &[],
        );

        let charger_on = without_pq_context(make_reading(165.0, -18.0, 65.0));
        assert_eq!(
            method.decide_reading(None, &charger_on, catalog, &[]),
            DeviceChange::None,
            "the first reading of a new level only starts the settling"
        );
        assert_eq!(
            method.decide_reading(None, &charger_on, catalog, &[]),
            DeviceChange::Added(CHARGER)
        );
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

    #[test]
    fn detects_80w_device_from_real_world_measurements() {
        let mut method = SettledPowerMatch::new(5.0, 3.0, 1, 8.0);

        // Baseline (only Raspberry Pis): 24.74W, -35.97 var, 124.8% THD
        let baseline = make_reading(24.74, -35.97, 124.8);
        assert_eq!(
            method.decide_reading(None, &baseline, DUMMY_DEVICE_CATALOG, &[]),
            DeviceChange::None
        );

        // 80W Device + 2 Pis turned on: 108.44W, -49.53 var, 122.83% THD
        let reading_80w = make_reading(108.44, -49.53, 122.83);
        let change =
            method.decide_reading(Some(&baseline), &reading_80w, DUMMY_DEVICE_CATALOG, &[]);
        assert_eq!(change, DeviceChange::Added(DUMMY_DEVICE_CATALOG[5]));

        // 80W Device turned off: back to baseline ~25.0W
        let back_to_base = make_reading(25.0, -36.0, 124.5);
        let active = [DUMMY_DEVICE_CATALOG[5]];
        let change_off = method.decide_reading(
            Some(&reading_80w),
            &back_to_base,
            DUMMY_DEVICE_CATALOG,
            &active,
        );
        assert_eq!(change_off, DeviceChange::Removed(DUMMY_DEVICE_CATALOG[5]));
    }

    #[test]
    fn adaptive_learns_background_and_ignores_idle_noise() {
        use crate::meter::MeterState;
        let mut method = AdaptiveNilm::new();
        let mut meter = MeterState::new(Vec::new());

        // Idle: two Raspberry Pis, ~23 W, leading Q, huge THD at the low idle current.
        let change = meter.apply_reading(make_reading(23.0, -34.0, 130.0), &mut method);
        assert!(matches!(change, DeviceChange::Added(device) if device.id == BACKGROUND_ID));

        let snapshot = meter.snapshot();
        assert_eq!(snapshot.active_devices.len(), 1);
        assert_eq!(snapshot.active_devices[0].id, BACKGROUND_ID);

        // Idle fluctuation stays below the edge threshold: no phantom devices.
        for power in [24.0_f32, 22.0, 25.0, 21.5] {
            assert_eq!(
                meter.apply_reading(make_reading(power, -34.0, 128.0), &mut method),
                DeviceChange::None
            );
        }
        assert_eq!(meter.snapshot().active_devices.len(), 1);
        assert_eq!(meter.snapshot().catalog.len(), 1);
    }

    #[test]
    fn adaptive_learns_two_appliances_from_one_reading_per_event() {
        use crate::meter::MeterState;
        // The gateway sends roughly one already-settled reading per state change, so
        // the method must decide on a single sample per edge.
        let mut method = AdaptiveNilm::new();
        let mut meter = MeterState::new(Vec::new());

        meter.apply_reading(make_reading(23.0, -34.0, 8.0), &mut method);

        // Appliance A on: +45 W, dQ -5 var.
        let a_id = match meter.apply_reading(make_reading(68.0, -39.0, 8.0), &mut method) {
            DeviceChange::Added(device) => device.id,
            other => panic!("expected appliance A added, got {other:?}"),
        };

        // Appliance B on: +120 W, dQ -30 var.
        let b_id = match meter.apply_reading(make_reading(188.0, -69.0, 8.0), &mut method) {
            DeviceChange::Added(device) => device.id,
            other => panic!("expected appliance B added, got {other:?}"),
        };
        assert_ne!(a_id, b_id);
        assert_eq!(meter.snapshot().active_devices.len(), 3); // background + A + B

        // Appliance A off: -45 W. The negated edge must resolve to A, not B.
        assert!(matches!(
            meter.apply_reading(make_reading(143.0, -64.0, 8.0), &mut method),
            DeviceChange::Removed(device) if device.id == a_id
        ));

        // Appliance A on again: same edge -> re-matched, no new device minted.
        assert!(matches!(
            meter.apply_reading(make_reading(188.0, -69.0, 8.0), &mut method),
            DeviceChange::Added(device) if device.id == a_id
        ));

        let catalog = meter.snapshot().catalog;
        let learned = catalog.iter().filter(|d| d.id != BACKGROUND_ID).count();
        assert_eq!(learned, 2, "only two appliances were ever seen");
    }

    #[test]
    fn adaptive_caps_the_number_of_learned_appliances() {
        use crate::meter::MeterState;
        let mut method = AdaptiveNilm::with_params(1, 6.0, 8.0, 1.5, 2);
        let mut meter = MeterState::new(Vec::new());
        meter.apply_reading(make_reading(23.0, -34.0, 8.0), &mut method);

        meter.apply_reading(make_reading(70.0, -34.0, 8.0), &mut method); // +47 -> learn #1
        meter.apply_reading(make_reading(190.0, -34.0, 8.0), &mut method); // +120 -> learn #2
        let before = meter.snapshot().catalog.len();

        // A third distinct turn-on has no free slot.
        assert_eq!(
            meter.apply_reading(make_reading(400.0, -34.0, 8.0), &mut method),
            DeviceChange::None
        );
        assert_eq!(meter.snapshot().catalog.len(), before);
    }
}
