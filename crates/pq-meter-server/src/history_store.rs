//! SQLite-backed measurement archive with one-time import from the legacy JSONL file.

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::Path,
    time::SystemTime,
};

use anyhow::Context;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};

use crate::{
    history::{History, HistoryEntry},
    input::MeterReading,
};

#[derive(Serialize, Deserialize)]
struct StoredBatch {
    schema_version: u8,
    received_at: String,
    readings: Vec<MeterReading>,
}

pub struct HistoryStore {
    connection: Connection,
}

impl HistoryStore {
    #[allow(dead_code)]
    pub fn open(path: &Path, history: &mut History<MeterReading>) -> anyhow::Result<(Self, u64)> {
        let (database_path, legacy_path) = if path.extension().is_some_and(|ext| ext == "jsonl") {
            (path.with_extension("db"), path.to_owned())
        } else {
            (path.to_owned(), path.with_extension("jsonl"))
        };
        Self::open_with_legacy(&database_path, &legacy_path, history)
    }

    pub fn open_with_legacy(
        database_path: &Path,
        legacy_path: &Path,
        history: &mut History<MeterReading>,
    ) -> anyhow::Result<(Self, u64)> {
        let parent = database_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent).context("creating history database directory")?;
        let connection = Connection::open(database_path)
            .with_context(|| format!("opening history database {}", database_path.display()))?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS measurements (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                received_at TEXT NOT NULL,
                received_at_epoch INTEGER NOT NULL,
                total_power REAL NOT NULL,
                heartbeat INTEGER NOT NULL,
                reading_json TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_measurements_received_at
                ON measurements(received_at_epoch DESC, id DESC);
            CREATE TABLE IF NOT EXISTS history_migrations (
                source_path TEXT PRIMARY KEY,
                source_hash TEXT NOT NULL,
                imported_at TEXT NOT NULL
            );",
        )
        .context("initializing history database schema")?;

