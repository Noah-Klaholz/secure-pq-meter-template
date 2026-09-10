//! Power-quality limits and the violations a reading breaches.
//!
//! The limits follow EN 50160, the European standard for the voltage characteristics of
//! public distribution networks. Keeping the rule here rather than in the dashboard's
//! JavaScript means one definition, checked by tests, that the UI only has to colour in.

use serde::Serialize;

use crate::input::{MeterReading, PhaseReading};

/// Nominal phase-to-neutral voltage of the low-voltage network.
pub const NOMINAL_VOLTAGE_V: f32 = 230.0;
/// Nominal grid frequency.
pub const NOMINAL_FREQUENCY_HZ: f32 = 50.0;

/// EN 50160 allows the supply voltage to deviate by 10% from nominal.
pub const VOLTAGE_TOLERANCE_RATIO: f32 = 0.10;
/// The band the voltage of a connected phase has to stay inside.
pub const VOLTAGE_MIN_V: f32 = NOMINAL_VOLTAGE_V * (1.0 - VOLTAGE_TOLERANCE_RATIO);
pub const VOLTAGE_MAX_V: f32 = NOMINAL_VOLTAGE_V * (1.0 + VOLTAGE_TOLERANCE_RATIO);

/// EN 50160 allows 1% frequency deviation for interconnected systems.
pub const FREQUENCY_MIN_HZ: f32 = 49.5;
pub const FREQUENCY_MAX_HZ: f32 = 50.5;

/// EN 50160 caps the total harmonic distortion of the supply voltage at 8%.
pub const THD_VOLTAGE_MAX_PCT: f32 = 8.0;

/// Below this current a phase counts as unloaded and its voltage is not judged.
///
/// An unconnected phase reads 0 V, which is not an undervoltage event — it is an absence
/// of supply on a terminal nothing is wired to.
pub const CONNECTED_PHASE_CURRENT_A: f32 = 0.01;

/// Current distortion is only meaningful once a real current flows.
///
/// The ratio is taken against a fundamental that approaches zero when a phase is idle, so
/// a bench meter reports THD_I of well over 100% for a few tens of milliamps. Reporting
/// that as a fault would cry wolf on every idle installation, so current distortion is
/// carried as context and only judged above this current.
pub const THD_CURRENT_RELEVANT_A: f32 = 1.0;
/// The distortion above which a meaningfully loaded phase is called out. IEEE 519 sets
/// current-distortion limits by supply strength; this is a demonstration threshold.
pub const THD_CURRENT_MAX_PCT: f32 = 8.0;

/// How severely a reading departs from the limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Outside the limit: a power-quality event.
    Violation,
    /// Inside the limit but close enough to be worth watching.
    Warning,
}

/// One quantity that left its limit, named so the dashboard can point at it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Violation {
    /// The measured quantity, e.g. `voltage_v`.
    pub quantity: &'static str,
    /// `L1`, `L2`, `L3`, or `-` for a quantity that is not per phase.
    pub phase: &'static str,
    pub value: f32,
    pub limit: f32,
    pub severity: Severity,
    /// A sentence naming what happened, ready to display.
    pub message: String,
}

/// The limits, published so the dashboard draws the same bands it is judged against.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Limits {
    pub nominal_voltage_v: f32,
    pub voltage_min_v: f32,
    pub voltage_max_v: f32,
    pub nominal_frequency_hz: f32,
    pub frequency_min_hz: f32,
    pub frequency_max_hz: f32,
    pub thd_voltage_max_pct: f32,
    pub thd_current_max_pct: f32,
    pub thd_current_relevant_a: f32,
}

impl Limits {
    pub const fn en50160() -> Self {
        Self {
            nominal_voltage_v: NOMINAL_VOLTAGE_V,
            voltage_min_v: VOLTAGE_MIN_V,
            voltage_max_v: VOLTAGE_MAX_V,
            nominal_frequency_hz: NOMINAL_FREQUENCY_HZ,
            frequency_min_hz: FREQUENCY_MIN_HZ,
            frequency_max_hz: FREQUENCY_MAX_HZ,
            thd_voltage_max_pct: THD_VOLTAGE_MAX_PCT,
            thd_current_max_pct: THD_CURRENT_MAX_PCT,
            thd_current_relevant_a: THD_CURRENT_RELEVANT_A,
        }
    }
}

const PHASE_NAMES: [&str; 3] = ["L1", "L2", "L3"];

