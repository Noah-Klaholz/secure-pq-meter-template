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

The anomaly detector monitors electrical power-quality measurements and identifies unusual deviations from normal operating conditions.

Instead of using a supervised machine-learning model, the detector uses **robust statistical anomaly detection based on the Median Absolute Deviation (MAD)**. This approach works well for the available unlabeled measurement data and is less sensitive to extreme values than methods based on the mean and standard deviation.

For each power-quality feature, the median and Median Absolute Deviation (MAD) of the baseline are calculated:

$$
MAD = \mathrm{median}(|x_i - \mathrm{median}(x)|)
$$

A new measurement is then assigned a robust z-score:

$$
z = 0.6745 \frac{|x - \mathrm{median}(x)|}{MAD}
$$

A measurement is considered anomalous when:

$$
z > 3.5
$$

### Power-quality features

Anomaly detection intentionally focuses on electrical quality rather than changes caused by devices switching on or off:

- `frequency_hz`
- `l1.voltage_v`
- `l1.thd_current_pct`
- `l1.thd_voltage_pct`

Load-dependent values such as real power, current, reactive power and power factor (`cos_phi`) are excluded from anomaly scoring. These values remain available to the separate NILM/device-inference component.

This separation allows the two components to answer different questions:

- **NILM:** What changed in the electrical load?
- **Anomaly detection:** Is the electrical power quality unusual?

### Baseline

The detector reads measurements from historical JSONL data and the current SQLite history. Both sources are normalized, deduplicated and combined chronologically in memory.

An initial robust baseline is created from the available measurements. Extreme outliers are removed before constructing the final fixed baseline, preventing individual abnormal measurements from distorting the reference distribution.

Missing optional measurements are handled independently per feature. For example, a missing THD value does not invalidate voltage or frequency measurements from the same sample.

### Why robust statistics?

An ML-based approach using scikit-learn was initially considered for anomaly detection. However, the available hackathon dataset was small, unlabeled and contained multiple normal operating states.

Early experiments showed that treating different measurement sessions as separate normal/anomalous distributions produced excessive false positives. We therefore chose a robust MAD-based approach that is easier to interpret and better suited to the available data.

An ML-based detector such as Isolation Forest remains an option for future work once a larger and more representative dataset is available.

### Usage

Run the detector from the repository root:

```bash
python3 ml/anomaly.py \
  --jsonl data/measurement-history_chris.jsonl \
  --db data/history.db \
  --threshold 3.5 \
  --limit 10
```

The report includes the detected anomalies, their scores and the strongest contributing power-quality feature.

### Dashboard integration

The detector can publish its latest result to the dashboard while continuously monitoring the measurement history:

```bash
python3 ml/anomaly.py \
  --jsonl data/measurement-history_chris.jsonl \
  --db data/history.db \
  --server http://127.0.0.1:8080 \
  --interval 2
```

The latest result is published to:

```text
POST /api/v1/anomaly
```

and contains:

```text
is_anomaly
score
strongest_feature
timestamp
```

The dashboard displays this result in the **Supply status** card. If no scorable measurement is available, no anomaly state is reported instead of incorrectly displaying `NORMAL`.

The anomaly state is kept in memory by the server, so the publisher should remain running to provide continuous updates and restore the state after a server restart.

### Example

During the hackathon dataset evaluation, the detector processed 520 measurements:

| Metric | Result |
|---|---:|
| Total measurements | 520 |
| Final baseline | 492 |
| Detected anomalies | 45 |
| Anomaly rate | 8.65% |

These results describe the available hackathon dataset and are **not intended as a general performance benchmark**.
