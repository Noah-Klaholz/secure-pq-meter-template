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


def main() -> None:
    parser = argparse.ArgumentParser(description="Load and inspect normalized measurement history")
    parser.add_argument("--jsonl", default="measurement-history.jsonl")
    parser.add_argument("--db", default="data/history.db")
    args = parser.parse_args()
    measurements = load_combined_history(args.jsonl, args.db)
    print(f"Loaded {len(measurements)} combined measurements")


if __name__ == "__main__":
    main()