/// Whether a phase carries enough current to have a supply worth judging.
fn is_connected(phase: &PhaseReading) -> bool {
    phase
        .current_a
        .is_some_and(|current| current.abs() >= CONNECTED_PHASE_CURRENT_A)
}

/// Every limit the reading breaches, most severe quantity first by measurement order.
///
/// A value the meter reported as unavailable is never a violation: an unknown value is not
/// a measured excursion, and treating it as one would flag every unwired phase.
pub fn violations(reading: &MeterReading) -> Vec<Violation> {
    let mut found = Vec::new();

    if let Some(frequency) = reading.frequency_hz.filter(|value| value.is_finite()) {
        let limit = if frequency < FREQUENCY_MIN_HZ {
            Some(FREQUENCY_MIN_HZ)
        } else if frequency > FREQUENCY_MAX_HZ {
            Some(FREQUENCY_MAX_HZ)
        } else {
            None
        };
        if let Some(limit) = limit {
            found.push(Violation {
                quantity: "frequency_hz",
                phase: "-",
                value: frequency,
                limit,
                severity: Severity::Violation,
                message: format!(
                    "Grid frequency {frequency:.2} Hz is outside {FREQUENCY_MIN_HZ:.1}–{FREQUENCY_MAX_HZ:.1} Hz"
                ),
            });
        }
    }

    for (index, phase) in [reading.l1, reading.l2, reading.l3].into_iter().enumerate() {
        let Some(phase) = phase else { continue };
        let name = PHASE_NAMES[index];

        // A phase with nothing drawing on it has no supply to judge.
        if is_connected(&phase)
            && let Some(voltage) = phase.voltage_v.filter(|value| value.is_finite())
        {
            let limit = if voltage < VOLTAGE_MIN_V {
                Some(VOLTAGE_MIN_V)
            } else if voltage > VOLTAGE_MAX_V {
                Some(VOLTAGE_MAX_V)
            } else {
                None
            };
            if let Some(limit) = limit {
                found.push(Violation {
                    quantity: "voltage_v",
                    phase: name,
                    value: voltage,
                    limit,
                    severity: Severity::Violation,
                    message: format!(
                        "{name} voltage {voltage:.1} V is outside {VOLTAGE_MIN_V:.0}–{VOLTAGE_MAX_V:.0} V"
                    ),
                });
            }
        }

        if let Some(thd) = phase.thd_voltage_pct.filter(|value| value.is_finite())
            && thd > THD_VOLTAGE_MAX_PCT
        {
            found.push(Violation {
                quantity: "thd_voltage_pct",
                phase: name,
                value: thd,
                limit: THD_VOLTAGE_MAX_PCT,
                severity: Severity::Violation,
                message: format!(
                    "{name} voltage distortion {thd:.1} % exceeds {THD_VOLTAGE_MAX_PCT:.0} %"
                ),
            });
        }

        // Only a phase drawing real current has a meaningful current distortion.
        let carries_load = phase
            .current_a
            .is_some_and(|current| current.abs() >= THD_CURRENT_RELEVANT_A);
        if carries_load
            && let Some(thd) = phase.thd_current_pct.filter(|value| value.is_finite())
            && thd > THD_CURRENT_MAX_PCT
        {
            found.push(Violation {
                quantity: "thd_current_pct",
                phase: name,
                value: thd,
                limit: THD_CURRENT_MAX_PCT,
                severity: Severity::Warning,
                message: format!(
                    "{name} current distortion {thd:.1} % exceeds {THD_CURRENT_MAX_PCT:.0} %"
                ),
            });
        }
    }

    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{PhaseReading, TotalsReading};

    fn phase(voltage: f32, current: f32, thd_u: Option<f32>, thd_i: Option<f32>) -> PhaseReading {
        PhaseReading {
            voltage_v: Some(voltage),
            current_a: Some(current),
            real_power_w: Some(voltage * current),
            apparent_power_va: Some(voltage * current),
            reactive_power_var: Some(0.0),
            cos_phi: Some(1.0),
            real_energy_consumed_wh: Some(1000.0),
            thd_voltage_pct: thd_u,
            thd_current_pct: thd_i,
        }
    }

    /// The meter in the lab: L1 loaded, the other phases unwired, so they read zero and
    /// carry no distortion figure at all.
    fn lab_reading(frequency: f32, l1: PhaseReading) -> MeterReading {
        let unwired = phase(0.0, 0.0, None, None);
        MeterReading {
            total_power: l1.real_power_w.unwrap_or(0.0),
            systime: Some(1_789_000_000),
            frequency_hz: Some(frequency),
            l1: Some(l1),
            l2: Some(unwired),
            l3: Some(unwired),
            totals: Some(TotalsReading {
                real_power_w: l1.real_power_w,
                apparent_power_va: l1.apparent_power_va,
                reactive_power_var: Some(0.0),
            }),
            heartbeat: false,
        }
    }

    #[test]
    fn a_healthy_reading_has_no_violations() {
        let reading = lab_reading(50.0, phase(230.0, 2.0, Some(1.9), Some(3.0)));
        assert_eq!(violations(&reading), Vec::new());
    }

    #[test]
    fn flags_voltage_outside_the_en50160_band() {
        for (voltage, limit) in [(200.0, VOLTAGE_MIN_V), (260.0, VOLTAGE_MAX_V)] {
            let reading = lab_reading(50.0, phase(voltage, 2.0, Some(1.9), None));
            let found = violations(&reading);
            assert_eq!(found.len(), 1, "{voltage} V");
            assert_eq!(found[0].quantity, "voltage_v");
            assert_eq!(found[0].phase, "L1");
            assert_eq!(found[0].limit, limit);
            assert_eq!(found[0].severity, Severity::Violation);
        }

        // The edges of the band are still compliant.
        for voltage in [VOLTAGE_MIN_V, VOLTAGE_MAX_V] {
            let reading = lab_reading(50.0, phase(voltage, 2.0, Some(1.9), None));
            assert!(violations(&reading).is_empty(), "{voltage} V");
        }
    }

    #[test]
    fn flags_frequency_outside_the_band() {
        for (frequency, limit) in [(49.0, FREQUENCY_MIN_HZ), (51.0, FREQUENCY_MAX_HZ)] {
            let reading = lab_reading(frequency, phase(230.0, 2.0, Some(1.9), None));
            let found = violations(&reading);
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].quantity, "frequency_hz");
            assert_eq!(found[0].phase, "-");
            assert_eq!(found[0].limit, limit);
        }
        for frequency in [FREQUENCY_MIN_HZ, 50.0, FREQUENCY_MAX_HZ] {
            assert!(
                violations(&lab_reading(frequency, phase(230.0, 2.0, Some(1.9), None))).is_empty()
            );
        }
    }

    #[test]
    fn flags_voltage_distortion_above_eight_percent() {
        let reading = lab_reading(50.0, phase(230.0, 2.0, Some(9.5), None));
        let found = violations(&reading);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].quantity, "thd_voltage_pct");
        assert_eq!(found[0].severity, Severity::Violation);
    }

    #[test]
    fn an_unwired_phase_reading_zero_volts_is_not_an_undervoltage() {
        // Both other phases sit at 0 V in the lab. Judging them would bury the real
        // measurement on L1 under two permanent, meaningless faults.
        let reading = lab_reading(50.0, phase(230.0, 2.0, Some(1.9), Some(2.0)));
        assert!(violations(&reading).is_empty());
    }

    #[test]
    fn current_distortion_is_only_judged_once_a_real_current_flows() {
        // The bench meter reports ~140 % THD_I for the 0.3 A it idles at, because the
        // fundamental it divides by is nearly zero. That is not a power-quality event.
        let idle = lab_reading(50.0, phase(230.0, 0.3, Some(1.9), Some(140.0)));
        assert!(violations(&idle).is_empty());

        // Under a real load the same distortion is worth reporting, as a warning rather
        // than a violation: EN 50160 does not set a current-distortion limit.
        let loaded = lab_reading(50.0, phase(230.0, 6.0, Some(1.9), Some(140.0)));
        let found = violations(&loaded);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].quantity, "thd_current_pct");
        assert_eq!(found[0].severity, Severity::Warning);
    }

    #[test]
    fn values_the_meter_could_not_determine_are_never_violations() {
        let mut reading = lab_reading(50.0, phase(230.0, 2.0, None, None));
        reading.frequency_hz = None;
        reading.l1 = Some(PhaseReading {
            voltage_v: None,
            ..reading.l1.unwrap()
        });
        assert!(violations(&reading).is_empty());
    }

    #[test]
    fn reports_every_breach_of_a_reading_not_only_the_first() {
        let reading = lab_reading(48.0, phase(190.0, 6.0, Some(12.0), Some(30.0)));
        let found = violations(&reading);
        let quantities: Vec<_> = found.iter().map(|v| v.quantity).collect();
        assert_eq!(
            quantities,
            vec![
                "frequency_hz",
                "voltage_v",
                "thd_voltage_pct",
                "thd_current_pct"
            ]
        );
    }
}
