//! Interchangeable decoding and validation of gateway request bodies.

use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize};

/// Converts an incoming request into validated readings in transmission order.
/// Implementations must reject the entire request if any reading is invalid.
pub trait ReadingDecoder: Send + Sync {
    fn decode_readings(&self, body: &[u8]) -> Result<Vec<MeterReading>, String>;
}

pub type SharedReadingDecoder = Arc<dyn ReadingDecoder>;

/// Decodes the gateway's measurement objects and arrays, plus legacy power payloads.
pub struct JsonReadingDecoder;

/// The gateway schema. Context is optional for legacy power-only senders, but must
/// have the correct type when present. Unknown fields are ignored for extensibility.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct MeterReading {
    /// The three-phase real power in watts, signed: a site that exports more than it
    /// draws reports a negative total.
    #[serde(alias = "power", alias = "power_watts", alias = "power_l1_n")]
    pub total_power: f32,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub systime: Option<i32>,
    #[serde(
        default,
        deserialize_with = "measured",
        skip_serializing_if = "Option::is_none"
    )]
    pub frequency_hz: Option<f32>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub l1: Option<PhaseReading>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub l2: Option<PhaseReading>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub l3: Option<PhaseReading>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub totals: Option<TotalsReading>,
}

/// One phase of the client's three-phase block.
///
/// All fields are required together, but any of them may be `null`: the meter reports
/// quantities it cannot determine — the harmonic distortion of a phase with no current,
/// for example — as unavailable rather than as a number. Null is kept as unavailable
/// instead of being flattened to zero, which would claim a measurement that never happened.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct PhaseReading {
    #[serde(deserialize_with = "measured")]
    pub voltage_v: Option<f32>,
    #[serde(deserialize_with = "measured")]
    pub current_a: Option<f32>,
    #[serde(deserialize_with = "measured")]
    pub real_power_w: Option<f32>,
    #[serde(deserialize_with = "measured")]
    pub apparent_power_va: Option<f32>,
    #[serde(deserialize_with = "measured")]
    pub reactive_power_var: Option<f32>,
    #[serde(deserialize_with = "measured")]
    pub cos_phi: Option<f32>,
    #[serde(deserialize_with = "measured")]
    pub real_energy_consumed_wh: Option<f32>,
    #[serde(deserialize_with = "measured")]
    pub thd_voltage_pct: Option<f32>,
    #[serde(deserialize_with = "measured")]
    pub thd_current_pct: Option<f32>,
}

/// The three-phase sums as the meter measures them, rather than sums derived from the
/// phases. Required together, and individually nullable for the same reason as a phase.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct TotalsReading {
    #[serde(deserialize_with = "measured")]
    pub real_power_w: Option<f32>,
    #[serde(deserialize_with = "measured")]
    pub apparent_power_va: Option<f32>,
    #[serde(deserialize_with = "measured")]
    pub reactive_power_var: Option<f32>,
}

