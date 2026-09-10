use std::time::SystemTime;

pub struct HistoryEntry<T> {
    pub timestamp: SystemTime,
    pub data: T,
}

pub struct History<T> {
    entries: Vec<HistoryEntry<T>>,
}

impl<T> History<T> {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn push(&mut self, data: T, timestamp: SystemTime) {
        self.entries.push(HistoryEntry { timestamp, data });
    }

    pub fn latest(&self) -> Option<&HistoryEntry<T>> {
        self.entries.last()
    }

    pub fn all(&self) -> &[HistoryEntry<T>] {
        &self.entries
    }

    pub fn recent(&self, n: usize) -> &[HistoryEntry<T>] {
        let start = self.entries.len().saturating_sub(n);
        &self.entries[start..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct TestMeasurement {
        total_power: f32,
        systime: i32,
        frequency_hz: f32,
        l1: TestL1Measurement,
    }

    #[derive(Debug, PartialEq)]
    struct TestL1Measurement {
        voltage_v: f32,
        current_a: f32,
        real_power_w: f32,
        apparent_power_va: f32,
        reactive_power_var: f32,
        cos_phi: f32,
        real_energy_consumed_wh: f32,
        thd_current_pct: f32,
    }

    fn measurement(total_power: f32, systime: i32) -> TestMeasurement {
        TestMeasurement {
            total_power,
            systime,
            frequency_hz: 50.0,
            l1: TestL1Measurement {
                voltage_v: 230.0,
                current_a: 1.5,
                real_power_w: total_power,
                apparent_power_va: 350.0,
                reactive_power_var: 20.0,
                cos_phi: 0.95,
                real_energy_consumed_wh: 1000.0,
                thd_current_pct: 2.0,
            },
        }
    }

    #[test]
    fn empty_history_has_no_entries() {
        let history = History::<TestMeasurement>::new();

        assert!(history.latest().is_none());
        assert!(history.all().is_empty());
        assert!(history.recent(1).is_empty());
    }

    #[test]
    fn push_one_preserves_the_complete_measurement_and_timestamp() {
        let mut history = History::new();
        let data = measurement(100.0, 1);
        let timestamp = SystemTime::UNIX_EPOCH;

        history.push(data, timestamp);

        let entry = history.latest().expect("history should contain one entry");
        assert_eq!(entry.timestamp, timestamp);
        assert_eq!(entry.data, measurement(100.0, 1));
    }

    #[test]
    fn push_multiple_preserves_insertion_order() {
        let mut history = History::new();

        history.push(measurement(100.0, 1), SystemTime::UNIX_EPOCH);
        history.push(
            measurement(200.0, 2),
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1),
        );

        assert_eq!(history.all()[0].data, measurement(100.0, 1));
        assert_eq!(history.all()[1].data, measurement(200.0, 2));
    }

    #[test]
    fn latest_returns_the_newest_entry() {
        let mut history = History::new();

        history.push(measurement(100.0, 1), SystemTime::UNIX_EPOCH);
        history.push(
            measurement(200.0, 2),
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1),
        );

        assert_eq!(history.latest().expect("history should not be empty").data, measurement(200.0, 2));
    }

    #[test]
    fn all_returns_all_entries_without_changing_order() {
        let mut history = History::new();

        history.push(measurement(100.0, 1), SystemTime::UNIX_EPOCH);
        history.push(
            measurement(200.0, 2),
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1),
        );
        history.push(
            measurement(300.0, 3),
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2),
        );

        assert_eq!(history.all().len(), 3);
        assert_eq!(history.all()[0].data.total_power, 100.0);
        assert_eq!(history.all()[1].data.total_power, 200.0);
        assert_eq!(history.all()[2].data.total_power, 300.0);
    }

    #[test]
    fn recent_returns_the_expected_entries() {
        let mut history = History::new();

        history.push(measurement(100.0, 1), SystemTime::UNIX_EPOCH);
        history.push(
            measurement(200.0, 2),
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1),
        );
        history.push(
            measurement(300.0, 3),
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2),
        );

        let recent = history.recent(2);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].data.total_power, 200.0);
        assert_eq!(recent[1].data.total_power, 300.0);
    }

    #[test]
    fn recent_larger_than_history_returns_all_entries() {
        let mut history = History::new();

        history.push(measurement(100.0, 1), SystemTime::UNIX_EPOCH);
        history.push(
            measurement(200.0, 2),
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1),
        );

        let recent = history.recent(10);
        assert_eq!(recent.len(), history.all().len());
        assert_eq!(recent[0].data, measurement(100.0, 1));
        assert_eq!(recent[1].data, measurement(200.0, 2));
    }
}