#!/usr/bin/env python3
"""Load and normalize measurement history from JSONL and SQLite."""

from __future__ import annotations

import argparse
import json
import logging
import sqlite3
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable
from urllib.request import Request, urlopen

logger = logging.getLogger(__name__)

MIN_VALID_SAMPLES = 30
DEFAULT_THRESHOLD = 3.5
POWER_QUALITY_FEATURES = (
    "frequency_hz",
    "l1.voltage_v",
    "l1.thd_current_pct",
    "l1.thd_voltage_pct",
)
FEATURES = POWER_QUALITY_FEATURES


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
        "thd_voltage_pct": _number(l1.get("thd_voltage_pct")),
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


def combine_measurements(
    *groups: Iterable[dict[str, Any]]
) -> list[dict[str, Any]]:
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
                        logger.warning(
                            "Skipping JSONL line %d: reading is not an object",
                            line_number,
                        )
                        continue
                    normalized = normalize_measurement(
                        reading, received_at, "jsonl"
                    )
                    if normalized is None:
                        logger.warning(
                            "Skipping JSONL line %d: reading has no total_power",
                            line_number,
                        )
                        continue
                    measurements.append(normalized)
            except (
                OSError,
                json.JSONDecodeError,
                TypeError,
                ValueError,
            ) as error:
                logger.warning(
                    "Skipping malformed JSONL line %d: %s", line_number, error
                )
    return sorted(measurements, key=lambda item: item["timestamp"])


def load_sqlite_history(path: str | Path) -> list[dict[str, Any]]:
    """Load valid readings from SQLite without depending on SQLite row IDs."""
    measurements: list[dict[str, Any]] = []
    with sqlite3.connect(path) as database:
        rows = database.execute(
            "SELECT received_at, total_power, heartbeat, reading_json "
            "FROM measurements ORDER BY received_at_epoch ASC, id ASC"
        )
        for row_number, (
            received_at,
            total_power,
            heartbeat,
            raw_reading,
        ) in enumerate(rows, start=1):
            timestamp = _timestamp(received_at)
            if timestamp is None:
                logger.warning(
                    "Skipping SQLite row %d: invalid received_at", row_number
                )
                continue
            try:
                reading = json.loads(raw_reading)
            except (TypeError, json.JSONDecodeError):
                logger.warning(
                    "Skipping SQLite row %d: malformed reading_json", row_number
                )
                continue
            if not isinstance(reading, dict):
                logger.warning(
                    "Skipping SQLite row %d: reading_json is not an object",
                    row_number,
                )
                continue
            normalized = normalize_measurement(
                reading,
                timestamp,
                "sqlite",
                fallback_total_power=total_power,
                fallback_heartbeat=heartbeat,
            )
            if normalized is None:
                logger.warning(
                    "Skipping SQLite row %d: reading has no total_power",
                    row_number,
                )
                continue
            measurements.append(normalized)
    return sorted(measurements, key=lambda item: item["timestamp"])


def load_combined_history(
    jsonl_path: str | Path, sqlite_path: str | Path
) -> list[dict[str, Any]]:
    """Load both sources, preserving JSONL precedence for duplicate records."""
    return combine_measurements(
        load_jsonl_history(jsonl_path), load_sqlite_history(sqlite_path)
    )


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
            "thd_voltage_pct",
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
            if feature in values:
                values[feature].append(value)
        previous = measurement

    baseline: dict[str, dict[str, float | int]] = {}
    for feature, feature_values in values.items():
        if len(feature_values) < min_valid_samples:
            continue
        median = _median(feature_values)
        mad = _median([abs(value - median) for value in feature_values])
        if mad == 0.0:
            variance = sum(
                (value - median) ** 2 for value in feature_values
            ) / len(feature_values)
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
    initial_previous: dict[str, Any] | None = None,
) -> list[dict[str, Any]]:
    """Score chronologically ordered measurements against a fixed baseline."""
    statistics = (
        baseline if baseline is not None else build_baseline(measurements)
    )
    scored: list[dict[str, Any]] = []
    previous = initial_previous
    for measurement in measurements:
        result = score_measurement(measurement, statistics, previous, threshold)
        scored.append({"measurement": measurement, **result})
        previous = measurement
    return scored


