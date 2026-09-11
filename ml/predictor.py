#!/usr/bin/env python3
"""Online Short-Term Load & Demand Forecaster for pq-meter-server.

Uses streaming machine learning (river) to continuously learn and adapt to
household/appliance power patterns in real time without batch retraining.
"""

from __future__ import annotations

import argparse
import datetime
import json
import logging
import math
import os
import sys
import time
from collections import deque
from typing import Any, Dict, List, Optional, Tuple

import requests
from river import drift, linear_model, metrics, optim, preprocessing

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [%(levelname)s] %(message)s",
    datefmt="%H:%M:%S",
)
logger = logging.getLogger("forecaster")


class OnlineLoadForecaster:
    """Continuously learning load forecasting model with residual delta prediction and drift tracking."""

    def __init__(self, horizon_seconds: float = 15.0, step_seconds: float = 3.0):
        self.horizon_seconds = horizon_seconds
        self.step_seconds = step_seconds
        self.model_name = "River Online Residual Regressor"

        # Adaptive online regression pipeline predicting future power delta (P_future - P_current)
        self.model = preprocessing.StandardScaler() | linear_model.LinearRegression(
            optimizer=optim.Adam(lr=0.05),
            l2=1e-3,
        )

        # Continual evaluation & drift tracking
        self.mae = metrics.MAE()
        self.samples_learned = 0
        self.drifts_detected = 0

        # Step event detection threshold
        self.last_settled_power: Optional[float] = None
        self.step_threshold_watts = 15.0

        # Pending prediction queue: stores (due_timestamp, features, base_power, predicted_delta)
        self.pending_feedback: deque[Tuple[float, Dict[str, float], float, float]] = deque()

        # Cache of recent raw readings for feature engineering
        self.history_buffer: deque[Dict[str, Any]] = deque(maxlen=60)

    def extract_features(
        self, current: Dict[str, Any], previous: Optional[Dict[str, Any]] = None
    ) -> Dict[str, float]:
        """Extract multi-dimensional electrical features from recent readings."""
        power = float(current.get("total_power_watts", 0.0))
        freq = float(current.get("frequency_hz") or 50.0)

        # Phase 1 metrics (defaulting if unwired or missing)
        v1 = float((current.get("voltage_v") or [230.0])[0] or 230.0)
        i1 = float((current.get("current_a") or [0.0])[0] or 0.0)
        thd1 = float((current.get("thd_current_pct") or [0.0])[0] or 0.0)

        # Temporal delta features (momentum & rate of change)
        prev_power = float(previous.get("total_power_watts", power)) if previous else power
        delta_power = power - prev_power

        # Rolling statistics over buffer
        recent_powers = [
            float(item.get("total_power_watts", power))
            for item in self.history_buffer
            if item.get("total_power_watts") is not None
        ]
        if not recent_powers:
            recent_powers = [power]

        mean_power = sum(recent_powers) / len(recent_powers)
        variance_power = (
            sum((p - mean_power) ** 2 for p in recent_powers) / len(recent_powers)
        )
        std_power = math.sqrt(variance_power)

        return {
            "power_watts": power,
            "delta_power": delta_power,
            "rolling_mean": mean_power,
            "rolling_std": std_power,
            "frequency_hz": freq,
            "voltage_v1": v1,
            "current_a1": i1,
            "thd_current_pct1": thd1,
        }

    def update_with_actual(self, actual_timestamp: float, actual_power: float):
        """Pairs historical predictions with ground truth as time advances."""
        while self.pending_feedback and self.pending_feedback[0][0] <= actual_timestamp:
            due_at, features, base_power, predicted_delta = self.pending_feedback.popleft()
            actual_delta = actual_power - base_power

            # Online model learning step: SGD update on the residual delta!
            self.model.learn_one(features, actual_delta)

            predicted_power = base_power + predicted_delta
            self.mae.update(actual_power, predicted_power)
            self.samples_learned += 1

            # Detect step events / sudden appliance switching
            if self.last_settled_power is not None:
                step = actual_power - self.last_settled_power
                if abs(step) >= self.step_threshold_watts:
                    self.drifts_detected += 1
                    action = "Appliance ADDED" if step > 0 else "Appliance REMOVED"
                    logger.info(
                        "⚡ Step Event: %s (ΔP: %+5.1f W, New level: %.1f W). Online model adapting!",
                        action,
                        step,
                        actual_power,
                    )
                    self.last_settled_power = actual_power
            else:
                self.last_settled_power = actual_power

    def forecast(
        self, current: Dict[str, Any], current_timestamp: float
    ) -> Dict[str, Any]:
        """Generates future forecast points based on current power + learned delta."""
        prev = self.history_buffer[-1] if self.history_buffer else None
        features = self.extract_features(current, prev)
        self.history_buffer.append(current)

        current_power = float(current.get("total_power_watts", 0.0))
        predicted_delta = self.model.predict_one(features)

        # In steady state, delta is near 0; during transients delta tracks momentum
        if not math.isfinite(predicted_delta):
            predicted_delta = features["delta_power"] * 0.5
        else:
            # Dampen unconstrained extrapolation
            predicted_delta = max(-120.0, min(120.0, predicted_delta))

        # Multi-step projection into the future
        steps = max(1, int(round(self.horizon_seconds / self.step_seconds)))
        points = []
        current_dt = datetime.datetime.fromtimestamp(
            current_timestamp, tz=datetime.timezone.utc
        )
        uncertainty = max(0.5, float(self.mae.get()) if self.samples_learned > 0 else 2.0)

        for step in range(1, steps + 1):
            offset = step * self.step_seconds
            t_future = current_dt + datetime.timedelta(seconds=offset)
            alpha = min(1.0, offset / self.horizon_seconds)
            # Projected power value
            val = max(0.0, current_power + alpha * predicted_delta)

            points.append({
                "at": t_future.strftime("%Y-%m-%dT%H:%M:%SZ"),
                "predicted_watts": round(val, 2),
                "lower_bound_watts": round(max(0.0, val - 1.96 * uncertainty), 2),
                "upper_bound_watts": round(val + 1.96 * uncertainty, 2),
            })

        # Queue the final horizon prediction for feedback learning when reality catches up
        target_timestamp = current_timestamp + self.horizon_seconds
        self.pending_feedback.append((target_timestamp, features, current_power, predicted_delta))

        return {
            "generated_at": current_dt.strftime("%Y-%m-%dT%H:%M:%SZ"),
            "horizon_seconds": self.horizon_seconds,
            "model_name": f"{self.model_name} (Online Updates: {self.samples_learned})",
            "mae": round(float(self.mae.get()), 2) if self.samples_learned > 0 else None,
            "points": points,
        }


