import json
import sqlite3
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path

from anomaly import combine_measurements, load_jsonl_history, load_sqlite_history


class TestHistoryLoaders(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        self.jsonl_path = self.root / "history.jsonl"
        self.database_path = self.root / "history.db"

    def tearDown(self):
        self.directory.cleanup()

    @staticmethod
    def reading(power=100.0, **overrides):
        reading = {
            "total_power": power,
            "frequency_hz": 50.0,
            "l1": {
                "current_a": 0.5,
                "voltage_v": 230.0,
                "reactive_power_var": -10.0,
                "cos_phi": 0.95,
                "thd_current_pct": 2.0,
            },
            "heartbeat": False,
        }
        reading.update(overrides)
        return reading

    def create_database(self, rows):
        with sqlite3.connect(self.database_path) as database:
            database.execute(
                "CREATE TABLE measurements ("
                "id INTEGER PRIMARY KEY AUTOINCREMENT, "
                "received_at TEXT NOT NULL, "
                "received_at_epoch INTEGER NOT NULL, "
                "total_power REAL NOT NULL, "
                "heartbeat INTEGER NOT NULL, "
                "reading_json TEXT NOT NULL)"
            )
            database.executemany(
                "INSERT INTO measurements "
                "(received_at, received_at_epoch, total_power, heartbeat, reading_json) "
                "VALUES (?, ?, ?, ?, ?)",
                rows,
            )

    def test_jsonl_loading_normalizes_fields(self):
        self.jsonl_path.write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "received_at": "2026-09-11T10:00:00Z",
                    "readings": [self.reading(123.0)],
                }
            )
            + "\n"
        )

        measurements = load_jsonl_history(self.jsonl_path)

        self.assertEqual(len(measurements), 1)
        measurement = measurements[0]
        self.assertEqual(measurement["source"], "jsonl")
        self.assertEqual(measurement["total_power"], 123.0)
        self.assertEqual(measurement["timestamp"].tzinfo, timezone.utc)
        self.assertEqual(measurement["l1"]["reactive_power_var"], -10.0)

    def test_sqlite_loading_reads_reading_json_and_dedicated_fallbacks(self):
        timestamp = "2026-09-11T10:00:00+00:00"
        reading = self.reading(123.0)
        reading.pop("heartbeat")
        self.create_database(
            [(timestamp, 0, 123.0, 1, json.dumps(reading))]
        )

        measurements = load_sqlite_history(self.database_path)

        self.assertEqual(len(measurements), 1)
        self.assertEqual(measurements[0]["source"], "sqlite")
        self.assertTrue(measurements[0]["heartbeat"])
        self.assertEqual(measurements[0]["l1"]["current_a"], 0.5)

    def test_combined_history_is_chronological(self):
        first = {
            "timestamp": datetime(2026, 9, 11, 9, tzinfo=timezone.utc),
            "source": "jsonl",
            "total_power": 1.0,
        }
        second = {
            "timestamp": datetime(2026, 9, 11, 10, tzinfo=timezone.utc),
            "source": "sqlite",
            "total_power": 2.0,
        }

        combined = combine_measurements([second], [first])

        self.assertEqual([item["total_power"] for item in combined], [1.0, 2.0])

    def test_combined_history_deduplicates_timestamp_and_power(self):
        timestamp = datetime(2026, 9, 11, 10, tzinfo=timezone.utc)
        jsonl_measurement = {
            "timestamp": timestamp,
            "source": "jsonl",
            "total_power": 123.0,
            "marker": "historical",
        }
        sqlite_measurement = {
            "timestamp": timestamp,
            "source": "sqlite",
            "total_power": 123.0,
            "marker": "runtime",
        }

        combined = combine_measurements([jsonl_measurement], [sqlite_measurement])

        self.assertEqual(len(combined), 1)
        self.assertEqual(combined[0]["source"], "jsonl")

    def test_missing_optional_fields_are_none(self):
        self.jsonl_path.write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "received_at": "2026-09-11T10:00:00Z",
                    "readings": [{"total_power": 123.0}],
                }
            )
            + "\n"
        )

        measurement = load_jsonl_history(self.jsonl_path)[0]

        self.assertIsNone(measurement["frequency_hz"])
        self.assertIsNone(measurement["l1"]["current_a"])
        self.assertIsNone(measurement["l1"]["voltage_v"])
        self.assertIsNone(measurement["l1"]["reactive_power_var"])
        self.assertIsNone(measurement["l1"]["cos_phi"])
        self.assertIsNone(measurement["l1"]["thd_current_pct"])

    def test_malformed_jsonl_and_reading_json_are_skipped(self):
        self.jsonl_path.write_text(
            "not json\n"
            + json.dumps(
                {
                    "schema_version": 1,
                    "received_at": "2026-09-11T10:00:00Z",
                    "readings": [self.reading(123.0), "incomplete"],
                }
            )
            + "\n"
        )
        self.create_database(
            [
                (
                    "2026-09-11T10:01:00+00:00",
                    0,
                    124.0,
                    0,
                    "{malformed",
                ),
            ]
        )

        self.assertEqual(len(load_jsonl_history(self.jsonl_path)), 1)
        self.assertEqual(len(load_sqlite_history(self.database_path)), 0)


if __name__ == "__main__":
    unittest.main()
