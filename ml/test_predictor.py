import datetime
import json
import tempfile
import unittest
from pathlib import Path

from predictor import OnlineLoadForecaster, warm_start_from_history


class TestOnlineLoadForecaster(unittest.TestCase):
    def setUp(self):
        self.forecaster = OnlineLoadForecaster(horizon_seconds=15.0, step_seconds=3.0)

    def test_init_defaults(self):
        self.assertEqual(self.forecaster.horizon_seconds, 15.0)
        self.assertEqual(self.forecaster.step_seconds, 3.0)
        self.assertEqual(self.forecaster.samples_learned, 0)
        self.assertEqual(self.forecaster.drifts_detected, 0)
        self.assertIsNone(self.forecaster.last_settled_power)

    def test_extract_features_complete_reading(self):
        reading = {
            "total_power_watts": 120.0,
            "frequency_hz": 50.02,
            "voltage_v": [231.5, 0.0, 0.0],
            "current_a": [0.52, 0.0, 0.0],
            "thd_current_pct": [3.2, None, None],
        }
        prev_reading = {
            "total_power_watts": 100.0,
        }
        features = self.forecaster.extract_features(reading, prev_reading)

        self.assertEqual(features["power_watts"], 120.0)
        self.assertAlmostEqual(features["delta_power"], 20.0)
        self.assertEqual(features["frequency_hz"], 50.02)
        self.assertEqual(features["voltage_v1"], 231.5)
        self.assertEqual(features["current_a1"], 0.52)
        self.assertEqual(features["thd_current_pct1"], 3.2)
        self.assertEqual(features["rolling_mean"], 120.0)
        self.assertEqual(features["rolling_std"], 0.0)

    def test_extract_features_with_missing_and_null_context(self):
        reading = {
            "total_power_watts": 85.0,
            "frequency_hz": None,
            "voltage_v": [None],
            "current_a": [],
            "thd_current_pct": None,
        }
        features = self.forecaster.extract_features(reading, None)

        self.assertEqual(features["power_watts"], 85.0)
        self.assertEqual(features["delta_power"], 0.0)
        self.assertEqual(features["frequency_hz"], 50.0)
        self.assertEqual(features["voltage_v1"], 230.0)
        self.assertEqual(features["current_a1"], 0.0)
        self.assertEqual(features["thd_current_pct1"], 0.0)

    def test_forecast_projection_structure(self):
        t0 = 1700000000.0
        reading = {
            "total_power_watts": 200.0,
            "frequency_hz": 50.0,
            "voltage_v": [230.0],
            "current_a": [0.87],
            "thd_current_pct": [1.5],
        }
        result = self.forecaster.forecast(reading, t0)

        self.assertIn("generated_at", result)
        self.assertEqual(result["horizon_seconds"], 15.0)
        self.assertIn("points", result)
        # 15s horizon / 3s step = 5 points
        self.assertEqual(len(result["points"]), 5)

        p1 = result["points"][0]
        self.assertIn("at", p1)
        self.assertIn("predicted_watts", p1)
        self.assertIn("lower_bound_watts", p1)
        self.assertIn("upper_bound_watts", p1)
        self.assertTrue(p1["lower_bound_watts"] <= p1["predicted_watts"] <= p1["upper_bound_watts"])

    def test_forecast_preserves_negative_export_power(self):
        t0 = 1700000000.0
        # A negative total power represents a decentralized grid site exporting solar
        reading = {
            "total_power_watts": -1500.0,
            "frequency_hz": 50.01,
            "voltage_v": [235.0],
            "current_a": [6.4],
            "thd_current_pct": [2.1],
        }
        result = self.forecaster.forecast(reading, t0)
        last_point = result["points"][-1]

        self.assertLess(last_point["predicted_watts"], 0.0)
        self.assertLess(last_point["lower_bound_watts"], 0.0)

    def test_online_learning_and_drift_detection(self):
        t0 = 1700000000.0
        reading_baseline = {"total_power_watts": 100.0}
        self.forecaster.forecast(reading_baseline, t0)

        # Baseline reading
        self.forecaster.update_with_actual(t0, 100.0)
        self.assertEqual(self.forecaster.drifts_detected, 0)

        # Advance time by 15s to match pending feedback
        t1 = t0 + 15.0
        # Appliance turned on: jump from 100W to 180W (+80W > 15W threshold)
        self.forecaster.update_with_actual(t1, 180.0)
        self.assertEqual(self.forecaster.samples_learned, 1)
        self.assertGreater(self.forecaster.drifts_detected, 0)
        self.assertIsNotNone(self.forecaster.mae.get())

    def test_warm_start_from_history(self):
        with tempfile.NamedTemporaryFile("w+", suffix=".jsonl", delete=False) as f:
            temp_path = f.name
            batch = {
                "schema_version": 1,
                "received_at": "2026-09-11T10:00:00Z",
                "readings": [
                    {
                        "total_power": 110.0,
                        "frequency_hz": 50.0,
                        "l1": {"voltage_v": 230.0, "current_a": 0.48, "thd_current_pct": 1.2},
                    },
                    {
                        "total_power": 112.0,
                        "frequency_hz": 49.99,
                        "l1": {"voltage_v": 230.1, "current_a": 0.49, "thd_current_pct": 1.3},
                    },
                ],
            }
            f.write(json.dumps(batch) + "\n")

        try:
            warm_start_from_history(self.forecaster, temp_path)
            self.assertGreater(self.forecaster.samples_learned, 0)
        finally:
            Path(temp_path).unlink(missing_ok=True)

    def test_warm_start_nonexistent_file(self):
        # Should not raise exception
        warm_start_from_history(self.forecaster, "/nonexistent/path/to/history.jsonl")
        self.assertEqual(self.forecaster.samples_learned, 0)


if __name__ == "__main__":
    unittest.main()
