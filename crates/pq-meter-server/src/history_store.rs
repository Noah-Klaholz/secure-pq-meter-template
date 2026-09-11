//! Append-only measurement archive. A complete request is committed as one JSON line.

use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
    path::Path,
};

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{history::History, input::MeterReading};

#[derive(Serialize, Deserialize)]
struct StoredBatch {
    schema_version: u8,
    received_at: String,
    readings: Vec<MeterReading>,
}

pub struct HistoryStore {
    file: File,
    failed: bool,
}

impl HistoryStore {
    /// Stream the archive into a bounded cache. Never replay old measurements through
    /// device inference: the next session needs to establish its own baseline.
    pub fn open(path: &Path, history: &mut History<MeterReading>) -> anyhow::Result<(Self, u64)> {
        let parent = path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent).context("creating history directory")?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("opening history archive {}", path.display()))?;
        file.try_lock()
            .context("history archive is already in use or cannot be locked")?;
        let mut reader = BufReader::new(&file);
        let mut line = Vec::new();
        let mut committed_bytes = 0;
        let mut count = 0u64;
        let mut last_at = None;
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            if !line.ends_with(b"\n") {
                // Only an incomplete final batch is recoverable. Never silently skip
                // malformed committed records or overwrite an unsupported schema.
                tracing::warn!(
                    "discarding an incomplete final history batch after an interrupted write"
                );
                file.set_len(committed_bytes)?;
                file.sync_all()?;
                break;
            }
            let batch: StoredBatch = serde_json::from_slice(&line)
                .with_context(|| format!("invalid history batch at byte {committed_bytes}"))?;
            anyhow::ensure!(
                batch.schema_version == 1,
                "unsupported history schema version"
            );
            anyhow::ensure!(!batch.readings.is_empty(), "empty history batch");
            let at = DateTime::parse_from_rfc3339(&batch.received_at)
                .context("invalid history timestamp")?
                .with_timezone(&Utc);
            anyhow::ensure!(
                last_at.is_none_or(|last| at >= last),
                "history timestamps are out of order"
            );
            last_at = Some(at);
            for reading in batch.readings {
                history.push(reading, at.into());
                count += 1;
            }
            committed_bytes += line.len() as u64;
        }
        drop(reader);
        file.seek(SeekFrom::End(0))?;
        Ok((
            Self {
                file,
                failed: false,
            },
            count,
        ))
    }

    /// Acknowledge only after the entire batch has reached disk. On failure, roll back
    /// any partial append before accepting another request.
    pub fn append(&mut self, readings: &[MeterReading], at: DateTime<Utc>) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.failed,
            "history archive needs recovery; restart the receiver"
        );
        anyhow::ensure!(!readings.is_empty(), "cannot archive an empty batch");
        let mut bytes = serde_json::to_vec(&StoredBatch {
            schema_version: 1,
            received_at: at.to_rfc3339(),
            readings: readings.to_vec(),
        })?;
        bytes.push(b'\n');
        let start = self.file.seek(SeekFrom::End(0))?;
        if let Err(error) = self
            .file
            .write_all(&bytes)
            .and_then(|()| self.file.sync_all())
        {
            if self
                .file
                .set_len(start)
                .and_then(|()| self.file.sync_all())
                .is_err()
            {
                self.failed = true;
            }
            return Err(error).context("persisting measurement history");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timestamp() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2025-01-02T03:04:05Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn restores_timestamps_and_full_readings_while_bounding_only_the_cache() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/history.jsonl");
        let mut cache = History::bounded(2);
        let (mut store, count) = HistoryStore::open(&path, &mut cache).unwrap();
        assert_eq!(count, 0);
        let reading = MeterReading {
            frequency_hz: Some(49.95),
            heartbeat: true,
            ..123.0.into()
        };
        store
            .append(&[100.0.into(), 200.0.into(), reading], timestamp())
            .unwrap();
        drop(store);
        let (mut store, count) = HistoryStore::open(&path, &mut cache).unwrap();
        assert_eq!(count, 3);
        assert_eq!(cache.all().len(), 2);
        assert_eq!(cache.latest().unwrap().data, reading);
        assert_eq!(
            cache.latest().unwrap().timestamp,
            std::time::SystemTime::from(timestamp())
        );
        store.append(&[300.0.into()], timestamp()).unwrap();
        drop(store);
        let mut cache = History::bounded(10);
        let (_, count) = HistoryStore::open(&path, &mut cache).unwrap();
        assert_eq!(count, 4);
        assert_eq!(cache.all().len(), 4);
    }

    #[test]
    fn recovers_an_incomplete_final_batch_and_accepts_new_appends() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.jsonl");
        let (mut store, _) = HistoryStore::open(&path, &mut History::bounded(10)).unwrap();
        store.append(&[100.0.into()], timestamp()).unwrap();
        store
            .file
            .write_all(b"{\"schema_version\":1,\"readings\":[")
            .unwrap();
        drop(store);
        let mut cache = History::bounded(10);
        let (mut store, count) = HistoryStore::open(&path, &mut cache).unwrap();
        assert_eq!(count, 1);
        store.append(&[200.0.into()], timestamp()).unwrap();
        drop(store);
        let (_, count) = HistoryStore::open(&path, &mut History::bounded(10)).unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn refuses_corrupt_or_unsupported_committed_records_without_overwriting_them() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.jsonl");
        for bytes in [
            "not json\n",
            "{\"schema_version\":2,\"received_at\":\"2025-01-02T03:04:05Z\",\"readings\":[{\"total_power\":1}]}\n",
        ] {
            std::fs::write(&path, bytes).unwrap();
            assert!(HistoryStore::open(&path, &mut History::bounded(10)).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn locks_the_archive_and_reports_write_failure_without_losing_committed_data() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.jsonl");
        let (mut store, _) = HistoryStore::open(&path, &mut History::bounded(10)).unwrap();
        store.append(&[100.0.into()], timestamp()).unwrap();
        assert!(HistoryStore::open(&path, &mut History::bounded(10)).is_err());
        let committed = std::fs::read(&path).unwrap();
        // A read-only handle provides a deterministic real I/O failure on all hosts.
        store.file = File::open(&path).unwrap();
        assert!(store.append(&[200.0.into()], timestamp()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), committed);
    }
}
