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
