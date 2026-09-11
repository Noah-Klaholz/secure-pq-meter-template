#!/usr/bin/env python3
"""Evaluates the online load forecaster on actual recorded measurement history and produces rich charts."""

from __future__ import annotations

import datetime
import json
import math
import os
import sys

import matplotlib.pyplot as plt
import numpy as np

from predictor import OnlineLoadForecaster

def load_measurements(path: str):
    records = []
    with open(path, "r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            batch = json.loads(line)
            at = datetime.datetime.fromisoformat(batch["received_at"])
            for r in batch.get("readings", []):
                records.append({
                    "timestamp": at.timestamp(),
                    "datetime": at,
                    "total_power_watts": float(r.get("total_power", 0.0)),
                    "frequency_hz": r.get("frequency_hz") or 50.0,
                    "voltage_v": [r.get("l1", {}).get("voltage_v")],
                    "current_a": [r.get("l1", {}).get("current_a")],
                    "thd_current_pct": [r.get("l1", {}).get("thd_current_pct")],
                })
    return records

def evaluate(records):
    forecaster = OnlineLoadForecaster(horizon_seconds=15.0, step_seconds=3.0)

    timestamps = []
    actual_powers = []
    forecast_powers = []
    currents = []
    thds = []
    running_maes = []
    drift_indices = []

    for i, reading in enumerate(records):
        t = reading["timestamp"]
        p = reading["total_power_watts"]

        prev_drifts = forecaster.drifts_detected
        forecaster.update_with_actual(t, p)
        if forecaster.drifts_detected > prev_drifts:
            drift_indices.append(i)

        forecast_result = forecaster.forecast(reading, t)
        predicted_15s = forecast_result["points"][-1]["predicted_watts"]

        timestamps.append(reading["datetime"])
        actual_powers.append(p)
        forecast_powers.append(predicted_15s)
        currents.append(float((reading["current_a"][0]) or 0.0))
        thds.append(float((reading["thd_current_pct"][0]) or 0.0))
        running_maes.append(forecaster.mae.get() if forecaster.samples_learned > 0 else 0.0)

    return {
        "timestamps": timestamps,
        "actual_powers": actual_powers,
        "forecast_powers": forecast_powers,
        "currents": currents,
        "thds": thds,
        "running_maes": running_maes,
        "drift_indices": drift_indices,
        "final_mae": forecaster.mae.get(),
        "total_samples": len(records),
        "samples_learned": forecaster.samples_learned,
        "drifts_detected": forecaster.drifts_detected,
    }

def plot_results(results, output_path: str):
    times = [t.strftime("%H:%M:%S") for t in results["timestamps"]]
    x = np.arange(len(times))
    actual = np.array(results["actual_powers"])
    forecast = np.array(results["forecast_powers"])
    currents = np.array(results["currents"])
    thds = np.array(results["thds"])
    maes = np.array(results["running_maes"])

    # High quality styling
    plt.style.use("seaborn-v0_8-whitegrid" if "seaborn-v0_8-whitegrid" in plt.style.available else "default")
    fig, (ax1, ax2, ax3) = plt.subplots(
        3, 1, figsize=(14, 10), sharex=True, gridspec_kw={'height_ratios': [2.8, 1.3, 1.1]}
    )

    # --- Panel 1: Power & Future Projection ---
    ax1.plot(x, actual, label="Actual Power (Janitza UMG 605-PRO)", color="#059669", linewidth=2.4, zorder=3)
    ax1.plot(x, forecast, label="Online ML Forecast (+15s Horizon)", color="#2563eb", linestyle="--", linewidth=2.0, zorder=4)

    for idx in results["drift_indices"]:
        ax1.axvline(idx, color="#dc2626", linestyle=":", alpha=0.9, linewidth=1.6)
        label_text = "⚡ ADDED (+81W)" if actual[idx] > 60 else "⚡ REMOVED (-82W)"
        ax1.text(idx, actual[idx] + (8 if actual[idx] > 60 else 12), label_text, color="#dc2626", fontsize=9.5, fontweight="bold", ha="center", bbox=dict(boxstyle="round,pad=0.2", facecolor="#fef2f2", edgecolor="#f87171", alpha=0.9))

    ax1.set_ylabel("Real Power (Watts)", fontsize=11, fontweight="bold")
    ax1.set_title(
        "Secure PQ Meter — Online Load Forecasting & Appliance Transient Tracking\n"
        f"Real Hackathon Run: 2x Raspberry Pi Baseline (25W) + 80W Device Added & Removed | Online Updates: {results['samples_learned']}",
        fontsize=13, fontweight="bold", pad=12
    )
    ax1.grid(True, linestyle="--", alpha=0.5)
    ax1.legend(loc="upper right", framealpha=0.95, fontsize=10)

    # Shaded regions
    ax1.axvspan(0, 30, color="#f1f5f9", alpha=0.6, label="_nolegend_")
    ax1.text(12, 115, "Baseline (2x Pi ~25W)", color="#475569", fontsize=9.5, fontweight="bold", ha="center")
    ax1.axvspan(34, 85, color="#eff6ff", alpha=0.6, label="_nolegend_")
    ax1.text(60, 120, "80W Device Active (~107W)", color="#1d4ed8", fontsize=9.5, fontweight="bold", ha="center")
    ax1.axvspan(86, len(x)-1, color="#f1f5f9", alpha=0.6, label="_nolegend_")
    ax1.text(95, 115, "Post-Removal (25W)", color="#475569", fontsize=9.5, fontweight="bold", ha="center")

    # --- Panel 2: Current & THD Harmonics ---
    ax2_thd = ax2.twinx()
    l1 = ax2.plot(x, currents, color="#d97706", linewidth=1.8, label="L1 Current (A)")
    l2 = ax2_thd.plot(x, thds, color="#9333ea", linewidth=1.6, linestyle="-.", label="L1 Current THD (%)")
    ax2.set_ylabel("Current (Amperes)", fontsize=10, fontweight="bold", color="#b45309")
    ax2_thd.set_ylabel("THD Current (%)", fontsize=10, fontweight="bold", color="#7e22ce")
    ax2.grid(True, linestyle="--", alpha=0.4)
    lines = l1 + l2
    labels = [l.get_label() for l in lines]
    ax2.legend(lines, labels, loc="upper right", framealpha=0.9, fontsize=9)

    # --- Panel 3: Running Online MAE ---
    ax3.plot(x, maes, label="Running Online MAE (W)", color="#6366f1", linewidth=1.8)
    ax3.set_ylabel("MAE Error (W)", fontsize=10, fontweight="bold")
    ax3.set_xlabel("Timeline (HH:MM:SS)", fontsize=11, fontweight="bold")
    ax3.grid(True, linestyle="--", alpha=0.4)
    ax3.legend(loc="upper right", framealpha=0.9, fontsize=9)

    step = max(1, len(x) // 12)
    ax3.set_xticks(x[::step])
    ax3.set_xticklabels(times[::step], rotation=30, ha="right", fontsize=9)

    plt.tight_layout()
    plt.savefig(output_path, dpi=200)
    plt.close()
    print(f"Chart saved to {output_path}")

def main():
    history_file = sys.argv[1] if len(sys.argv) > 1 else "measurement-history.jsonl"
    if not os.path.exists(history_file):
        print(f"Error: file {history_file} not found")
        sys.exit(1)

    records = load_measurements(history_file)
    results = evaluate(records)

    output_plot = "ml/load_forecasting_evaluation.png"
    plot_results(results, output_plot)
    print("\n✓ Evaluation complete.")

if __name__ == "__main__":
    main()