def _load_optional(loader: Any, path: str | Path) -> list[dict[str, Any]]:
    try:
        return loader(path)
    except (FileNotFoundError, OSError, sqlite3.Error) as error:
        logger.info("History source unavailable (%s): %s", path, error)
        return []


def analyze_sources(
    jsonl_path: str | Path,
    sqlite_path: str | Path,
    threshold: float = DEFAULT_THRESHOLD,
    baseline_prefilter_threshold: float = 6.0,
) -> dict[str, Any]:
    """Build a robust fixed baseline from both sources, then score all samples."""
    jsonl_measurements = _load_optional(load_jsonl_history, jsonl_path)
    sqlite_measurements = _load_optional(load_sqlite_history, sqlite_path)
    combined = combine_measurements(jsonl_measurements, sqlite_measurements)
    initial_baseline = build_baseline(combined)
    initial_scored = score_history(
        combined,
        baseline=initial_baseline,
        threshold=baseline_prefilter_threshold,
    )
    excluded_features: dict[str, int] = {}
    excluded_keys: set[tuple[datetime, float]] = set()
    for index, result in enumerate(initial_scored):
        if not result["scorable"]:
            continue
        previous = combined[index - 1] if index else None
        initial_values = extract_features(result["measurement"], previous)
        extreme_features = [
            feature
            for feature, score in result["feature_scores"].items()
            if score >= baseline_prefilter_threshold
            or (
                initial_baseline[feature]["mad"] == 0
                and initial_values.get(feature)
                != initial_baseline[feature]["median"]
            )
        ]
        if extreme_features:
            excluded_keys.add(_dedup_key(result["measurement"]))
            for feature in extreme_features:
                excluded_features[feature] = (
                    excluded_features.get(feature, 0) + 1
                )

    training_measurements = [
        measurement
        for measurement in combined
        if _dedup_key(measurement) not in excluded_keys
    ]
    final_baseline = build_baseline(training_measurements)
    scored = score_history(
        combined,
        baseline=final_baseline,
        threshold=threshold,
    )
    anomalies = [result for result in scored if result["is_anomaly"]]
    return {
        "jsonl_count": len(jsonl_measurements),
        "sqlite_count": len(sqlite_measurements),
        "total_count": len(combined),
        "baseline_count": len(training_measurements),
        "evaluation_count": len(combined),
        "initial_baseline_count": len(combined),
        "excluded_count": len(excluded_keys),
        "excluded_features": excluded_features,
        "baseline_prefilter_threshold": baseline_prefilter_threshold,
        "baseline": final_baseline,
        "scored": scored,
        "anomalies": anomalies,
        "scorable_count": sum(result["scorable"] for result in scored),
        "threshold": threshold,
    }


def _display_timestamp(timestamp: datetime) -> str:
    return timestamp.isoformat().replace("+00:00", "Z")


def current_result(analysis: dict[str, Any]) -> dict[str, Any] | None:
    """Expose the latest measurement's result, never an older anomaly event."""
    if not analysis["scored"]:
        return None
    latest = analysis["scored"][-1]
    if not latest["scorable"]:
        return None
    return {
        "is_anomaly": latest["is_anomaly"],
        "score": latest["overall_score"],
        "strongest_feature": latest["strongest_feature"],
        "timestamp": _display_timestamp(latest["measurement"]["timestamp"]),
    }


