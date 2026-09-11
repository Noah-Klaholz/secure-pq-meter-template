import json
import sqlite3
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path

from anomaly import (
    build_baseline,
    combine_measurements,
    extract_features,
    load_jsonl_history,
    load_sqlite_history,
    score_history,
    score_measurement,
)


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

    def test_median_and_mad_calculation(self):
        measurements = [
            {"total_power": value, "l1": {}}
            for value in [1.0, 2.0, 3.0, 4.0, 100.0]
        ]

        baseline = build_baseline(measurements, min_valid_samples=1)

        self.assertEqual(baseline["total_power"]["median"], 3.0)
        self.assertEqual(baseline["total_power"]["mad"], 1.0)

    def test_stable_baseline_produces_low_scores(self):
        measurements = [self.reading(100.0) for _ in range(30)]
        baseline = build_baseline(measurements)

        result = score_measurement(self.reading(100.0), baseline)

        self.assertTrue(result["scorable"])
        self.assertEqual(result["overall_score"], 0.0)
        self.assertFalse(result["is_anomaly"])

    def test_strong_total_power_deviation_is_detected(self):
        baseline = build_baseline([self.reading(100.0) for _ in range(30)])

        result = score_measurement(self.reading(250.0), baseline)

        self.assertTrue(result["is_anomaly"])
        self.assertEqual(result["strongest_feature"], "total_power")
        self.assertGreaterEqual(result["feature_scores"]["total_power"], 3.5)

    def test_strong_thd_deviation_is_detected(self):
        baseline = build_baseline([self.reading(100.0) for _ in range(30)])
        anomalous = self.reading(100.0)
        anomalous["l1"]["thd_current_pct"] = 100.0

        result = score_measurement(anomalous, baseline)

        self.assertTrue(result["is_anomaly"])
        self.assertEqual(result["strongest_feature"], "l1.thd_current_pct")

    def test_delta_total_power_is_calculated(self):
        previous = self.reading(100.0)
        current = self.reading(125.0)

        features = extract_features(current, previous)

        self.assertEqual(features["delta_total_power"], 25.0)
        self.assertNotIn("delta_total_power", extract_features(current))

    def test_missing_optional_values_do_not_crash_scoring(self):
        baseline = build_baseline([self.reading(100.0) for _ in range(30)])
        measurement = {"total_power": 100.0, "l1": {}}

        result = score_measurement(measurement, baseline)

        self.assertTrue(result["scorable"])
        self.assertIn("total_power", result["feature_scores"])

    def test_features_with_fewer_than_thirty_samples_are_excluded(self):
        measurements = [self.reading(100.0) for _ in range(30)]
        measurements[0]["frequency_hz"] = None
        for measurement in measurements[1:]:
            measurement["frequency_hz"] = None
        measurements[0]["frequency_hz"] = 50.0

        baseline = build_baseline(measurements)

        self.assertNotIn("frequency_hz", baseline)

    def test_mad_zero_uses_standard_deviation_fallback(self):
        measurements = [self.reading(100.0) for _ in range(29)] + [self.reading(101.0)]

        baseline = build_baseline(measurements)

        self.assertEqual(baseline["total_power"]["mad"], 0.0)
        self.assertGreater(baseline["total_power"]["scale"], 0.0)

    def test_constant_baseline_identical_value_scores_zero(self):
        baseline = build_baseline([self.reading(100.0) for _ in range(30)])

        result = score_measurement(self.reading(100.0), baseline)

        self.assertEqual(result["overall_score"], 0.0)

    def test_constant_baseline_different_value_scores_high(self):
        baseline = build_baseline([self.reading(100.0) for _ in range(30)])

        result = score_measurement(self.reading(101.0), baseline)

        self.assertGreater(result["overall_score"], 3.5)
        self.assertTrue(result["is_anomaly"])

    def test_strongest_contributing_feature_is_reported(self):
        baseline = build_baseline([self.reading(100.0) for _ in range(30)])
        measurement = self.reading(100.0)
        measurement["l1"]["thd_current_pct"] = 50.0

        result = score_measurement(measurement, baseline)

        self.assertEqual(result["strongest_feature"], "l1.thd_current_pct")
        self.assertEqual(
            result["overall_score"], result["feature_scores"]["l1.thd_current_pct"]
        )

    def test_sqlite_sample_can_be_scored_after_fixed_baseline_creation(self):
        baseline_measurements = [self.reading(100.0) for _ in range(30)]
        baseline = build_baseline(baseline_measurements)
        later_measurements = baseline_measurements + [self.reading(250.0)]

        scored = score_history(later_measurements, baseline=baseline)

        self.assertTrue(scored[-1]["is_anomaly"])
        self.assertEqual(len(scored), 31)


if __name__ == "__main__":
    unittest.main()
