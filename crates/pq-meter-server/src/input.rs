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
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub frequency_hz: Option<f32>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub l1: Option<L1Reading>,
}

/// All fields emitted in the client's L1 measurement block are required together.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct L1Reading {
    pub voltage_v: f32,
    pub current_a: f32,
    pub real_power_w: f32,
    pub apparent_power_va: f32,
    pub reactive_power_var: f32,
    pub cos_phi: f32,
    pub real_energy_consumed_wh: f32,
    pub thd_current_pct: f32,
}

// Missing context is allowed, but explicit nulls (including non-finite floats
// serialized by serde_json on the client) must not silently become missing data.
fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

impl From<f32> for MeterReading {
    fn from(total_power: f32) -> Self {
        Self {
            total_power,
            systime: None,
            frequency_hz: None,
            l1: None,
        }
    }
}

impl MeterReading {
    fn validate(&self) -> Result<(), String> {
        if !self.total_power.is_finite() || self.total_power < 0.0 {
            return Err("total power must be a finite, non-negative number".to_owned());
        }
        if self.frequency_hz.is_some_and(|value| !value.is_finite()) {
            return Err("frequency_hz must be finite".to_owned());
        }
        if let Some(l1) = self.l1 {
            for (name, value) in [
                ("voltage_v", l1.voltage_v),
                ("current_a", l1.current_a),
                ("real_power_w", l1.real_power_w),
                ("apparent_power_va", l1.apparent_power_va),
                ("reactive_power_var", l1.reactive_power_var),
                ("cos_phi", l1.cos_phi),
                ("real_energy_consumed_wh", l1.real_energy_consumed_wh),
                ("thd_current_pct", l1.thd_current_pct),
            ] {
                if !value.is_finite() {
                    return Err(format!("l1.{name} must be finite"));
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
    Reading(MeterReading),
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
                        IncomingReading::Reading(reading) => reading,
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

    // Mirrors every field emitted by pq-meter-client's monitor().
    pub fn client_reading(power: f32, systime: i32) -> serde_json::Value {
        serde_json::json!({
            "total_power": power,
            "systime": systime,
            "frequency_hz": 50.0,
            "l1": {
                "voltage_v": 230.0,
                "current_a": 0.5,
                "real_power_w": power,
                "apparent_power_va": 115.0,
                "reactive_power_var": -10.0,
                "cos_phi": 0.9_f32,
                "real_energy_consumed_wh": 1234.0,
                "thd_current_pct": 2.0
            }
        })
    }

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
            r#"{"total_power":-1}"#,
            r#"{"total_power":1e100}"#,
            r#"{"total_power":null}"#,
            r#"{"total_power":"100"}"#,
            r#"{"total_power":1,"power":2}"#,
            r#"{"message":"unknown"}"#,
            r#"{"message":"NaN"}"#,
            r#"{"message":"inf"}"#,
            r#"{"total_power":null,"message":"100"}"#,
            r#"[{"total_power":100},{"total_power":-1}]"#,
        ] {
            assert!(
                JsonReadingDecoder.decode_readings(body.as_bytes()).is_err(),
                "{body}"
            );
        }
    }

    #[test]
    fn validates_context_instead_of_ignoring_it() {
        for field in ["systime", "frequency_hz", "l1"] {
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
        for field in [
            "voltage_v",
            "current_a",
            "real_power_w",
            "apparent_power_va",
            "reactive_power_var",
            "cos_phi",
            "real_energy_consumed_wh",
            "thd_current_pct",
        ] {
            for invalid in [
                serde_json::json!(null),
                serde_json::json!("invalid"),
                serde_json::json!(1e100),
            ] {
                let mut reading = client_reading(100.0, 123);
                reading["l1"][field] = invalid;
                assert!(
                    JsonReadingDecoder
                        .decode_readings(&serde_json::to_vec(&reading).unwrap())
                        .is_err(),
                    "{reading}"
                );
            }
            let mut reading = client_reading(100.0, 123);
            reading["l1"].as_object_mut().unwrap().remove(field);
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
}
