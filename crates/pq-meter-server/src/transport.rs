//! What the gateway reports about the SCION link it delivers over.
//!
//! The receiver cannot see most of this for itself. Which path the packets took, how many
//! readings are waiting on the gateway, and how often it had to fail over are facts about
//! the *sending* end, and the gateway is the SCION endhost that holds them. It attaches
//! them to each batch as headers, which keeps the measurement body exactly as documented
//! and separates metadata about the delivery from the measurement itself.
//!
//! The receiver still judges liveness for itself, from when a batch actually arrived; a
//! gateway that has stopped sending cannot claim to be connected.

use axum::http::HeaderMap;
use serde::Serialize;

/// The SCION path the gateway currently sends over, e.g. `1-ff00:0:132 1>3 2-ff00:0:212`.
pub const HEADER_PATH: &str = "x-pq-scion-path";
/// Readings buffered on the gateway and not yet acknowledged.
pub const HEADER_QUEUED: &str = "x-pq-queued-readings";
/// Round trip of the gateway's previous batch, in milliseconds.
pub const HEADER_ACK_LATENCY: &str = "x-pq-ack-latency-ms";
/// How often the gateway has changed path since it started.
pub const HEADER_FAILOVERS: &str = "x-pq-failover-count";
/// Readings the gateway gave up on: shed from a full queue, or refused by this receiver.
pub const HEADER_DROPPED: &str = "x-pq-dropped-readings";
/// How often the gateway had to re-establish its Modbus connection to the meter.
pub const HEADER_RECONNECTS: &str = "x-pq-modbus-reconnects";

/// Longest path string that is kept. A SCION path over a handful of ASes is far shorter;
/// the cap stops a peer from pushing an unbounded string into the dashboard's state.
const MAX_PATH_LEN: usize = 256;

/// Transport state as reported by the gateway on its most recent batch.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct GatewayTransport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scion_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queued_readings: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_ack_latency_ms: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failover_count: Option<u32>,
    /// Readings the gateway admits it lost. A non-zero value here is the one honest signal
    /// that the archive has a hole in it; without it a gap is indistinguishable from an
    /// installation that had nothing to report.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dropped_readings: Option<u64>,
    /// How often the gateway lost and regained the meter, which is a gap in acquisition
    /// rather than in delivery.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modbus_reconnects: Option<u64>,
}

impl GatewayTransport {
    /// Whether any field was reported at all. A gateway that sends no headers, such as one
    /// built before this existed, leaves the panel empty rather than showing made-up zeros.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Reads the transport headers of a batch.
    ///
    /// Every field is optional and independently parsed: a gateway that reports only some
    /// of them still gets those shown, and a malformed value is dropped rather than
    /// failing the batch. Measurements must not be rejected over their metadata.
    pub fn from_headers(headers: &HeaderMap) -> Self {
        Self {
            scion_path: headers
                .get(HEADER_PATH)
                .and_then(|value| value.to_str().ok())
                .and_then(sanitize_path),
            queued_readings: parse_header(headers, HEADER_QUEUED),
            last_ack_latency_ms: parse_header::<f32>(headers, HEADER_ACK_LATENCY)
                .filter(|value| value.is_finite() && *value >= 0.0),
            failover_count: parse_header(headers, HEADER_FAILOVERS),
            dropped_readings: parse_header(headers, HEADER_DROPPED),
            modbus_reconnects: parse_header(headers, HEADER_RECONNECTS),
        }
    }
}

fn parse_header<T: std::str::FromStr>(headers: &HeaderMap, name: &str) -> Option<T> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
}

