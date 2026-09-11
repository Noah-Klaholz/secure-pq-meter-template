#!/usr/bin/env python3
"""Load and normalize measurement history from JSONL and SQLite."""

from __future__ import annotations

import argparse
import json
import logging
import sqlite3
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable

logger = logging.getLogger(__name__)

MIN_VALID_SAMPLES = 30
DEFAULT_THRESHOLD = 3.5
PRIMARY_FEATURES = (
    "total_power",
    "delta_total_power",
    "l1.current_a",
    "l1.thd_current_pct",
    "frequency_hz",
)
OPTIONAL_FEATURES = (
    "l1.reactive_power_var",
    "l1.cos_phi",
    "l1.voltage_v",
)
FEATURES = PRIMARY_FEATURES + OPTIONAL_FEATURES


def _timestamp(value: Any) -> datetime | None:
    if not isinstance(value, str):
        return None
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    return parsed.astimezone(timezone.utc)


def _number(value: Any) -> float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return float(value)


def _phase(reading: dict[str, Any]) -> dict[str, float | None]:
    l1 = reading.get("l1")
    if not isinstance(l1, dict):
        l1 = {}
    return {
        "current_a": _number(l1.get("current_a")),
        "voltage_v": _number(l1.get("voltage_v")),
        "reactive_power_var": _number(l1.get("reactive_power_var")),
        "cos_phi": _number(l1.get("cos_phi")),
        "thd_current_pct": _number(l1.get("thd_current_pct")),
    }


def normalize_measurement(
    reading: dict[str, Any],
    timestamp: datetime,
    source: str,
    fallback_total_power: Any = None,
    fallback_heartbeat: Any = False,
) -> dict[str, Any] | None:
    """Convert one stored reading to the common analysis representation."""
    total_power = _number(reading.get("total_power"))
    if total_power is None:
        total_power = _number(fallback_total_power)
    if total_power is None:
        return None
    heartbeat = reading.get("heartbeat", fallback_heartbeat)
    if isinstance(heartbeat, int) and heartbeat in (0, 1):
        heartbeat = bool(heartbeat)
    return {
        "timestamp": timestamp,
        "source": source,
        "total_power": total_power,
        "frequency_hz": _number(reading.get("frequency_hz")),
        "heartbeat": heartbeat if isinstance(heartbeat, bool) else False,
        "l1": _phase(reading),
    }


def _dedup_key(measurement: dict[str, Any]) -> tuple[datetime, float]:
    return measurement["timestamp"], measurement["total_power"]


def combine_measurements(*groups: Iterable[dict[str, Any]]) -> list[dict[str, Any]]:
    """Deduplicate by canonical timestamp and power, preferring JSONL history."""
    combined: dict[tuple[datetime, float], dict[str, Any]] = {}
    for group in groups:
        for measurement in group:
            key = _dedup_key(measurement)
            combined.setdefault(key, measurement)
    return sorted(combined.values(), key=lambda item: item["timestamp"])


def load_jsonl_history(path: str | Path) -> list[dict[str, Any]]:
    """Load valid readings from legacy JSONL batches, skipping malformed records."""
    measurements: list[dict[str, Any]] = []
    with Path(path).open("r", encoding="utf-8") as archive:
        for line_number, line in enumerate(archive, start=1):
            if not line.strip():
                continue
            try:
                batch = json.loads(line)
                received_at = _timestamp(batch.get("received_at"))
                readings = batch.get("readings")
                if received_at is None or not isinstance(readings, list):
                    raise ValueError("missing timestamp or readings array")
                for reading in readings:
                    if not isinstance(reading, dict):
                        logger.warning("Skipping JSONL line %d: reading is not an object", line_number)
                        continue
                    normalized = normalize_measurement(reading, received_at, "jsonl")
                    if normalized is None:
                        logger.warning("Skipping JSONL line %d: reading has no total_power", line_number)
                        continue
                    measurements.append(normalized)
            except (OSError, json.JSONDecodeError, TypeError, ValueError) as error:
                logger.warning("Skipping malformed JSONL line %d: %s", line_number, error)
    return sorted(measurements, key=lambda item: item["timestamp"])


def load_sqlite_history(path: str | Path) -> list[dict[str, Any]]:
    """Load valid readings from SQLite without depending on SQLite row IDs."""
    measurements: list[dict[str, Any]] = []
    with sqlite3.connect(path) as database:
        rows = database.execute(
            "SELECT received_at, total_power, heartbeat, reading_json "
            "FROM measurements ORDER BY received_at_epoch ASC, id ASC"
        )
        for row_number, (received_at, total_power, heartbeat, raw_reading) in enumerate(rows, start=1):
            timestamp = _timestamp(received_at)
            if timestamp is None:
                logger.warning("Skipping SQLite row %d: invalid received_at", row_number)
                continue
            try:
                reading = json.loads(raw_reading)
            except (TypeError, json.JSONDecodeError):
                logger.warning("Skipping SQLite row %d: malformed reading_json", row_number)
                continue
            if not isinstance(reading, dict):
                logger.warning("Skipping SQLite row %d: reading_json is not an object", row_number)
                continue
            normalized = normalize_measurement(
                reading,
                timestamp,
                "sqlite",
                fallback_total_power=total_power,
                fallback_heartbeat=heartbeat,
            )
            if normalized is None:
                logger.warning("Skipping SQLite row %d: reading has no total_power", row_number)
                continue
            measurements.append(normalized)
    return sorted(measurements, key=lambda item: item["timestamp"])


