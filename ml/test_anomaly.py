import json
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path

from anomaly import (
    analyze_sources,
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
                "thd_voltage_pct": 1.5,
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

    def write_jsonl_measurements(self, readings):
        lines = []
        for index, reading in enumerate(readings):
            lines.append(
                json.dumps(
                    {
                        "schema_version": 1,
                        "received_at": f"2026-09-11T10:{index // 60:02d}:{index % 60:02d}Z",
                        "readings": [reading],
                    }
                )
            )
        self.jsonl_path.write_text("\n".join(lines) + "\n")

    def run_cli(self, *arguments):
        return subprocess.run(
            [sys.executable, "anomaly.py", *arguments],
            cwd=Path(__file__).parent,
            capture_output=True,
            text=True,
            check=False,
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
        self.assertEqual(measurement["l1"]["thd_voltage_pct"], 1.5)

    def test_sqlite_loading_reads_reading_json_and_dedicated_fallbacks(self):
        timestamp = "2026-09-11T10:00:00+00:00"
        reading = self.reading(123.0)
        reading.pop("heartbeat")
        self.create_database([(timestamp, 0, 123.0, 1, json.dumps(reading))])

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

        combined = combine_measurements(
            [jsonl_measurement], [sqlite_measurement]
        )

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
            {"frequency_hz": value, "l1": {}}
            for value in [1.0, 2.0, 3.0, 4.0, 100.0]
        ]

        baseline = build_baseline(measurements, min_valid_samples=1)

        self.assertEqual(baseline["frequency_hz"]["median"], 3.0)
        self.assertEqual(baseline["frequency_hz"]["mad"], 1.0)

    def test_stable_baseline_produces_low_scores(self):
        measurements = [self.reading(100.0) for _ in range(30)]
        baseline = build_baseline(measurements)

        result = score_measurement(self.reading(100.0), baseline)

        self.assertTrue(result["scorable"])
        self.assertEqual(result["overall_score"], 0.0)
        self.assertFalse(result["is_anomaly"])

    def test_large_total_power_change_alone_is_not_an_anomaly(self):
        baseline = build_baseline([self.reading(100.0) for _ in range(30)])

        result = score_measurement(self.reading(250.0), baseline)

        self.assertFalse(result["is_anomaly"])
        self.assertNotIn("total_power", result["feature_scores"])

    def test_large_current_change_alone_is_not_an_anomaly(self):
        baseline = build_baseline([self.reading(100.0) for _ in range(30)])
        measurement = self.reading(100.0)
        measurement["l1"]["current_a"] = 100.0

        result = score_measurement(measurement, baseline)

        self.assertFalse(result["is_anomaly"])
        self.assertNotIn("l1.current_a", result["feature_scores"])

    def test_abnormal_frequency_is_detected(self):
        baseline = build_baseline([self.reading(100.0) for _ in range(30)])
        measurement = self.reading(100.0)
        measurement["frequency_hz"] = 55.0

        result = score_measurement(measurement, baseline)

        self.assertTrue(result["is_anomaly"])
        self.assertEqual(result["strongest_feature"], "frequency_hz")

    def test_abnormal_voltage_is_detected(self):
        baseline = build_baseline([self.reading(100.0) for _ in range(30)])
        measurement = self.reading(100.0)
        measurement["l1"]["voltage_v"] = 300.0

        result = score_measurement(measurement, baseline)

        self.assertTrue(result["is_anomaly"])
        self.assertEqual(result["strongest_feature"], "l1.voltage_v")

    def test_strong_thd_deviation_is_detected(self):
        baseline = build_baseline([self.reading(100.0) for _ in range(30)])
        anomalous = self.reading(100.0)
        anomalous["l1"]["thd_current_pct"] = 100.0

        result = score_measurement(anomalous, baseline)

        self.assertTrue(result["is_anomaly"])
        self.assertEqual(result["strongest_feature"], "l1.thd_current_pct")

    def test_abnormal_thd_voltage_is_detected_when_available(self):
        baseline = build_baseline([self.reading(100.0) for _ in range(30)])
        measurement = self.reading(100.0)
        measurement["l1"]["thd_voltage_pct"] = 50.0

        result = score_measurement(measurement, baseline)

        self.assertTrue(result["is_anomaly"])
        self.assertEqual(result["strongest_feature"], "l1.thd_voltage_pct")

    def test_delta_total_power_is_calculated(self):
        previous = self.reading(100.0)
        current = self.reading(125.0)

        features = extract_features(current, previous)

        self.assertEqual(features["delta_total_power"], 25.0)
        self.assertNotIn("delta_total_power", extract_features(current))

    def test_missing_optional_values_do_not_crash_scoring(self):
        baseline = build_baseline([self.reading(100.0) for _ in range(30)])
        measurement = {"total_power": 100.0, "frequency_hz": 50.0, "l1": {}}

        result = score_measurement(measurement, baseline)

        self.assertTrue(result["scorable"])
        self.assertIn("frequency_hz", result["feature_scores"])

    def test_features_with_fewer_than_thirty_samples_are_excluded(self):
        measurements = [self.reading(100.0) for _ in range(30)]
        measurements[0]["frequency_hz"] = None
        for measurement in measurements[1:]:
            measurement["frequency_hz"] = None
        measurements[0]["frequency_hz"] = 50.0

        baseline = build_baseline(measurements)

        self.assertNotIn("frequency_hz", baseline)

    def test_mad_zero_uses_standard_deviation_fallback(self):
        measurements = [self.reading(100.0) for _ in range(29)] + [
            self.reading(101.0)
        ]
        measurements[-1]["frequency_hz"] = 51.0

        baseline = build_baseline(measurements)

        self.assertEqual(baseline["frequency_hz"]["mad"], 0.0)
        self.assertGreater(baseline["frequency_hz"]["scale"], 0.0)

    def test_constant_baseline_identical_value_scores_zero(self):
        baseline = build_baseline([self.reading(100.0) for _ in range(30)])

        result = score_measurement(self.reading(100.0), baseline)

        self.assertEqual(result["overall_score"], 0.0)

    def test_constant_baseline_different_value_scores_high(self):
        baseline = build_baseline([self.reading(100.0) for _ in range(30)])
        measurement = self.reading(100.0)
        measurement["frequency_hz"] = 51.0

        result = score_measurement(measurement, baseline)

        self.assertGreater(result["overall_score"], 3.5)
        self.assertTrue(result["is_anomaly"])

    def test_strongest_contributing_feature_is_reported(self):
        baseline = build_baseline([self.reading(100.0) for _ in range(30)])
        measurement = self.reading(100.0)
        measurement["l1"]["thd_current_pct"] = 50.0

        result = score_measurement(measurement, baseline)

        self.assertEqual(result["strongest_feature"], "l1.thd_current_pct")
        self.assertEqual(
            result["overall_score"],
            result["feature_scores"]["l1.thd_current_pct"],
        )

    def test_sqlite_sample_can_be_scored_after_fixed_baseline_creation(self):
        baseline_measurements = [self.reading(100.0) for _ in range(30)]
        baseline = build_baseline(baseline_measurements)
        later_measurements = baseline_measurements + [self.reading(250.0)]

        scored = score_history(later_measurements, baseline=baseline)

        self.assertFalse(scored[-1]["is_anomaly"])
        self.assertEqual(len(scored), 31)

    def test_cli_jsonl_and_sqlite_together(self):
        self.write_jsonl_measurements([self.reading(100.0) for _ in range(30)])
        self.create_database(
            [
                (
                    "2026-09-11T11:00:00+00:00",
                    0,
                    1000.0,
                    0,
                    json.dumps(self.reading(1000.0)),
                )
            ]
        )

        result = self.run_cli(
            "--jsonl", str(self.jsonl_path), "--db", str(self.database_path)
        )

        self.assertEqual(result.returncode, 0)
        self.assertIn("JSONL samples:", result.stdout)
        self.assertIn("SQLite samples:", result.stdout)
        self.assertIn("Final baseline:", result.stdout)
        self.assertIn("Evaluation samples:", result.stdout)

    def test_cli_jsonl_only(self):
        self.write_jsonl_measurements([self.reading(100.0) for _ in range(30)])

        result = self.run_cli(
            "--jsonl",
            str(self.jsonl_path),
            "--db",
            str(self.root / "missing.db"),
        )

        self.assertEqual(result.returncode, 0)
        self.assertIn("JSONL samples:       30", result.stdout)
        self.assertIn("SQLite samples:      0", result.stdout)

    def test_cli_sqlite_only(self):
        self.create_database(
            [
                (
                    f"2026-09-11T10:00:{index:02d}+00:00",
                    index,
                    100.0,
                    0,
                    json.dumps(self.reading()),
                )
                for index in range(30)
            ]
        )

        result = self.run_cli(
            "--jsonl",
            str(self.root / "missing.jsonl"),
            "--db",
            str(self.database_path),
        )

        self.assertEqual(result.returncode, 0)
        self.assertIn("JSONL samples:       0", result.stdout)
        self.assertIn("SQLite samples:      30", result.stdout)

    def test_cli_missing_sources_are_graceful(self):
        result = self.run_cli(
            "--jsonl",
            str(self.root / "missing.jsonl"),
            "--db",
            str(self.root / "missing.db"),
        )

        self.assertEqual(result.returncode, 0)
        self.assertIn("No usable measurements found", result.stdout)
        self.assertNotIn("Traceback", result.stderr)

    def test_cli_custom_threshold_and_result_limit(self):
        self.write_jsonl_measurements(
            [self.reading(100.0) for _ in range(30)] + [self.reading(1000.0)]
        )
        anomalous = self.reading(1000.0)
        anomalous["l1"]["voltage_v"] = 500.0
        self.create_database(
            [("2026-09-11T11:00:00+00:00", 0, 1000.0, 0, json.dumps(anomalous))]
        )

        result = self.run_cli(
            "--jsonl",
            str(self.jsonl_path),
            "--db",
            str(self.database_path),
            "--threshold",
            "0.1",
            "--limit",
            "1",
        )

        self.assertEqual(result.returncode, 0)
        self.assertIn("Threshold:           0.10", result.stdout)
        self.assertEqual(result.stdout.count("strongest_feature:"), 1)

    def test_cli_anomalies_are_newest_first(self):
        self.write_jsonl_measurements([self.reading(100.0) for _ in range(30)])
        first = self.reading(1000.0)
        first["l1"]["voltage_v"] = 500.0
        second = self.reading(1100.0)
        second["l1"]["voltage_v"] = 600.0
        self.create_database(
            [
                ("2026-09-11T11:00:00+00:00", 0, 1000.0, 0, json.dumps(first)),
                (
                    "2026-09-11T11:01:00+00:00",
                    60,
                    1100.0,
                    0,
                    json.dumps(second),
                ),
            ]
        )

        result = self.run_cli(
            "--jsonl",
            str(self.jsonl_path),
            "--db",
            str(self.database_path),
            "--threshold",
            "0.1",
            "--limit",
            "10",
        )

        self.assertEqual(result.returncode, 0)
        self.assertLess(
            result.stdout.index("2026-09-11T11:01:00Z"),
            result.stdout.index("2026-09-11T11:00:00Z"),
        )

    def test_mixed_sources_build_one_final_baseline(self):
        jsonl_measurements = [self.reading(100.0) for _ in range(30)]
        self.write_jsonl_measurements(jsonl_measurements)
        self.create_database(
            [
                (
                    f"2026-09-11T11:00:{index:02d}+00:00",
                    index,
                    200.0,
                    0,
                    json.dumps(self.reading(200.0)),
                )
                for index in range(30)
            ]
        )

        analysis = analyze_sources(
            self.jsonl_path,
            self.database_path,
            baseline_prefilter_threshold=6.0,
        )

        self.assertEqual(analysis["total_count"], 60)
        self.assertGreaterEqual(analysis["baseline_count"], 30)
        self.assertIn("frequency_hz", analysis["baseline"])

    def test_extreme_samples_are_excluded_and_features_are_reported(self):
        self.write_jsonl_measurements([self.reading(100.0) for _ in range(30)])
        self.create_database(
            [
                (
                    "2026-09-11T11:00:00+00:00",
                    0,
                    10000.0,
                    0,
                    json.dumps(
                        {
                            **self.reading(10000.0),
                            "l1": {
                                **self.reading(10000.0)["l1"],
                                "voltage_v": 500.0,
                            },
                        }
                    ),
                )
            ]
        )

        analysis = analyze_sources(self.jsonl_path, self.database_path)

        self.assertGreater(analysis["excluded_count"], 0)
        self.assertIn("l1.voltage_v", analysis["excluded_features"])
        self.assertLess(analysis["baseline_count"], analysis["total_count"])

    def test_source_identity_does_not_affect_score(self):
        baseline = build_baseline([self.reading(100.0) for _ in range(30)])
        jsonl_sample = self.reading(100.0)
        sqlite_sample = dict(jsonl_sample)
        sqlite_sample["source"] = "sqlite"

        jsonl_score = score_measurement(jsonl_sample, baseline)
        sqlite_score = score_measurement(sqlite_sample, baseline)

        self.assertEqual(
            jsonl_score["overall_score"], sqlite_score["overall_score"]
        )

    def test_stable_mixed_operating_states_have_low_final_anomaly_rate(self):
        first_state = [self.reading(100.0) for _ in range(30)]
        second_state = [self.reading(200.0) for _ in range(30)]
        combined = first_state + second_state
        baseline = build_baseline(combined)

        scored = score_history(combined, baseline=baseline)

        anomaly_rate = sum(result["is_anomaly"] for result in scored) / len(
            scored
        )
        self.assertLess(anomaly_rate, 0.1)


if __name__ == "__main__":
    unittest.main()
