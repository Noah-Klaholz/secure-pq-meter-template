//! The connection to the meter, held so that losing it is not fatal.
//!
//! A gateway that exits when the meter hiccups is a gateway that stops metering. The meter
//! and the gateway are separate devices on a link the gateway does not control, and either
//! can restart without the other: this reconnects on its own schedule and reports a read
//! failure as a gap in the data rather than as the end of the program.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::time::Instant;
use tokio_modbus::Slave;
use umg605_modbus_client::{Snapshot, Umg605ProClient};

use crate::retry::Backoff;

/// Shortest and longest wait between attempts to reach the meter.
const RECONNECT_BASE: Duration = Duration::from_millis(100);
const RECONNECT_MAX: Duration = Duration::from_secs(5);

/// A meter connection that re-establishes itself.
pub struct MeterConnection {
    address: SocketAddr,
    unit: Slave,
    timeout: Duration,
    client: Option<Umg605ProClient>,
    backoff: Backoff,
    /// When the next attempt to reach the meter may be made.
    retry_at: Option<Instant>,
    /// Whether the meter has ever answered, so the first connection is not a reconnection.
    connected_before: bool,
    reconnects: u64,
}

impl MeterConnection {
    /// Describes the meter without touching the network.
    ///
    /// Connecting is deferred to the first read, so the gateway can start before the meter
    /// does and simply wait for it.
    pub fn new(address: SocketAddr, unit: Slave, timeout: Duration) -> Self {
        Self {
            address,
            unit,
            timeout,
            client: None,
            backoff: Backoff::new(RECONNECT_BASE, RECONNECT_MAX),
            retry_at: None,
            connected_before: false,
            reconnects: 0,
        }
    }

    /// Reads the meter, or reports that it is unavailable right now.
    ///
    /// Returns `None` for a meter that is unreachable, that failed this read, or that is
    /// still inside its reconnect backoff. None of those end the acquisition loop.
    pub async fn snapshot(&mut self) -> Option<Snapshot> {
        if self.client.is_none() && !self.connect().await {
            return None;
        }

        let client = self.client.as_mut()?;
        match client.snapshot().await {
            Ok(snapshot) => {
                self.backoff.reset();
                Some(snapshot)
            }
            Err(error) => {
                eprintln!("Warning: reading the meter failed, reconnecting: {error}");
                // The connection is not trustworthy after a failed transaction: a timed-out
                // read can still have a reply in flight, which would answer the next one.
                self.client = None;
                self.schedule_retry();
                None
            }
        }
    }

    /// How often the gateway had to re-establish a connection it previously had.
    pub fn reconnects(&self) -> u64 {
        self.reconnects
    }

    /// Attempts to connect, unless a previous failure asked for a longer wait.
    async fn connect(&mut self) -> bool {
        if self.retry_at.is_some_and(|at| Instant::now() < at) {
            return false;
        }
        self.retry_at = None;

        match Umg605ProClient::connect_tcp(self.address, self.unit, self.timeout).await {
            Ok(client) => {
                if self.connected_before {
                    self.reconnects += 1;
                    println!("Reconnected to the meter at {}", self.address);
                } else {
                    self.connected_before = true;
                }
                self.client = Some(client);
                self.backoff.reset();
                true
            }
            Err(error) => {
                eprintln!(
                    "Warning: cannot reach the meter at {}: {error}",
                    self.address
                );
                self.schedule_retry();
                false
            }
        }
    }

    fn schedule_retry(&mut self) {
        self.retry_at = Some(Instant::now() + self.backoff.next_delay());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    /// Port 1 on the loopback interface: nothing listens there, so connecting fails fast.
    fn unreachable_meter() -> MeterConnection {
        MeterConnection::new(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1),
            Slave(1),
            Duration::from_millis(50),
        )
    }

    #[tokio::test]
    async fn a_meter_that_never_answers_yields_no_snapshot_and_does_not_panic() {
        let mut meter = unreachable_meter();
        assert!(meter.snapshot().await.is_none());
        assert!(meter.snapshot().await.is_none());
    }

    /// The gateway must be able to start before the meter, so a failed first connection is
    /// a wait rather than a reconnection.
    #[tokio::test]
    async fn failing_to_connect_the_first_time_is_not_counted_as_a_reconnect() {
        let mut meter = unreachable_meter();
        meter.snapshot().await;
        assert_eq!(meter.reconnects(), 0);
    }

    /// Backoff must actually hold the loop off, or an unreachable meter would be retried on
    /// every tick.
    #[tokio::test(start_paused = true)]
    async fn a_failed_attempt_defers_the_next_one() {
        let mut meter = unreachable_meter();
        meter.snapshot().await;
        assert!(meter.retry_at.is_some());

        let deferred = meter.retry_at;
        meter.snapshot().await;
        assert_eq!(
            meter.retry_at, deferred,
            "the second attempt should have been skipped, not made"
        );
    }
}