def publish_result(server: str, result: dict[str, Any] | None) -> None:
    """Publish the current result, or clear the dashboard when it is unscorable."""
    request = Request(
        f"{server.rstrip('/')}/api/v1/anomaly",
        data=json.dumps(result, allow_nan=False).encode("utf-8"),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urlopen(request, timeout=5) as response:
        response.read()


def format_report(analysis: dict[str, Any], limit: int = 10) -> str:
    """Format an analysis result for terminal output."""
    baseline = analysis["baseline"]
    active = [feature for feature in FEATURES if feature in baseline]
    excluded = [feature for feature in FEATURES if feature not in baseline]
    anomalies = sorted(
        analysis["anomalies"],
        key=lambda result: result["measurement"]["timestamp"],
        reverse=True,
    )[: max(0, limit)]
    lines = [
        "Anomaly Detection Summary",
        "-------------------------",
        f"JSONL samples:       {analysis['jsonl_count']}",
        f"SQLite samples:      {analysis['sqlite_count']}",
        f"Total samples:       {analysis['total_count']}",
        f"Initial baseline:    {analysis['initial_baseline_count']}",
        f"Excluded extreme:    {analysis['excluded_count']}",
        f"Final baseline:      {analysis['baseline_count']}",
        f"Evaluation samples:  {analysis['evaluation_count']}",
        f"Scorable samples:    {analysis['scorable_count']}",
        f"Anomalies detected:  {len(analysis['anomalies'])}",
        (
            f"Anomaly percentage:  {100.0 * len(analysis['anomalies']) / analysis['evaluation_count']:.2f}%"
            if analysis["evaluation_count"]
            else "Anomaly percentage:  0.00%"
        ),
        f"Threshold:           {analysis['threshold']:.2f}",
        f"Baseline prefilter:  {analysis['baseline_prefilter_threshold']:.2f}",
        "",
        "Active power-quality anomaly features:",
    ]
    lines.extend(active or ["(none)"])
    if excluded:
        lines.extend(["", "Excluded baseline features (< 30 valid samples):"])
        lines.extend(excluded)
    if analysis["excluded_features"]:
        lines.extend(["", "Features causing exclusions:"])
        lines.extend(
            f"{feature}: {count} sample(s)"
            for feature, count in sorted(analysis["excluded_features"].items())
        )
    if anomalies:
        lines.extend(["", "Latest anomalies", "----------------"])
        for result in anomalies:
            measurement = result["measurement"]
            strongest = result["strongest_feature"]
            lines.extend(
                [
                    _display_timestamp(measurement["timestamp"]),
                    f"source: {measurement['source']}",
                    f"score: {result['overall_score']:.2f}",
                    f"total_power: {measurement['total_power']:.3f} W",
                    f"strongest_feature: {strongest}",
                    f"feature_score: {result['feature_scores'][strongest]:.2f}",
                    "",
                ]
            )
    else:
        lines.extend(["", "Latest anomalies", "----------------", "None"])
    return "\n".join(lines).rstrip() + "\n"


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Analyze measurement history for anomalies"
    )
    parser.add_argument(
        "--jsonl", default="data/measurement-history_chris.jsonl"
    )
    parser.add_argument("--db", default="data/history.db")
    parser.add_argument("--threshold", type=float, default=DEFAULT_THRESHOLD)
    parser.add_argument(
        "--baseline-prefilter-threshold", type=float, default=6.0
    )
    parser.add_argument("--limit", type=int, default=10)
    parser.add_argument("--json", action="store_true", help="Print the current result as JSON")
    parser.add_argument("--server", help="Publish to this dashboard URL (e.g. http://127.0.0.1:8080)")
    parser.add_argument("--interval", type=float, help="Repeat analysis/publishing every N seconds")
    args = parser.parse_args()
    if args.interval is not None and not (0 < args.interval < float("inf")):
        parser.error("--interval must be a finite positive number")
    while True:
        try:
            analysis = analyze_sources(
                args.jsonl,
                args.db,
                threshold=args.threshold,
                baseline_prefilter_threshold=args.baseline_prefilter_threshold,
            )
            result = current_result(analysis)
            if args.server:
                publish_result(args.server, result)
            if args.json or args.server:
                print(json.dumps(result, allow_nan=False), flush=True)
            elif analysis["total_count"] == 0:
                print("No usable measurements found in the requested history sources.")
            else:
                print(format_report(analysis, limit=args.limit), end="")
        except (OSError, ValueError) as error:
            if args.interval is None:
                raise
            logger.warning("Anomaly update failed; retrying: %s", error)
        if args.interval is None:
            return
        time.sleep(args.interval)


if __name__ == "__main__":
    main()