def load_combined_history(
    jsonl_path: str | Path, sqlite_path: str | Path
) -> list[dict[str, Any]]:
    """Load both sources, preserving JSONL precedence for duplicate records."""
    return combine_measurements(load_jsonl_history(jsonl_path), load_sqlite_history(sqlite_path))


def extract_features(
    measurement: dict[str, Any], previous: dict[str, Any] | None = None
) -> dict[str, float]:
    """Extract only valid physical features from one normalized measurement."""
    features: dict[str, float] = {}
    total_power = _number(measurement.get("total_power"))
    if total_power is not None:
        features["total_power"] = total_power
    if previous is not None:
        previous_power = _number(previous.get("total_power"))
        if total_power is not None and previous_power is not None:
            features["delta_total_power"] = total_power - previous_power

    for name in ("frequency_hz",):
        value = _number(measurement.get(name))
        if value is not None:
            features[name] = value
    l1 = measurement.get("l1")
    if isinstance(l1, dict):
        for name in (
            "current_a",
            "thd_current_pct",
            "reactive_power_var",
            "cos_phi",
            "voltage_v",
        ):
            value = _number(l1.get(name))
            if value is not None:
                features[f"l1.{name}"] = value
    return features


def _median(values: list[float]) -> float:
    ordered = sorted(values)
    middle = len(ordered) // 2
    if len(ordered) % 2:
        return ordered[middle]
    return (ordered[middle - 1] + ordered[middle]) / 2.0


def build_baseline(
    measurements: Iterable[dict[str, Any]],
    min_valid_samples: int = MIN_VALID_SAMPLES,
) -> dict[str, dict[str, float | int]]:
    """Build immutable median/MAD statistics for features with enough samples."""
    values: dict[str, list[float]] = {feature: [] for feature in FEATURES}
    previous = None
    for measurement in measurements:
        extracted = extract_features(measurement, previous)
        for feature, value in extracted.items():
            values[feature].append(value)
        previous = measurement

    baseline: dict[str, dict[str, float | int]] = {}
    for feature, feature_values in values.items():
        if len(feature_values) < min_valid_samples:
            continue
        median = _median(feature_values)
        mad = _median([abs(value - median) for value in feature_values])
        if mad == 0.0:
            variance = sum((value - median) ** 2 for value in feature_values) / len(feature_values)
            scale = variance**0.5
        else:
            scale = mad
        baseline[feature] = {
            "median": median,
            "mad": mad,
            "scale": scale,
            "count": len(feature_values),
        }
    return baseline


def _robust_score(value: float, statistics: dict[str, float | int]) -> float:
    median = float(statistics["median"])
    difference = abs(value - median)
    if difference == 0.0:
        return 0.0
    scale = float(statistics["scale"])
    if scale == 0.0:
        scale = 1e-12
    return 0.6745 * difference / scale


def score_measurement(
    measurement: dict[str, Any],
    baseline: dict[str, dict[str, float | int]],
    previous: dict[str, Any] | None = None,
    threshold: float = DEFAULT_THRESHOLD,
) -> dict[str, Any]:
    """Score one measurement without changing the supplied baseline."""
    feature_values = extract_features(measurement, previous)
    feature_scores = {
        feature: _robust_score(feature_values[feature], baseline[feature])
        for feature in FEATURES
        if feature in feature_values and feature in baseline
    }
    if not feature_scores:
        return {
            "overall_score": None,
            "is_anomaly": False,
            "strongest_feature": None,
            "feature_scores": {},
            "scorable": False,
            "reason": "no feature has both a valid value and a baseline",
        }
    strongest_feature = max(feature_scores, key=feature_scores.get)
    overall_score = feature_scores[strongest_feature]
    return {
        "overall_score": overall_score,
        "is_anomaly": overall_score >= threshold,
        "strongest_feature": strongest_feature,
        "feature_scores": feature_scores,
        "scorable": True,
        "reason": None,
    }


def score_history(
    measurements: list[dict[str, Any]],
    baseline: dict[str, dict[str, float | int]] | None = None,
    threshold: float = DEFAULT_THRESHOLD,
) -> list[dict[str, Any]]:
    """Score chronologically ordered measurements against a fixed baseline."""
    statistics = baseline if baseline is not None else build_baseline(measurements)
    scored: list[dict[str, Any]] = []
    previous = None
    for measurement in measurements:
        result = score_measurement(measurement, statistics, previous, threshold)
        scored.append({"measurement": measurement, **result})
        previous = measurement
    return scored


def main() -> None:
    parser = argparse.ArgumentParser(description="Load and inspect normalized measurement history")
    parser.add_argument("--jsonl", default="measurement-history.jsonl")
    parser.add_argument("--db", default="data/history.db")
    args = parser.parse_args()
    measurements = load_combined_history(args.jsonl, args.db)
    print(f"Loaded {len(measurements)} combined measurements")


if __name__ == "__main__":
    main()
