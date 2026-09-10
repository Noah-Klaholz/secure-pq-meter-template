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