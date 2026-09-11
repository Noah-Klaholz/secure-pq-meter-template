//! The gateway's queue of readings that the receiver has not acknowledged yet.
//!
//! Acquisition and upload run as separate tasks, so this is the one thing they share. It
//! exists because a reading must survive the attempt to send it: the gateway hands a batch
//! to the uploader, and the readings stay queued here until the receiver says it has them.
//!
//! The queue is bounded. A gateway that cannot reach the receiver for long enough will run
//! out of room, and the choice of what to lose is a real one: this evicts the *oldest*
//! reading, so a long outage leaves a gap in the middle of the archive rather than a
//! dashboard frozen at the moment the link went down.

use std::collections::VecDeque;

/// A batch handed to the uploader, and the position it was taken from.
///
/// The position matters because the uploader does not hold the lock while it sends. The
/// acquisition task can evict the readings in this lease in the meantime, and committing by
/// position rather than by count is what stops the acknowledgement from removing whatever
/// happens to sit at the front by then.
#[derive(Clone, Debug)]
pub struct Lease {
    /// Absolute position of the first reading in the lease.
    start: u64,
    pub readings: Vec<serde_json::Value>,
}

impl Lease {
    pub fn len(&self) -> usize {
        self.readings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.readings.is_empty()
    }

    /// One past the absolute position of the last reading in the lease.
    fn end(&self) -> u64 {
        self.start + self.readings.len() as u64
    }
}

/// Readings the gateway gave up on, by reason.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpoolStats {
    /// Evicted to make room, because the receiver was unreachable for too long.
    pub dropped_overflow: u64,
    /// Refused by the receiver. Retrying these forever would wedge the queue.
    pub dropped_rejected: u64,
}

impl SpoolStats {
    pub fn dropped_total(&self) -> u64 {
        self.dropped_overflow + self.dropped_rejected
    }
}

/// A bounded, in-order queue of readings awaiting acknowledgement.
pub struct Spool {
    queue: VecDeque<serde_json::Value>,
    /// Absolute position of the front of `queue`, counting every reading ever pushed.
    head: u64,
    capacity: usize,
    stats: SpoolStats,
}

impl Spool {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            queue: VecDeque::with_capacity(capacity.min(1024)),
            head: 0,
            capacity,
            stats: SpoolStats::default(),
        }
    }

    /// Queues a reading, evicting the oldest one if there is no room.
    pub fn push(&mut self, reading: serde_json::Value) {
        while self.queue.len() >= self.capacity {
            self.queue.pop_front();
            self.head += 1;
            self.stats.dropped_overflow += 1;
        }
        self.queue.push_back(reading);
    }

    /// Takes up to `max` readings from the front without removing them.
    ///
    /// They stay queued so that a failed send loses nothing; `commit` is what removes them.
    pub fn peek(&self, max: usize) -> Lease {
        Lease {
            start: self.head,
            readings: self.queue.iter().take(max).cloned().collect(),
        }
    }

    /// Removes the readings of an acknowledged lease.
    pub fn commit(&mut self, lease: &Lease) {
        self.remove_through(lease.end());
    }

    /// Removes the readings of a lease the receiver refused, and counts them as lost.
    pub fn reject(&mut self, lease: &Lease) {
        let removed = self.remove_through(lease.end());
        self.stats.dropped_rejected += removed as u64;
    }

    /// Drops everything up to absolute position `end`, ignoring what was evicted already.
    fn remove_through(&mut self, end: u64) -> usize {
        let Some(ahead) = end.checked_sub(self.head).filter(|ahead| *ahead > 0) else {
            // Every reading in the lease was evicted while it was in flight.
            return 0;
        };
        let remove = (ahead as usize).min(self.queue.len());
        self.queue.drain(..remove);
        self.head += remove as u64;
        remove
    }

    /// Readings queued here and not yet acknowledged, including any in flight.
    pub fn depth(&self) -> usize {
        self.queue.len()
    }

    pub fn stats(&self) -> SpoolStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spool_of(capacity: usize, readings: usize) -> Spool {
        let mut spool = Spool::new(capacity);
        for index in 0..readings {
            spool.push(json!({ "n": index }));
        }
        spool
    }

    fn values(lease: &Lease) -> Vec<i64> {
        lease
            .readings
            .iter()
            .map(|reading| reading["n"].as_i64().unwrap())
            .collect()
    }

    #[test]
    fn peek_leaves_the_readings_queued_so_a_failed_send_loses_nothing() {
        let spool = spool_of(10, 5);
        let lease = spool.peek(3);
        assert_eq!(values(&lease), vec![0, 1, 2]);
        assert_eq!(spool.depth(), 5);
    }

    #[test]
    fn commit_removes_exactly_the_acknowledged_prefix() {
        let mut spool = spool_of(10, 5);
        let lease = spool.peek(3);
        spool.commit(&lease);
        assert_eq!(spool.depth(), 2);
        assert_eq!(values(&spool.peek(10)), vec![3, 4]);
        assert_eq!(spool.stats(), SpoolStats::default());
    }

    #[test]
    fn overflow_evicts_the_oldest_and_counts_it() {
        let spool = spool_of(3, 5);
        assert_eq!(spool.depth(), 3);
        assert_eq!(values(&spool.peek(10)), vec![2, 3, 4]);
        assert_eq!(spool.stats().dropped_overflow, 2);
    }

    #[test]
    fn rejecting_a_lease_counts_the_readings_as_lost() {
        let mut spool = spool_of(10, 5);
        let lease = spool.peek(2);
        spool.reject(&lease);
        assert_eq!(spool.depth(), 3);
        assert_eq!(spool.stats().dropped_rejected, 2);
        assert_eq!(spool.stats().dropped_total(), 2);
    }

    /// The uploader does not hold the lock while it sends, so acquisition can evict the
    /// readings it is sending. The acknowledgement must then remove nothing rather than
    /// remove whatever moved to the front in the meantime.
    #[test]
    fn committing_a_lease_that_was_evicted_meanwhile_removes_nothing_newer() {
        let mut spool = spool_of(3, 3);
        let lease = spool.peek(3);
        assert_eq!(values(&lease), vec![0, 1, 2]);

        // The link is down and acquisition keeps going: the whole lease is evicted.
        for index in 3..6 {
            spool.push(json!({ "n": index }));
        }
        assert_eq!(values(&spool.peek(10)), vec![3, 4, 5]);

        spool.commit(&lease);
        assert_eq!(values(&spool.peek(10)), vec![3, 4, 5]);
        assert_eq!(spool.depth(), 3);
    }

    /// The partial case: some of the lease survived the eviction, some did not.
    #[test]
    fn committing_a_partly_evicted_lease_removes_only_what_it_covered() {
        let mut spool = spool_of(4, 4);
        let lease = spool.peek(2);
        assert_eq!(values(&lease), vec![0, 1]);

        spool.push(json!({ "n": 4 }));
        assert_eq!(values(&spool.peek(10)), vec![1, 2, 3, 4]);

        spool.commit(&lease);
        assert_eq!(values(&spool.peek(10)), vec![2, 3, 4]);
    }

    #[test]
    fn a_zero_capacity_spool_still_holds_one_reading() {
        let spool = spool_of(0, 3);
        assert_eq!(spool.depth(), 1);
        assert_eq!(values(&spool.peek(10)), vec![2]);
    }
}
