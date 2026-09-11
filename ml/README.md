# Real-Time Short-Term Load / Demand Forecasting (Online Machine Learning)

This module provides **real-time, adaptive online machine learning** for the Power Quality Meter.

Instead of traditional batch training that freezes model parameters, this forecaster continuously learns from incoming streaming readings using the [`river`](https://riverml.xyz/) online ML library.

---

## ⚡ How It Works

1. **Multi-Dimensional Feature Extraction**:
   For each measurement from `/api/v1/history`, the predictor extracts:
   * Current active power ($P$ in Watts)
   * Power derivative / momentum ($\Delta P = P_t - P_{t-1}$)
   * Rolling statistics over recent samples ($\mu_P, \sigma_P$)
   * Grid frequency ($f$ in Hz)
   * Voltage ($V_{L1}$), current ($I_{L1}$), and current harmonic distortion ($\text{THD}_{I, L1}$)

2. **Multi-Step Forward Projection**:
   * Uses an online adaptive linear regression pipeline with standard scaling (`StandardScaler | LinearRegression(optim=Adam)`).
   * Generates predictions for $+3\text{s}, +6\text{s}, +9\text{s}, +12\text{s}, +15\text{s}$ with dynamic confidence intervals ($95\%$).

3. **Self-Supervised Online Learning Loop**:
   * When a prediction is made for time $t + 15\text{s}$, it is placed in a feedback buffer.
   * As time advances and the true measurement arrives at $t + 15\text{s}$, the model:
     1. Computes the loss: $\text{error} = |P_{\text{actual}} - P_{\text{predicted}}|$
     2. Updates running performance: $\text{MAE}$
     3. Updates weights immediately with one SGD step: `model.learn_one(features, actual_power)`
     4. Feeds the error into an **ADWIN concept drift detector** to sense when an appliance switches on/off.

4. **Dashboard Integration**:
   * The predicted curve is sent to the Rust server via `POST /api/v1/forecast`.
   * The dashboard renders a **dashed projection line** directly on the Real Power SVG chart with an interactive legend and updated horizon status.

---

## 🚀 How to Run

Ensure the Rust server is running (e.g. `cargo run --bin pq-meter-server`), then start the forecaster:

```bash
# Using uv (fastest):
uv run python ml/predictor.py

# Or with custom options:
uv run python ml/predictor.py --server http://127.0.0.1:8080 --interval 1.0 --horizon 15.0
```

If `measurement-history.jsonl` exists in the root directory, the model will automatically warm-start its weights on startup before entering the live loop.

## Power-quality anomaly detection

The standalone anomaly detector identifies unusual electrical quality values. It uses
robust statistical detection based on feature medians and Median Absolute Deviation (MAD),
not supervised training or the forecasting model above.

It reads historical JSONL data and current SQLite data, normalizes both sources, removes
duplicates, and combines them chronologically in memory. The initial robust baseline is
used to exclude only extreme outliers before the final fixed baseline is built. Source
files are read only; the detector does not migrate, rewrite, or modify them.

Power-quality anomaly scoring uses only:

```text
frequency_hz
l1.voltage_v
l1.thd_current_pct
l1.thd_voltage_pct
```

Load-dependent fields such as total power, current, reactive power, and cos phi remain
available for NILM/device inference but are intentionally excluded from power-quality
anomaly scoring. Missing optional sensor values are handled independently per feature;
one unavailable value does not invalidate the complete measurement.

The report includes the anomaly score, strongest contributing feature, anomaly count and
anomaly percentage. Run it from the repository root with:

```bash
python ml/anomaly.py \
   --jsonl measurement-history.jsonl \
   --db data/history.db \
   --threshold 3.5 \
   --limit 10
```

### Example result

Example output from the current hackathon dataset (not a universal benchmark):

```text
Total samples: 520
Final baseline: 492
Anomalies detected: 45
Anomaly percentage: 8.65%
```
