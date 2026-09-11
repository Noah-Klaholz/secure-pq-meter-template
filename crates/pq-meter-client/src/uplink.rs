//! The HTTP/3 client the gateway delivers over, held so that losing the receiver is not
//! permanent.
//!
//! [`scion_http3::Client`] keeps a pool of connections, which is what makes sending a batch
//! several times a second cheap. The cost is that the pool outlives the thing it is pooling:
//! once the receiver has gone away and come back, the connection in the pool refers to a
//! peer that no longer exists, and every request on it times out. A gateway that reuses one
//! client for its whole life therefore never recovers from a receiver restart — it simply
//! stops delivering, for as long as it runs.
//!
//! That was invisible while a failed batch was discarded on the spot: every batch failed,
//! and every batch was thrown away, so nothing accumulated to show it. Now that readings are
//! kept until they are acknowledged, a pool that cannot recover means a queue that never
//! drains, so the connection is rebuilt after it has failed a few times in a row.

use scion_http3::{Client, Config, scion_quic::quic::config::QuicConfig};
use url::Url;

/// Consecutive failed batches after which the connection is rebuilt.
///
/// More than one, because a single failure is usually the network rather than the pool, and
/// a rebuild throws away a connection that is merely having a bad moment. Not many more,
/// because until it happens nothing is being delivered at all.
const REBUILD_AFTER_FAILURES: u32 = 3;

/// The connection to the receiver, and the state needed to make a new one.
pub struct Uplink {
    endhost_api: Url,
    client: Client,
    consecutive_failures: u32,
    rebuilds: u64,
}

impl Uplink {
    /// Prepares a client. This does no I/O: the connection is established with the first
    /// request, which is also why a gateway may start before its receiver exists.
    pub fn new(endhost_api: Url) -> Self {
        Self {
            client: build(endhost_api.clone()),
            endhost_api,
            consecutive_failures: 0,
            rebuilds: 0,
        }
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Records that a batch was delivered, or refused for its content rather than lost.
    ///
    /// Either way the connection carried a request to the receiver and back, which is the
    /// only thing that proves it still works.
    pub fn note_reachable(&mut self) {
        self.consecutive_failures = 0;
    }

    /// Records that a batch never reached the receiver, rebuilding the connection if this
    /// has now happened often enough to suspect the pool rather than the network.
    pub async fn note_unreachable(&mut self) {
        self.consecutive_failures += 1;
        if self.consecutive_failures < REBUILD_AFTER_FAILURES {
            return;
        }

        self.consecutive_failures = 0;
        self.rebuilds += 1;
        eprintln!(
            "Warning: {REBUILD_AFTER_FAILURES} batches in a row did not reach the receiver, rebuilding the connection"
        );

        // Swap the new client in first, so closing the stale one cannot hold up delivery.
        let stale = std::mem::replace(&mut self.client, build(self.endhost_api.clone()));
        stale.close().await;
    }

    /// How often the gateway has had to rebuild its connection to the receiver.
    #[cfg(test)]
    pub fn rebuilds(&self) -> u64 {
        self.rebuilds
    }
}

fn build(endhost_api: Url) -> Client {
    Client::new(
        Config::new(endhost_api)
            // TODO(security): development credential, not a real one. A gateway should ask
            // the AA (the authentication and authorization service) for a SNAP token that
            // identifies *this* device, so the network can refuse an unknown one before its
            // packets reach the application. The dummy token identifies nobody.
            .with_auth_token(snap_tokens::v0::dummy_snap_token())
            // TODO(security): the connection is encrypted but the peer is unauthenticated.
            // `verify_peer(false)` accepts any certificate, so anything that can answer on
            // the address can impersonate the receiver and collect the meter data. Pin the
            // backend certificate, or verify against a CA the gateway is provisioned with.
            .with_quic_config(QuicConfig::builder().verify_peer(false).build()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uplink() -> Uplink {
        Uplink::new("http://127.0.0.1:31000/".parse().unwrap())
    }

    #[tokio::test]
    async fn a_run_of_failures_shorter_than_the_threshold_keeps_the_connection() {
        let mut uplink = uplink();
        for _ in 0..REBUILD_AFTER_FAILURES - 1 {
            uplink.note_unreachable().await;
        }
        assert_eq!(uplink.rebuilds(), 0);
    }

    #[tokio::test]
    async fn enough_failures_in_a_row_rebuild_the_connection() {
        let mut uplink = uplink();
        for _ in 0..REBUILD_AFTER_FAILURES {
            uplink.note_unreachable().await;
        }
        assert_eq!(uplink.rebuilds(), 1);
    }

    /// A connection that is working again must not be rebuilt because of failures that
    /// happened before it recovered.
    #[tokio::test]
    async fn reaching_the_receiver_clears_the_run_of_failures() {
        let mut uplink = uplink();
        for _ in 0..REBUILD_AFTER_FAILURES - 1 {
            uplink.note_unreachable().await;
        }
        uplink.note_reachable();
        uplink.note_unreachable().await;
        assert_eq!(uplink.rebuilds(), 0);
    }

    /// A receiver that stays away is retried on a fresh connection each time, not once.
    #[tokio::test]
    async fn a_lasting_outage_keeps_rebuilding() {
        let mut uplink = uplink();
        for _ in 0..REBUILD_AFTER_FAILURES * 3 {
            uplink.note_unreachable().await;
        }
        assert_eq!(uplink.rebuilds(), 3);
    }
}
