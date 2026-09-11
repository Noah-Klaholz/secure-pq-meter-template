//! What the gateway can say about the SCION link it sends over.
//!
//! The receiver cannot see any of this: which of the available paths the packets take, how
//! many readings are still waiting here, and how long the last acknowledgement took are all
//! facts about this end. The gateway measures them and attaches them to each batch, so the
//! dashboard can show the transport as well as the measurements.
//!
//! None of it is required for delivering readings. If the path manager cannot be reached,
//! the gateway keeps sending measurements and simply reports less about itself.

use std::time::{Duration, Instant, SystemTime};

use scion_stack::{
    path::{fetcher::PathFetcherImpl, manager::MultiPathManager},
    stack::ScionStackBuilder,
};
use sciparse::identifier::isd_asn::IsdAsn;
use sciparse::path::ScionPath;
use url::Url;

/// Header names the receiver reads these from.
pub const HEADER_PATH: &str = "x-pq-scion-path";
pub const HEADER_QUEUED: &str = "x-pq-queued-readings";
pub const HEADER_ACK_LATENCY: &str = "x-pq-ack-latency-ms";
pub const HEADER_FAILOVERS: &str = "x-pq-failover-count";
pub const HEADER_DROPPED: &str = "x-pq-dropped-readings";
pub const HEADER_RECONNECTS: &str = "x-pq-modbus-reconnects";

/// How often the selected path is looked up again.
///
/// A path lookup talks to the endhost API, so it runs on its own schedule rather than on
/// every batch: the gateway can batch several times a second.
const PATH_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// Tracks the SCION path in use and counts how often it changed.
pub struct ScionLink {
    /// Kept alive for as long as the path manager it created is used.
    _stack: scion_stack::stack::ScionStack,
    path_manager: MultiPathManager<PathFetcherImpl>,
    local_as: IsdAsn,
    server_as: IsdAsn,
    current_path: Option<String>,
    failover_count: u32,
    last_refresh: Option<Instant>,
}

impl ScionLink {
    /// Attaches to the same endhost API the HTTP/3 client uses, to ask it for paths.
    ///
    /// This is a second, read-only view of the network: it selects nothing and sends
    /// nothing, it only reports which path the stack would pick.
    pub async fn attach(endhost_api: Url, server_as: IsdAsn) -> anyhow::Result<Self> {
        let stack = ScionStackBuilder::new()
            .with_endhost_api(endhost_api)
            // TODO(security): development credential, as in the HTTP/3 client. Both should
            // use the same real SNAP token once the gateway has one.
            .with_auth_token(snap_tokens::v0::dummy_snap_token())
            .build()
            .await?;
        let local_as = stack
            .local_ases()
            .first()
            .copied()
            .ok_or_else(|| anyhow::anyhow!("the endhost API reported no local AS"))?;

        Ok(Self {
            path_manager: stack.create_path_manager(),
            _stack: stack,
            local_as,
            server_as,
            current_path: None,
            failover_count: 0,
            last_refresh: None,
        })
    }

    /// Looks the selected path up again if it is time to, counting a change as a failover.
    ///
    /// A change of path is exactly what SCION buys here: when one path stops working the
    /// stack moves to another, and the count is how visible that is from the outside.
    pub async fn refresh(&mut self) {
        let now = Instant::now();
        let due = self
            .last_refresh
            .is_none_or(|last| now.duration_since(last) >= PATH_REFRESH_INTERVAL);
        if !due {
            return;
        }
        self.last_refresh = Some(now);

        let Ok(path) = self
            .path_manager
            .path(self.local_as, self.server_as, SystemTime::now())
            .await
        else {
            // Losing sight of the path is not worth a message every few seconds, and the
            // last known path stays on display until a new one is selected.
            return;
        };

        let described = describe(&path);
        match &self.current_path {
            Some(current) if *current == described => {}
            Some(_) => {
                self.failover_count += 1;
                self.current_path = Some(described);
            }
            None => self.current_path = Some(described),
        }
    }

    pub fn current_path(&self) -> Option<&str> {
        self.current_path.as_deref()
    }

    pub fn failover_count(&self) -> u32 {
        self.failover_count
    }
}

/// Renders a path the way the SDK prints it, as the AS-level hops it traverses.
fn describe(path: &ScionPath) -> String {
    let mut rendered = String::new();
    if path.format_interfaces(&mut rendered).is_err() || rendered.trim().is_empty() {
        return path.to_string();
    }
    rendered
}

/// What the gateway reports about itself with each batch.
///
/// These are counters, not estimates. `queued_readings` is the real depth of the spool at
/// the moment the batch was built, including the readings in the batch itself, because they
/// are not delivered until the receiver says so. A batch that fails leaves the depth where
/// it was, and a batch the receiver refuses shows up in `dropped_readings`. The README notes
/// that these figures are self-reported and therefore not evidence; that is a reason for
/// them to be accurate, not a licence for them to be optimistic.
#[derive(Clone, Copy, Debug, Default)]
pub struct DeliveryStats {
    /// Readings buffered here and not yet acknowledged by the receiver.
    pub queued_readings: usize,
    /// Round trip of the previous batch. The current one cannot know its own yet.
    pub last_ack_latency_ms: Option<f32>,
    /// Readings the gateway gave up on: evicted from a full spool, or refused outright.
    pub dropped_readings: u64,
    /// How often the connection to the meter had to be re-established.
    pub modbus_reconnects: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_stats_start_empty() {
        let stats = DeliveryStats::default();
        assert_eq!(stats.queued_readings, 0);
        assert_eq!(stats.last_ack_latency_ms, None);
        assert_eq!(stats.dropped_readings, 0);
        assert_eq!(stats.modbus_reconnects, 0);
    }
}