def warm_start_from_history(forecaster: OnlineLoadForecaster, history_path: str):
    """Pre-trains the online model on an existing measurement-history.jsonl file."""
    if not os.path.exists(history_path):
        return

    logger.info("Warm-starting online model from history archive: %s", history_path)
    count = 0
    with open(history_path, "r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                batch = json.loads(line)
                readings = batch.get("readings", [])
                base_time = time.time()
                for r in readings:
                    forecaster.update_with_actual(base_time, float(r.get("total_power", 0.0)))
                    forecaster.forecast(
                        {
                            "total_power_watts": r.get("total_power", 0.0),
                            "frequency_hz": r.get("frequency_hz"),
                            "voltage_v": [r.get("l1", {}).get("voltage_v")],
                            "current_a": [r.get("l1", {}).get("current_a")],
                            "thd_current_pct": [r.get("l1", {}).get("thd_current_pct")],
                        },
                        base_time,
                    )
                    count += 1
            except Exception as e:
                logger.debug("Skipping line: %s", e)
    logger.info("Warm-start finished with %d measurements. MAE: %.2f W", count, forecaster.mae.get())


def run_predictor(
    server_url: str,
    history_file: Optional[str] = None,
    poll_interval: float = 1.0,
    horizon: float = 15.0,
):
    """Main loop: polls server for latest reading, updates model, posts forecast."""
    forecaster = OnlineLoadForecaster(horizon_seconds=horizon)

    if history_file:
        warm_start_from_history(forecaster, history_file)

    history_endpoint = f"{server_url.rstrip('/')}/api/v1/history"
    forecast_endpoint = f"{server_url.rstrip('/')}/api/v1/forecast"

    logger.info("Starting Online Forecaster loop...")
    logger.info("  Polling:  %s", history_endpoint)
    logger.info("  Posting:  %s", forecast_endpoint)
    logger.info("  Horizon:  +%.0f seconds", horizon)

    last_seen_at = None

    while True:
        try:
            resp = requests.get(history_endpoint, timeout=3.0)
            if resp.status_code == 200:
                data = resp.json()
                samples = data.get("samples", [])
                if samples:
                    latest = samples[-1]
                    latest_at = latest.get("at")

                    if latest_at != last_seen_at:
                        last_seen_at = latest_at
                        now_ts = time.time()
                        latest_power = float(latest.get("total_power_watts", 0.0))

                        # 1. Provide ground truth to previous predictions
                        forecaster.update_with_actual(now_ts, latest_power)

                        # 2. Make new forward prediction
                        forecast_payload = forecaster.forecast(latest, now_ts)

                        # 3. Publish to server
                        post_resp = requests.post(
                            forecast_endpoint, json=forecast_payload, timeout=3.0
                        )

                        mae_str = f"{forecaster.mae.get():.2f} W" if forecaster.samples_learned > 0 else "warming up"
                        next_pred = forecast_payload["points"][-1]["predicted_watts"]
                        logger.info(
                            "Power: %6.1f W | Forecast(+%02ds): %6.1f W | Online MAE: %s | Updates: %d",
                            latest_power,
                            int(horizon),
                            next_pred,
                            mae_str,
                            forecaster.samples_learned,
                        )
            elif resp.status_code == 503:
                logger.debug("Meter state currently unavailable on server, waiting...")
        except requests.exceptions.ConnectionError:
            logger.warning("Could not reach %s. Retrying in %.1fs...", server_url, poll_interval * 2)
            time.sleep(poll_interval)
        except Exception as e:
            logger.error("Error during prediction cycle: %s", e)

        time.sleep(poll_interval)


def main():
    parser = argparse.ArgumentParser(description="Real-time Online Load Forecaster")
    parser.add_argument(
        "--server",
        default="http://127.0.0.1:8080",
        help="Base URL of the pq-meter-server dashboard (default: http://127.0.0.1:8080)",
    )
    parser.add_argument(
        "--history-file",
        default="measurement-history.jsonl",
        help="Path to saved history archive for warm-starting",
    )
    parser.add_argument(
        "--interval",
        type=float,
        default=1.0,
        help="Polling interval in seconds (default: 1.0)",
    )
    parser.add_argument(
        "--horizon",
        type=float,
        default=15.0,
        help="Forecasting horizon in seconds (default: 15.0)",
    )
    args = parser.parse_args()

    run_predictor(
        server_url=args.server,
        history_file=args.history_file,
        poll_interval=args.interval,
        horizon=args.horizon,
    )


if __name__ == "__main__":
    main()