/// Keeps a path string to printable characters and a sane length.
///
/// The value arrives from the network and ends up on the dashboard. The dashboard renders
/// it as text rather than markup, so this is defence in depth: it bounds what is stored
/// and keeps control characters out of logs and of the JSON the API serves.
fn sanitize_path(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_PATH_LEN)
        .collect();
    (!cleaned.is_empty()).then_some(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    #[test]
    fn reads_a_complete_report() {
        let reported = GatewayTransport::from_headers(&headers(&[
            (HEADER_PATH, "1-ff00:0:132 1>3 2-ff00:0:212"),
            (HEADER_QUEUED, "7"),
            (HEADER_ACK_LATENCY, "12.5"),
            (HEADER_FAILOVERS, "2"),
            (HEADER_DROPPED, "13"),
            (HEADER_RECONNECTS, "4"),
        ]));

        assert_eq!(
            reported,
            GatewayTransport {
                scion_path: Some("1-ff00:0:132 1>3 2-ff00:0:212".to_owned()),
                queued_readings: Some(7),
                last_ack_latency_ms: Some(12.5),
                failover_count: Some(2),
                dropped_readings: Some(13),
                modbus_reconnects: Some(4),
            }
        );
        assert!(!reported.is_empty());
    }

    #[test]
    fn a_gateway_that_reports_nothing_leaves_the_state_empty() {
        let reported = GatewayTransport::from_headers(&HeaderMap::new());
        assert!(reported.is_empty());
        // Nothing reported must serialize to nothing, not to zeros that look measured.
        assert_eq!(
            serde_json::to_value(&reported).unwrap(),
            serde_json::json!({})
        );
    }

    #[test]
    fn a_malformed_field_is_dropped_without_losing_the_others() {
        let reported = GatewayTransport::from_headers(&headers(&[
            (HEADER_PATH, "1-ff00:0:132 1>3 2-ff00:0:212"),
            (HEADER_QUEUED, "not a number"),
            (HEADER_ACK_LATENCY, "NaN"),
            (HEADER_FAILOVERS, "-1"),
            (HEADER_DROPPED, "3.5"),
        ]));

        assert_eq!(
            reported.scion_path.as_deref(),
            Some("1-ff00:0:132 1>3 2-ff00:0:212")
        );
        assert_eq!(reported.queued_readings, None);
        assert_eq!(reported.last_ack_latency_ms, None, "NaN is not a latency");
        assert_eq!(reported.failover_count, None, "a count cannot be negative");
        assert_eq!(reported.dropped_readings, None, "a count is not fractional");
    }

    /// A gateway built before these headers existed reports the fields it knows and nothing
    /// else, and must not have zeros invented for the rest.
    #[test]
    fn an_older_gateway_reporting_only_the_original_fields_still_works() {
        let reported = GatewayTransport::from_headers(&headers(&[
            (HEADER_PATH, "1-ff00:0:132 1>3 2-ff00:0:212"),
            (HEADER_QUEUED, "7"),
        ]));

        assert_eq!(reported.queued_readings, Some(7));
        assert_eq!(reported.dropped_readings, None);
        assert_eq!(reported.modbus_reconnects, None);
        assert!(!reported.is_empty());

        let serialized = serde_json::to_value(&reported).unwrap();
        assert!(serialized.get("dropped_readings").is_none());
        assert!(serialized.get("modbus_reconnects").is_none());
    }

    /// A gateway that has lost nothing must say so, rather than stay silent: zero dropped is
    /// a claim the dashboard should be able to show.
    #[test]
    fn a_reported_zero_is_kept_and_is_not_an_absence() {
        let reported = GatewayTransport::from_headers(&headers(&[
            (HEADER_DROPPED, "0"),
            (HEADER_RECONNECTS, "0"),
        ]));
        assert_eq!(reported.dropped_readings, Some(0));
        assert_eq!(reported.modbus_reconnects, Some(0));
        assert!(!reported.is_empty());
    }

    #[test]
    fn bounds_and_cleans_a_path_reported_by_the_peer() {
        let long = "A".repeat(MAX_PATH_LEN * 2);
        let reported =
            GatewayTransport::from_headers(&headers(&[(HEADER_PATH, &format!("  {long}  "))]));
        assert_eq!(reported.scion_path.unwrap().len(), MAX_PATH_LEN);

        // An empty or whitespace-only report is an absence, not a path.
        assert_eq!(
            GatewayTransport::from_headers(&headers(&[(HEADER_PATH, "   ")])).scion_path,
            None
        );
    }
}
