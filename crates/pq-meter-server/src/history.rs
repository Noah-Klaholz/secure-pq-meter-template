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
}