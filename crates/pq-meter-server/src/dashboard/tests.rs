use std::sync::Mutex;

use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use tower::ServiceExt;

use super::{
    model::{HistoryResponse, LiveMeterSource, Snapshot},
    *,
};
use crate::{
    decision::{ClosestPowerMatch, DUMMY_DEVICE_CATALOG},
    input::MeterReading,
    meter::MeterState,
};

fn source() -> Arc<LiveMeterSource> {
    Arc::new(LiveMeterSource {
        meter: Arc::new(Mutex::new(MeterState::new(DUMMY_DEVICE_CATALOG.to_vec()))),
        decision_method: "immediate",
    })
}

#[test]
fn snapshot_tracks_baseline_addition_removal_and_preserves_last_change() {
    let source = source();
    let empty = source.snapshot().unwrap();
    assert_eq!(empty.total_power_watts, None);
    assert_eq!(empty.latest_reading, None);
    assert_eq!(empty.last_received_at, None);
    assert_eq!(empty.readings_received, 0);
    assert!(empty.devices.iter().all(|device| !device.active));

    let mut method = ClosestPowerMatch::new(3.0);
    source
        .meter
        .lock()
        .unwrap()
        .apply_reading(100.0.into(), &mut method);
    let baseline = source.snapshot().unwrap();
    assert_eq!(baseline.total_power_watts, Some(100.0));
    assert!(baseline.last_received_at.is_some());
    assert!(baseline.last_change.is_none());

    source
        .meter
        .lock()
        .unwrap()
        .apply_reading(123.0.into(), &mut method);
    let added = source.snapshot().unwrap();
    assert!(added.devices[0].active);
    assert_eq!(added.inferred_power_watts, 23.0);
    let added_at = added.last_change.unwrap().received_at;
    source
        .meter
        .lock()
        .unwrap()
        .apply_reading(123.0.into(), &mut method);
    assert_eq!(
        source.snapshot().unwrap().last_change.unwrap().received_at,
        added_at
    );

    source
        .meter
        .lock()
        .unwrap()
        .apply_reading(100.0.into(), &mut method);
    let removed = source.snapshot().unwrap();
    assert_eq!(removed.readings_received, 4);
    assert!(!removed.devices[0].active);
    assert_eq!(removed.inferred_power_watts, 0.0);
    assert_eq!(removed.last_change.unwrap().kind, "removed");
    // Snapshots are owned: a subsequent reading cannot mutate one already being sent.
    assert_eq!(baseline.total_power_watts, Some(100.0));
    assert_eq!(baseline.readings_received, 1);
}

#[tokio::test]
async fn http_snapshot_is_versioned_read_only_and_uncached() {
    let source = source();
    let app = router(source.clone());
    let response = app
        .clone()
        .oneshot(Request::get("/api/v1/state").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let value: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 100_000).await.unwrap()).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["total_power_watts"], serde_json::Value::Null);
    assert_eq!(value["latest_reading"], serde_json::Value::Null);
    assert_eq!(
        value["devices"].as_array().unwrap().len(),
        DUMMY_DEVICE_CATALOG.len()
    );
    let response = app
        .oneshot(
            Request::post("/api/v1/state")
                .body(Body::from(r#"{"total_power":999}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(source.snapshot().unwrap().readings_received, 0);
}

#[tokio::test]
async fn http_history_returns_recent_measurements() {
    let source = source();
    let mut method = ClosestPowerMatch::new(3.0);
    let reading: MeterReading = serde_json::from_value(serde_json::json!({
        "total_power": 123.0,
        "systime": 456,
        "frequency_hz": 50.0,
        "l1": {
            "voltage_v": 240.09,
            "current_a": 0.28,
            "real_power_w": 123.0,
            "apparent_power_va": 130.0,
            "reactive_power_var": 20.0,
            "cos_phi": 0.95,
            "real_energy_consumed_wh": 1000.0,
            "thd_voltage_pct": 1.2,
            "thd_current_pct": 2.0
        }
    }))
    .unwrap();
    source
        .meter
        .lock()
        .unwrap()
        .apply_reading(reading, &mut method);

    let response = router(source)
        .oneshot(Request::get("/api/v1/history").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 100_000).await.unwrap()).unwrap();
    let measurements = value["measurements"].as_array().unwrap();
    assert_eq!(measurements.len(), 1);
    assert_eq!(measurements[0]["data"]["total_power"], 123.0);
    assert_eq!(measurements[0]["data"]["systime"], 456);
    assert_eq!(measurements[0]["data"]["frequency_hz"], 50.0);
    assert_eq!(measurements[0]["data"]["l1"]["voltage_v"], 240.09);
    assert_eq!(measurements[0]["data"]["l1"]["current_a"], 0.28);
    assert!(measurements[0]["timestamp"].as_str().unwrap().contains('T'));
}

#[tokio::test]
async fn assets_have_correct_types_and_unknown_paths_return_not_found() {
    let app = router(source());
    for (path, content_type) in [
        ("/", "text/html; charset=utf-8"),
        ("/assets/styles.css", "text/css; charset=utf-8"),
        ("/assets/app.js", "text/javascript; charset=utf-8"),
        ("/assets/api.js", "text/javascript; charset=utf-8"),
        ("/assets/overview.js", "text/javascript; charset=utf-8"),
    ] {
        let response = app
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], content_type);
        assert!(
            response
                .headers()
                .contains_key(header::CONTENT_SECURITY_POLICY)
        );
        assert!(
            !to_bytes(response.into_body(), 100_000)
                .await
                .unwrap()
                .is_empty()
        );
    }
    let response = app
        .oneshot(Request::get("/api/v1/unknown").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unavailable_source_returns_json_error_instead_of_empty_measurements() {
    struct Unavailable;
    impl SnapshotSource for Unavailable {
        fn snapshot(&self) -> Result<Snapshot, &'static str> {
            Err("meter state is unavailable")
        }

        fn history(&self, _count: usize) -> Result<HistoryResponse, &'static str> {
            Err("meter state is unavailable")
        }
    }
    let response = router(Arc::new(Unavailable))
        .oneshot(Request::get("/api/v1/state").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let value: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1000).await.unwrap()).unwrap();
    assert_eq!(value["error"], "meter state is unavailable");
}