        if legacy_path.is_file() {
            Self::import_legacy(&connection, legacy_path)?;
        }
        let count = Self::load_cache(&connection, history)?;
        Ok((Self { connection }, count))
    }

    fn import_legacy(connection: &Connection, path: &Path) -> anyhow::Result<()> {
        let source_path = std::fs::canonicalize(path)
            .with_context(|| format!("resolving legacy history path {}", path.display()))?;
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading legacy history {}", path.display()))?;
        let source_hash = hash_bytes(&bytes);
        let key = source_path.to_string_lossy();
        let already_imported: Option<String> = connection
            .query_row(
                "SELECT source_hash FROM history_migrations WHERE source_path = ?1",
                [key.as_ref()],
                |row| row.get(0),
            )
            .optional()
            .context("checking legacy history migration")?;
        if let Some(imported_hash) = already_imported {
            anyhow::ensure!(
                imported_hash == source_hash,
                "legacy history file changed after migration; refusing duplicate import"
            );
            return Ok(());
        }

        let transaction = connection
            .unchecked_transaction()
            .context("starting legacy history migration")?;
        let mut committed_bytes = 0;
        let mut imported = 0;
        let mut last_at = None;
        for line in bytes.split_inclusive(|byte| *byte == b'\n') {
            if !line.ends_with(b"\n") {
                tracing::warn!("discarding an incomplete final history batch during migration");
                break;
            }
            let batch: StoredBatch = serde_json::from_slice(line)
                .with_context(|| format!("invalid history batch at byte {committed_bytes}"))?;
            anyhow::ensure!(batch.schema_version == 1, "unsupported history schema version");
            anyhow::ensure!(!batch.readings.is_empty(), "empty history batch");
            let at = DateTime::parse_from_rfc3339(&batch.received_at)
                .context("invalid history timestamp")?
                .with_timezone(&Utc);
            anyhow::ensure!(last_at.is_none_or(|last| at >= last), "history timestamps are out of order");
            last_at = Some(at);
            insert_readings(&transaction, &batch.readings, at)?;
            imported += batch.readings.len();
            committed_bytes += line.len();
        }
        transaction.execute(
            "INSERT INTO history_migrations (source_path, source_hash, imported_at)
             VALUES (?1, ?2, ?3)",
            params![key.as_ref(), source_hash, Utc::now().to_rfc3339()],
        )?;
        transaction.commit().context("committing legacy history migration")?;
        tracing::info!(path = %path.display(), imported, "imported legacy measurement history into SQLite");
        Ok(())
    }

    fn load_cache(
        connection: &Connection,
        history: &mut History<MeterReading>,
    ) -> anyhow::Result<u64> {
        let mut statement = connection.prepare(
            "SELECT received_at, reading_json FROM measurements
             ORDER BY received_at_epoch ASC, id ASC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut count = 0;
        for row in rows {
            let (at, reading) = row?;
            let at = DateTime::parse_from_rfc3339(&at)
                .context("invalid stored history timestamp")?
                .with_timezone(&Utc);
            history.push(
                serde_json::from_str(&reading).context("invalid stored measurement")?,
                SystemTime::from(at),
            );
            count += 1;
        }
        Ok(count)
    }

    pub fn append(&mut self, readings: &[MeterReading], at: DateTime<Utc>) -> anyhow::Result<()> {
        anyhow::ensure!(!readings.is_empty(), "cannot archive an empty batch");
        let transaction = self
            .connection
            .transaction()
            .context("starting history transaction")?;
        insert_readings(&transaction, readings, at)?;
        transaction.commit().context("committing measurement history")?;
        Ok(())
    }

    pub fn recent(
        &self,
        window: std::time::Duration,
    ) -> anyhow::Result<Vec<HistoryEntry<MeterReading>>> {
        let newest: Option<i64> = self
            .connection
            .query_row("SELECT MAX(received_at_epoch) FROM measurements", [], |row| {
                row.get(0)
            })?;
        let Some(newest) = newest else {
            return Ok(Vec::new());
        };
        let cutoff = newest.saturating_sub(window.as_millis().min(i64::MAX as u128) as i64);
        let mut statement = self.connection.prepare(
            "SELECT received_at, reading_json
             FROM measurements
             WHERE received_at_epoch >= ?1
             ORDER BY received_at_epoch DESC, id DESC
             LIMIT 600",
        )?;
        let rows = statement.query_map([cutoff], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut entries = Vec::new();
        for row in rows {
            let (at, reading) = row?;
            let at = DateTime::parse_from_rfc3339(&at)
                .context("invalid stored history timestamp")?
                .with_timezone(&Utc);
            entries.push(HistoryEntry {
                timestamp: SystemTime::from(at),
                data: serde_json::from_str(&reading).context("invalid stored measurement")?,
            });
        }
        entries.reverse();
        Ok(entries)
    }
}

fn insert_readings(
    transaction: &Transaction<'_>,
    readings: &[MeterReading],
    at: DateTime<Utc>,
) -> anyhow::Result<()> {
    let timestamp = at.to_rfc3339();
    for reading in readings {
        transaction.execute(
            "INSERT INTO measurements
                (received_at, received_at_epoch, total_power, heartbeat, reading_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                timestamp,
                at.timestamp_millis(),
                reading.total_power,
                reading.heartbeat,
                serde_json::to_string(reading)?,
            ],
        )?;
    }
    Ok(())
}

fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
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
    fn creates_database_and_preserves_complete_measurements_across_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.db");
        let reading = MeterReading { frequency_hz: Some(49.95), heartbeat: true, ..123.0.into() };
        let (mut store, count) = HistoryStore::open(&path, &mut History::bounded(10)).unwrap();
        assert_eq!(count, 0);
        store.append(&[reading], timestamp()).unwrap();
        drop(store);
        let mut cache = History::bounded(10);
        let (_, count) = HistoryStore::open(&path, &mut cache).unwrap();
        assert_eq!(count, 1);
        assert_eq!(cache.latest().unwrap().data, reading);
    }

    #[test]
    fn imports_legacy_json_once() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("history.db");
        let legacy = directory.path().join("history.jsonl");
        let batch = StoredBatch {
            schema_version: 1,
            received_at: timestamp().to_rfc3339(),
            readings: vec![100.0.into(), 200.0.into()],
        };
        std::fs::write(&legacy, format!("{}\n", serde_json::to_string(&batch).unwrap())).unwrap();
        let (_, count) = HistoryStore::open_with_legacy(&database, &legacy, &mut History::bounded(10)).unwrap();
        assert_eq!(count, 2);
        let (_, count) = HistoryStore::open_with_legacy(&database, &legacy, &mut History::bounded(10)).unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn recent_returns_the_newest_window_in_oldest_first_order() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.db");
        let (mut store, _) = HistoryStore::open(&path, &mut History::bounded(10)).unwrap();
        let first = timestamp();
        store.append(&[100.0.into()], first).unwrap();
        store
            .append(&[200.0.into()], first + chrono::Duration::seconds(30))
            .unwrap();
        store
            .append(&[300.0.into()], first + chrono::Duration::seconds(90))
            .unwrap();

        let recent = store.recent(std::time::Duration::from_secs(60)).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].data.total_power, 200.0);
        assert_eq!(recent[1].data.total_power, 300.0);
    }

    #[test]
    fn failed_import_leaves_legacy_json_untouched() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("history.db");
        let legacy = directory.path().join("history.jsonl");
        let contents = b"not json\n";
        std::fs::write(&legacy, contents).unwrap();

        assert!(HistoryStore::open_with_legacy(
            &database,
            &legacy,
            &mut History::bounded(10),
        )
        .is_err());
        assert_eq!(std::fs::read(&legacy).unwrap(), contents);
    }
}
