//! Interchangeable decoding of gateway request bodies.

use std::sync::Arc;

use serde::Deserialize;

/// Converts an incoming request body into total active power in watts.
///
/// Implement this trait for the final gateway schema and select it in `main.rs`. The HTTP
/// handler and device-decision code do not need to change when the wire format changes.
pub trait ReadingDecoder: Send + Sync {
    fn decode_total_power(&self, body: &[u8]) -> Result<f32, String>;
}

pub type SharedReadingDecoder = Arc<dyn ReadingDecoder>;

/// Temporary JSON decoder supporting both a direct reading and the repository's CLI client.
pub struct DummyJsonDecoder;

#[derive(Debug, Deserialize)]
struct PowerReading {
    #[serde(alias = "power", alias = "power_watts", alias = "power_l1_n")]
    total_power: f32,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum IncomingReading {
    Reading(PowerReading),
    ClientMessage { message: String },
}

impl ReadingDecoder for DummyJsonDecoder {
    fn decode_total_power(&self, body: &[u8]) -> Result<f32, String> {
        let reading: IncomingReading = serde_json::from_slice(body)
            .map_err(|error| format!("invalid power-reading JSON: {error}"))?;

        let total_power = match reading {
            IncomingReading::Reading(reading) => reading.total_power,
            IncomingReading::ClientMessage { message } => message
                .parse()
                .map_err(|_| "message must contain a power value in watts".to_owned())?,
        };

        if !total_power.is_finite() || total_power < 0.0 {
            return Err("total power must be a finite, non-negative number".to_owned());
        }

        Ok(total_power)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_direct_and_existing_client_payloads() {
        let decoder = DummyJsonDecoder;

        assert_eq!(
            decoder.decode_total_power(br#"{"total_power":860.0}"#),
            Ok(860.0)
        );
        assert_eq!(
            decoder.decode_total_power(br#"{"message":"860"}"#),
            Ok(860.0)
        );
    }

    #[test]
    fn rejects_invalid_power() {
        let decoder = DummyJsonDecoder;

        assert!(
            decoder
                .decode_total_power(br#"{"total_power":-1}"#)
                .is_err()
        );
        assert!(
            decoder
                .decode_total_power(br#"{"message":"unknown"}"#)
                .is_err()
        );
    }
}