impl PhaseReading {
    /// The measured values, for validating and reporting them by name.
    fn fields(&self) -> [(&'static str, Option<f32>); 9] {
        [
            ("voltage_v", self.voltage_v),
            ("current_a", self.current_a),
            ("real_power_w", self.real_power_w),
            ("apparent_power_va", self.apparent_power_va),
            ("reactive_power_var", self.reactive_power_var),
            ("cos_phi", self.cos_phi),
            ("real_energy_consumed_wh", self.real_energy_consumed_wh),
            ("thd_voltage_pct", self.thd_voltage_pct),
            ("thd_current_pct", self.thd_current_pct),
        ]
    }
}

impl TotalsReading {
    fn fields(&self) -> [(&'static str, Option<f32>); 3] {
        [
            ("real_power_w", self.real_power_w),
            ("apparent_power_va", self.apparent_power_va),
            ("reactive_power_var", self.reactive_power_var),
        ]
    }
}

// Missing context is allowed, but explicit nulls must not silently become missing data.
// This is for the structural fields, which are either sent whole or not at all.
fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

// A single measured value, where `null` means the meter reported the quantity as
// unavailable. Using `deserialize_with` without `default` keeps a field of a phase block
// required while still accepting null as its value; the fields that carry `default` as
// well stay optional for legacy senders.
fn measured<'de, D>(deserializer: D) -> Result<Option<f32>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<f32>::deserialize(deserializer)
}

impl From<f32> for MeterReading {
    fn from(total_power: f32) -> Self {
        Self {
            total_power,
            systime: None,
            frequency_hz: None,
            l1: None,
            l2: None,
            l3: None,
            totals: None,
        }
    }
}

impl MeterReading {
    fn validate(&self) -> Result<(), String> {
        // Signed: exported power is a normal reading in a decentralized grid. Only a
        // value that is not a number at all makes the reading unusable.
        if !self.total_power.is_finite() {
            return Err("total power must be a finite number".to_owned());
        }
        if self.frequency_hz.is_some_and(|value| !value.is_finite()) {
            return Err("frequency_hz must be a finite number or null".to_owned());
        }
        let phases = [("l1", self.l1), ("l2", self.l2), ("l3", self.l3)];
        for (phase, reading) in phases {
            let Some(reading) = reading else { continue };
            for (name, value) in reading.fields() {
                if value.is_some_and(|value| !value.is_finite()) {
                    return Err(format!("{phase}.{name} must be a finite number or null"));
                }
            }
        }
        if let Some(totals) = self.totals {
            for (name, value) in totals.fields() {
                if value.is_some_and(|value| !value.is_finite()) {
                    return Err(format!("totals.{name} must be a finite number or null"));
                }
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyMessage {
    message: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum IncomingReading {
    // Boxed to keep the variants a similar size: a reading carries three phase blocks,
    // a legacy message carries one string. It is unwrapped as soon as it is decoded.
    Reading(Box<MeterReading>),
    ClientMessage(LegacyMessage),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum IncomingReadings {
    Single(IncomingReading),
    Batch(Vec<IncomingReading>),
}

impl ReadingDecoder for JsonReadingDecoder {
    fn decode_readings(&self, body: &[u8]) -> Result<Vec<MeterReading>, String> {
        let incoming: IncomingReadings = serde_json::from_slice(body)
            .map_err(|error| format!("invalid meter-reading JSON: {error}"))?;
        let readings = match incoming {
            IncomingReadings::Single(reading) => vec![reading],
            IncomingReadings::Batch(readings) => readings,
        };
        if readings.is_empty() {
            return Err("measurement batch must not be empty".to_owned());
        }
        readings
            .into_iter()
            .enumerate()
            .map(|(index, incoming)| {
                let decode = || {
                    let reading = match incoming {
                        IncomingReading::Reading(reading) => *reading,
                        IncomingReading::ClientMessage(LegacyMessage { message }) => {
                            MeterReading::from(message.parse::<f32>().map_err(|_| {
                                "message must contain a power value in watts".to_owned()
                            })?)
                        }
                    };
                    reading.validate()?;
                    Ok(reading)
                };
                decode().map_err(|error: String| format!("reading {}: {error}", index + 1))
            })
            .collect()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// One phase as the client sends it. `thd` is `None` for a phase the meter cannot
    /// measure the distortion of, which is what an unconnected phase looks like.
    fn phase(voltage: f32, current: f32, power: f32, thd: Option<f32>) -> serde_json::Value {
        serde_json::json!({
            "voltage_v": voltage,
            "current_a": current,
            "real_power_w": power,
            "apparent_power_va": power * 1.15,
            "reactive_power_var": -10.0,
            "cos_phi": 0.9_f32,
            "real_energy_consumed_wh": 1234.0,
            "thd_voltage_pct": thd,
            "thd_current_pct": thd,
        })
    }

    // Mirrors every field emitted by pq-meter-client's monitor(): the meter in the lab is
    // wired up on L1, so the other phases carry zeros and no distortion reading.
    pub fn client_reading(power: f32, systime: i32) -> serde_json::Value {
        serde_json::json!({
            "total_power": power,
            "systime": systime,
            "frequency_hz": 50.0,
            "l1": phase(230.0, 0.5, power, Some(2.0)),
            "l2": phase(0.0, 0.0, 0.0, None),
            "l3": phase(0.0, 0.0, 0.0, None),
            "totals": {
                "real_power_w": power,
                "apparent_power_va": power * 1.15,
                "reactive_power_var": -10.0
            }
        })
    }

    const PHASE_FIELDS: [&str; 9] = [
        "voltage_v",
        "current_a",
        "real_power_w",
        "apparent_power_va",
        "reactive_power_var",
        "cos_phi",
        "real_energy_consumed_wh",
        "thd_voltage_pct",
        "thd_current_pct",
    ];

    #[test]
    fn accepts_single_readings_aliases_and_legacy_messages() {
        for body in [
            r#"{"total_power":860.0}"#,
            r#"{"power":860.0}"#,
            r#"{"power_watts":860.0}"#,
            r#"{"power_l1_n":860.0}"#,
            r#"{"message":"860"}"#,
        ] {
            assert_eq!(
                JsonReadingDecoder.decode_readings(body.as_bytes()),
                Ok(vec![860.0.into()])
            );
        }
    }

    #[test]
    fn preserves_every_client_field_and_batch_order() {
        let first = client_reading(100.0, 123);
        let second = client_reading(123.0, 124);
        for value in [first.clone(), serde_json::json!([first, second])] {
            let decoded = JsonReadingDecoder
                .decode_readings(&serde_json::to_vec(&value).unwrap())
                .unwrap();
            let expected = if value.is_array() {
                value
            } else {
                serde_json::json!([value])
            };
            assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
        }
    }

    #[test]
    fn keeps_all_three_phases_and_the_measured_sums() {
        let decoded = JsonReadingDecoder
            .decode_readings(&serde_json::to_vec(&client_reading(100.0, 123)).unwrap())
            .unwrap();
        let [reading] = decoded[..] else {
            panic!("expected exactly one reading");
        };

        assert_eq!(reading.total_power, 100.0);
        assert_eq!(reading.l1.unwrap().voltage_v, Some(230.0));
        assert_eq!(reading.l2.unwrap().voltage_v, Some(0.0));
        assert_eq!(reading.l3.unwrap().voltage_v, Some(0.0));
        assert_eq!(reading.totals.unwrap().real_power_w, Some(100.0));
        assert_eq!(reading.totals.unwrap().reactive_power_var, Some(-10.0));
    }

    #[test]
    fn accepts_exported_power_as_a_negative_total() {
        // A site that feeds into the grid is a normal reading, not a broken one.
        let decoded = JsonReadingDecoder
            .decode_readings(br#"{"total_power":-1200.5}"#)
            .unwrap();
        assert_eq!(decoded, vec![(-1200.5).into()]);

        let reading = client_reading(-1200.5, 10);
        assert!(
            JsonReadingDecoder
                .decode_readings(&serde_json::to_vec(&reading).unwrap())
                .is_ok()
        );
    }

    #[test]
    fn keeps_unavailable_phase_values_apart_from_zero_and_from_missing_blocks() {
        let decoded = JsonReadingDecoder
            .decode_readings(&serde_json::to_vec(&client_reading(100.0, 123)).unwrap())
            .unwrap();
        let l3 = decoded[0].l3.expect("the L3 block is present");

        // Null is retained as "not measured" rather than read as a distortion of zero.
        assert_eq!(l3.thd_current_pct, None);
        assert_eq!(l3.current_a, Some(0.0));

        // And it round-trips as null, so a reader downstream sees the same distinction.
        let value = serde_json::to_value(decoded[0]).unwrap();
        assert_eq!(value["l3"]["thd_current_pct"], serde_json::Value::Null);
        assert_eq!(value["l3"]["current_a"], serde_json::json!(0.0));
    }

    #[test]
    fn rejects_invalid_shapes_power_and_empty_batches() {
        for body in [
            "[]",
            "{}",
            "null",
            "42",
            "[[]]",
            "[null]",
            "[{}]",
            "{",
            r#"{"readings":[{"total_power":100}]}"#,
            r#"{"total_power":1e100}"#,
            r#"{"total_power":-1e100}"#,
            r#"{"total_power":null}"#,
            r#"{"total_power":"100"}"#,
            r#"{"total_power":1,"power":2}"#,
            r#"{"message":"unknown"}"#,
            r#"{"message":"NaN"}"#,
            r#"{"message":"inf"}"#,
            r#"{"total_power":null,"message":"100"}"#,
            r#"[{"total_power":100},{"total_power":1e100}]"#,
        ] {
            assert!(
                JsonReadingDecoder.decode_readings(body.as_bytes()).is_err(),
                "{body}"
            );
        }
    }

    #[test]
    fn validates_context_instead_of_ignoring_it() {
        for field in ["systime", "l1", "l2", "l3", "totals"] {
            for invalid in [serde_json::json!(null), serde_json::json!("invalid")] {
                let mut reading = client_reading(100.0, 123);
                reading[field] = invalid;
                assert!(
                    JsonReadingDecoder
                        .decode_readings(&serde_json::to_vec(&reading).unwrap())
                        .is_err(),
                    "{reading}"
                );
            }
        }
        for phase in ["l1", "l2", "l3"] {
            for field in PHASE_FIELDS {
                for invalid in [serde_json::json!("invalid"), serde_json::json!(1e100)] {
                    let mut reading = client_reading(100.0, 123);
                    reading[phase][field] = invalid;
                    assert!(
                        JsonReadingDecoder
                            .decode_readings(&serde_json::to_vec(&reading).unwrap())
                            .is_err(),
                        "{reading}"
                    );
                }
                // A field of a phase block has to be sent, even when its value is null.
                let mut reading = client_reading(100.0, 123);
                reading[phase].as_object_mut().unwrap().remove(field);
                assert!(
                    JsonReadingDecoder
                        .decode_readings(&serde_json::to_vec(&reading).unwrap())
                        .is_err(),
                    "{phase} without {field} must be rejected"
                );
            }
        }
        for field in ["real_power_w", "apparent_power_va", "reactive_power_var"] {
            let mut reading = client_reading(100.0, 123);
            reading["totals"][field] = serde_json::json!(1e100);
            assert!(
                JsonReadingDecoder
                    .decode_readings(&serde_json::to_vec(&reading).unwrap())
                    .is_err()
            );

            let mut reading = client_reading(100.0, 123);
            reading["totals"].as_object_mut().unwrap().remove(field);
            assert!(
                JsonReadingDecoder
                    .decode_readings(&serde_json::to_vec(&reading).unwrap())
                    .is_err()
            );
        }
        for (field, invalid) in [
            ("systime", serde_json::json!(1.5)),
            ("systime", serde_json::json!(2147483648_i64)),
            ("frequency_hz", serde_json::json!(1e100)),
            ("frequency_hz", serde_json::json!("invalid")),
        ] {
            let mut reading = client_reading(100.0, 123);
            reading[field] = invalid;
            assert!(
                JsonReadingDecoder
                    .decode_readings(&serde_json::to_vec(&reading).unwrap())
                    .is_err()
            );
        }
    }

    #[test]
    fn accepts_null_for_a_value_the_meter_could_not_determine() {
        // Every measured scalar may be reported as unavailable; the structural blocks
        // themselves may not, since a missing block is a different statement.
        for phase in ["l1", "l2", "l3"] {
            for field in PHASE_FIELDS {
                let mut reading = client_reading(100.0, 123);
                reading[phase][field] = serde_json::json!(null);
                assert!(
                    JsonReadingDecoder
                        .decode_readings(&serde_json::to_vec(&reading).unwrap())
                        .is_ok(),
                    "{phase}.{field} must be allowed to be null"
                );
            }
        }
        let mut reading = client_reading(100.0, 123);
        reading["frequency_hz"] = serde_json::json!(null);
        reading["totals"]["reactive_power_var"] = serde_json::json!(null);
        assert!(
            JsonReadingDecoder
                .decode_readings(&serde_json::to_vec(&reading).unwrap())
                .is_ok()
        );
    }

    #[test]
    fn reports_which_value_of_which_phase_was_rejected() {
        let mut reading = client_reading(100.0, 123);
        reading["l2"]["cos_phi"] = serde_json::json!(1e100);
        let error = JsonReadingDecoder
            .decode_readings(&serde_json::to_vec(&serde_json::json!([reading])).unwrap())
            .unwrap_err();

        assert!(error.contains("reading 1"), "{error}");
        assert!(error.contains("l2.cos_phi"), "{error}");
    }
}
