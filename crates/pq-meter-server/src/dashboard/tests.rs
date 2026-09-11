use std::sync::Mutex;

use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use tower::ServiceExt;

use super::{
    model::{HistorySeries, LiveMeterSource, Snapshot},
    *,
};
use crate::{
    decision::{ClosestPowerMatch, DUMMY_DEVICE_CATALOG},
    meter::MeterState,
};

fn source() -> Arc<LiveMeterSource> {
    Arc::new(LiveMeterSource {
        meter: Arc::new(Mutex::new(MeterState::new(DUMMY_DEVICE_CATALOG.to_vec()))),
        decision_method: "immediate",
    })
}

#[tokio::test]
async fn anomaly_updates_reach_the_live_snapshot_and_can_be_cleared() {
    let app = router(source(), Arc::new(Mutex::new(None)));
    let response = app
        .clone()
        .oneshot(Request::get("/api/v1/anomaly").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    for payload in [
        serde_json::json!({
            "is_anomaly": true, "score": 6.745,
            "strongest_feature": "l1.voltage_v", "timestamp": "2026-09-11T11:00:00Z"
        }),
        serde_json::json!({
            "is_anomaly": false, "score": 0.0,
            "strongest_feature": "frequency_hz", "timestamp": "2026-09-11T11:00:01Z"
        }),
        serde_json::Value::Null,
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/anomaly")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let response = app
            .clone()
            .oneshot(Request::get("/api/v1/anomaly").body(Body::empty()).unwrap())
            .await
            .unwrap();
        if payload.is_null() {
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
        } else {
            assert_eq!(response.status(), StatusCode::OK);
            let value: serde_json::Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 10_000).await.unwrap())
                    .unwrap();
            assert_eq!(value, payload);
        }

        let response = app
            .clone()
            .oneshot(Request::get("/api/v1/state").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let value: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 100_000).await.unwrap()).unwrap();
        assert_eq!(value["power_quality"]["anomaly"], payload);
    }
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
    let app = router(source.clone(), Arc::new(Mutex::new(None)));
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
async fn history_returns_oldest_first_dashboard_series() {
    let source = source();
    let mut method = ClosestPowerMatch::new(3.0);
    source
        .meter
        .lock()
        .unwrap()
        .apply_reading(lab_reading(123.0, 240.09, 50.0), &mut method);
    source
        .meter
        .lock()
        .unwrap()
        .apply_reading(lab_reading(456.0, 239.99, 49.9), &mut method);

    let series: HistorySeries = source.history().unwrap();
    assert_eq!(series.schema_version, 1);
    assert!(series.window_seconds > 0);
    assert_eq!(series.samples.len(), 2);
    assert_eq!(series.samples[0].total_power_watts, 123.0);
    assert_eq!(series.samples[0].frequency_hz, Some(50.0));
    assert_eq!(series.samples[0].voltage_v[0], Some(240.09));
    assert_eq!(series.samples[0].current_a[0], Some(2.0));
    assert_eq!(series.samples[0].thd_voltage_pct[0], Some(1.9));
    assert_eq!(series.samples[0].thd_current_pct[0], Some(3.0));
    assert_eq!(series.samples[1].total_power_watts, 456.0);
    assert_eq!(series.samples[1].voltage_v[0], Some(239.99));

    let response = router(source, Arc::new(Mutex::new(None)))
        .oneshot(Request::get("/api/v1/history").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 100_000).await.unwrap()).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert!(value["window_seconds"].as_u64().unwrap() > 0);
    let samples = value["samples"].as_array().unwrap();
    assert_eq!(samples.len(), 2);
    assert_eq!(samples[0]["total_power_watts"], 123.0);
    assert_eq!(samples[0]["frequency_hz"], 50.0);
    assert_eq!(samples[0]["voltage_v"][0], 240.09);
    assert_eq!(samples[0]["current_a"][0], 2.0);
}

#[tokio::test]
async fn assets_have_correct_types_and_unknown_paths_return_not_found() {
    let app = router(source(), Arc::new(Mutex::new(None)));
    for (path, content_type) in [
        ("/", "text/html; charset=utf-8"),
        ("/assets/styles.css", "text/css; charset=utf-8"),
        ("/assets/app.js", "text/javascript; charset=utf-8"),
        ("/assets/api.js", "text/javascript; charset=utf-8"),
        ("/assets/overview.js", "text/javascript; charset=utf-8"),
        ("/assets/charts.js", "text/javascript; charset=utf-8"),
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
        .oneshot(Request::get("/api/v1/nothing").body(Body::empty()).unwrap())
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
        fn history(&self) -> Result<HistorySeries, &'static str> {
            Err("meter state is unavailable")
        }
    }
    let response = router(Arc::new(Unavailable), Arc::new(Mutex::new(None)))
        .oneshot(Request::get("/api/v1/state").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let value: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1000).await.unwrap()).unwrap();
    assert_eq!(value["error"], "meter state is unavailable");
}

/// A reading shaped like the gateway's, with the load on L1 and the other phases unwired.
fn lab_reading(power: f32, voltage: f32, frequency: f32) -> crate::input::MeterReading {
    use crate::input::{MeterReading, PhaseReading, TotalsReading};
    let unwired = PhaseReading {
        voltage_v: Some(0.0),
        current_a: Some(0.0),
        real_power_w: Some(0.0),
        apparent_power_va: Some(0.0),
        reactive_power_var: Some(0.0),
        cos_phi: Some(1.0),
        real_energy_consumed_wh: Some(0.0),
        thd_voltage_pct: None,
        thd_current_pct: None,
    };
    MeterReading {
        total_power: power,
        systime: Some(1_789_000_000),
        frequency_hz: Some(frequency),
        l1: Some(PhaseReading {
            voltage_v: Some(voltage),
            current_a: Some(2.0),
            real_power_w: Some(power),
            apparent_power_va: Some(power.abs() * 1.1),
            reactive_power_var: Some(-30.0),
            cos_phi: Some(0.9),
            real_energy_consumed_wh: Some(4000.0),
            thd_voltage_pct: Some(1.9),
            thd_current_pct: Some(3.0),
        }),
        l2: Some(unwired),
        l3: Some(unwired),
        totals: Some(TotalsReading {
            real_power_w: Some(power),
            apparent_power_va: Some(power.abs() * 1.1),
            reactive_power_var: Some(-30.0),
        }),
        heartbeat: false,
    }
}

#[test]
fn snapshot_carries_every_phase_the_limits_and_the_events() {
    let source = source();
    let mut method = ClosestPowerMatch::new(3.0);
    source
        .meter
        .lock()
        .unwrap()
        .apply_reading(lab_reading(500.0, 230.0, 50.0), &mut method);

    let snapshot = source.snapshot().unwrap();
    let value = serde_json::to_value(&snapshot).unwrap();

    // All three phases are present, including the ones with nothing wired to them.
    let phases = value["power_quality"]["phases"].as_array().unwrap();
    assert_eq!(phases.len(), 3);
    assert_eq!(phases[0]["name"], "L1");
    assert_eq!(phases[0]["connected"], true);
    assert_eq!(phases[2]["connected"], false);
    // A value the meter could not determine stays null rather than becoming zero.
    assert_eq!(phases[2]["thd_current_pct"], serde_json::Value::Null);

    assert_eq!(value["power_quality"]["frequency_hz"], 50.0);
    assert_eq!(value["power_quality"]["flow"], "import");
    assert!(
        value["power_quality"]["violations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // The dashboard draws the same bands the receiver judges against.
    assert_eq!(value["limits"]["voltage_min_v"], 207.0);
    assert_eq!(value["limits"]["frequency_max_hz"], 50.5);
}

#[test]
fn snapshot_reports_export_and_flags_a_bad_supply() {
    let source = source();
    let mut method = ClosestPowerMatch::new(3.0);
    source
        .meter
        .lock()
        .unwrap()
        .apply_reading(lab_reading(-1200.0, 195.0, 48.5), &mut method);

    let value = serde_json::to_value(source.snapshot().unwrap()).unwrap();
    assert_eq!(value["power_quality"]["flow"], "export");
    assert_eq!(value["power_quality"]["total_power_watts"], -1200.0);

    let violations = value["power_quality"]["violations"].as_array().unwrap();
    let quantities: Vec<_> = violations
        .iter()
        .map(|violation| violation["quantity"].as_str().unwrap())
        .collect();
    assert_eq!(quantities, vec!["frequency_hz", "voltage_v"]);
    assert_eq!(violations[1]["phase"], "L1");
}

#[test]
fn transport_state_is_judged_by_the_receiver_not_claimed_by_the_gateway() {
    use crate::transport::GatewayTransport;

    let source = source();
    // Before anything arrives there is nothing to claim.
    let value = serde_json::to_value(source.snapshot().unwrap()).unwrap();
    assert_eq!(value["transport"]["state"], "waiting");
    assert_eq!(value["transport"]["gateway_reporting"], false);
    assert_eq!(value["transport"]["scion_path"], serde_json::Value::Null);

    let mut method = ClosestPowerMatch::new(3.0);
    {
        let mut meter = source.meter.lock().unwrap();
        meter.record_transport(GatewayTransport {
            scion_path: Some("1-ff00:0:132 1>3 2-ff00:0:212".to_owned()),
            queued_readings: Some(4),
            last_ack_latency_ms: Some(11.5),
            failover_count: Some(1),
            dropped_readings: Some(6),
            modbus_reconnects: Some(2),
        });
        meter.apply_reading(lab_reading(500.0, 230.0, 50.0), &mut method);
    }

    let value = serde_json::to_value(source.snapshot().unwrap()).unwrap();
    assert_eq!(value["transport"]["state"], "live");
    assert_eq!(value["transport"]["gateway_reporting"], true);
    assert_eq!(
        value["transport"]["scion_path"],
        "1-ff00:0:132 1>3 2-ff00:0:212"
    );
    assert_eq!(value["transport"]["last_ack_latency_ms"], 11.5);
    assert_eq!(value["transport"]["queued_readings"], 4);
    assert_eq!(value["transport"]["failover_count"], 1);
    assert_eq!(value["transport"]["dropped_readings"], 6);
    assert_eq!(value["transport"]["modbus_reconnects"], 2);
}

#[test]
fn history_holds_the_accepted_readings_in_order() {
    let source = source();
    let mut method = ClosestPowerMatch::new(3.0);
    for power in [100.0, 200.0, 300.0] {
        source
            .meter
            .lock()
            .unwrap()
            .apply_reading(lab_reading(power, 230.0, 50.0), &mut method);
    }

    let series = source.history().unwrap();
    assert_eq!(series.window_seconds, 60);
    assert_eq!(series.samples.len(), 3);
    let powers: Vec<_> = series
        .samples
        .iter()
        .map(|sample| sample.total_power_watts)
        .collect();
    assert_eq!(powers, vec![100.0, 200.0, 300.0]);

    let value = serde_json::to_value(&series).unwrap();
    let first = &value["samples"][0];
    assert_eq!(first["frequency_hz"], 50.0);
    assert_eq!(first["voltage_v"][0], 230.0);
    assert_eq!(first["voltage_v"][1], 0.0);
    // The unwired phases carry no distortion figure, and that survives to the chart.
    assert_eq!(first["thd_current_pct"][2], serde_json::Value::Null);
    assert!(first["at"].as_str().unwrap().contains('T'));
}

#[tokio::test]
async fn history_endpoint_is_versioned_and_separate_from_the_live_snapshot() {
    let source = source();
    let mut method = ClosestPowerMatch::new(3.0);
    source
        .meter
        .lock()
        .unwrap()
        .apply_reading(lab_reading(500.0, 230.0, 50.0), &mut method);

    let response = router(source, Arc::new(Mutex::new(None)))
        .oneshot(Request::get("/api/v1/history").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let value: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 200_000).await.unwrap()).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["samples"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn an_unavailable_source_fails_the_history_endpoint_too() {
    struct Unavailable;
    impl SnapshotSource for Unavailable {
        fn snapshot(&self) -> Result<Snapshot, &'static str> {
            Err("meter state is unavailable")
        }
        fn history(&self) -> Result<HistorySeries, &'static str> {
            Err("meter state is unavailable")
        }
    }
    let response = router(Arc::new(Unavailable), Arc::new(Mutex::new(None)))
        .oneshot(Request::get("/api/v1/history").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

fn rename_request(id: &str, name: &str) -> Request<Body> {
    Request::put(format!("/api/v1/devices/{id}/label"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::json!({ "name": name }).to_string()))
        .unwrap()
}

#[tokio::test]
async fn rename_persists_and_updates_devices_and_last_change_without_changing_detection() {
    use crate::labels::DeviceLabels;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("labels.json");
    let source = Arc::new(LiveMeterSource {
        meter: Arc::new(Mutex::new(MeterState::with_labels(
            DUMMY_DEVICE_CATALOG.to_vec(),
            DeviceLabels::load(path.clone()).unwrap(),
        ))),
        decision_method: "immediate",
    });
    let mut method = ClosestPowerMatch::new(3.0);
    for power in [100.0, 123.0] {
        source
            .meter
            .lock()
            .unwrap()
            .apply_reading(power.into(), &mut method);
    }
    let id = DUMMY_DEVICE_CATALOG[0].id;
    let response = router(source.clone(), Arc::new(Mutex::new(None)))
        .oneshot(rename_request(id, "  Büro & Server <1>  "))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let snapshot = source.snapshot().unwrap();
    assert_eq!(snapshot.devices[0].name, "Büro & Server <1>");
    assert_eq!(
        snapshot.last_change.unwrap().device_name,
        "Büro & Server <1>"
    );
    assert_eq!(snapshot.readings_received, 2);
    assert_eq!(snapshot.inferred_power_watts, 23.0);
    source
        .meter
        .lock()
        .unwrap()
        .apply_reading(100.0.into(), &mut method);
    assert_eq!(
        source.snapshot().unwrap().last_change.unwrap().device_name,
        "Büro & Server <1>"
    );

    let restarted = LiveMeterSource {
        meter: Arc::new(Mutex::new(MeterState::with_labels(
            DUMMY_DEVICE_CATALOG.to_vec(),
            DeviceLabels::load(path).unwrap(),
        ))),
        decision_method: "immediate",
    };
    assert_eq!(
        restarted.snapshot().unwrap().devices[0].name,
        "Büro & Server <1>"
    );
}

#[tokio::test]
async fn rename_rejects_invalid_names_unknown_ids_and_reports_failed_persistence() {
    let source = source();
    let app = router(source.clone(), Arc::new(Mutex::new(None)));
    let id = DUMMY_DEVICE_CATALOG[0].id;
    for name in [
        "".to_owned(),
        "   ".to_owned(),
        "x".repeat(81),
        "a\nb".to_owned(),
    ] {
        let response = app
            .clone()
            .oneshot(rename_request(id, &name))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    let response = app
        .clone()
        .oneshot(rename_request("missing", "Name"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    // This source has no persistence path. It must not pretend to have saved the label.
    let response = app.oneshot(rename_request(id, "New name")).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        source.snapshot().unwrap().devices[0].name,
        DUMMY_DEVICE_CATALOG[0].name
    );
}

#[test]
fn saved_adaptive_identities_survive_restart_and_a_different_connection_order() {
    use crate::{
        decision::{AdaptiveNilm, DecisionMethod, DeviceChange},
        labels::DeviceLabels,
    };
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("labels.json");
    let mut method = AdaptiveNilm::new();
    let mut meter = MeterState::with_labels(Vec::new(), DeviceLabels::load(path.clone()).unwrap());
    for power in [23.0, 83.0, 283.0] {
        meter.apply_reading(lab_reading(power, 230.0, 50.0), &mut method);
    }
    meter.rename_device("learned-1", "Desk lamp").unwrap();
    meter.rename_device("learned-2", "Monitor").unwrap();
    let labels = DeviceLabels::load(path).unwrap();
    let mut restored = AdaptiveNilm::restore(labels.appliances.clone()).unwrap();
    assert_eq!(
        serde_json::to_value(method.learned_profiles()).unwrap(),
        serde_json::to_value(restored.learned_profiles()).unwrap()
    );
    let mut meter = MeterState::with_labels(restored.learned_devices(), labels);
    meter.apply_reading(lab_reading(23.0, 230.0, 50.0), &mut restored);
    // The second device reconnects first; the label follows its signature, not discovery order.
    let change = meter.apply_reading(lab_reading(223.0, 230.0, 50.0), &mut restored);
    assert!(matches!(change, DeviceChange::Added(device) if device.id == "learned-2"));
    assert_eq!(meter.snapshot().device_names["learned-2"], "Monitor");
    let change = meter.apply_reading(lab_reading(283.0, 230.0, 50.0), &mut restored);
    assert!(matches!(change, DeviceChange::Added(device) if device.id == "learned-1"));
    // A new discovery cannot reuse a restored ID.
    let change = meter.apply_reading(lab_reading(783.0, 230.0, 50.0), &mut restored);
    assert!(matches!(change, DeviceChange::Added(device) if device.id == "learned-3"));
}

#[tokio::test]
async fn history_survives_long_downtime_without_replaying_live_state() {
    use crate::{history::History, history_store::HistoryStore, labels::DeviceLabels};
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("history.db");
    let legacy = directory.path().join("history.jsonl");
    let (mut archive, _) =
        HistoryStore::open_with_legacy(&database, &legacy, &mut History::bounded(600)).unwrap();
    let at = chrono::DateTime::parse_from_rfc3339("2025-01-02T03:04:05Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    archive
        .append(&[lab_reading(123.0, 230.0, 49.99)], at)
        .unwrap();
    drop(archive);
    let source = Arc::new(LiveMeterSource {
        meter: Arc::new(Mutex::new(
            MeterState::with_labels(DUMMY_DEVICE_CATALOG.to_vec(), DeviceLabels::default())
                .with_history_file(&database, &legacy)
                .unwrap(),
        )),
        decision_method: "immediate",
    });
    let response = router(source.clone(), Arc::new(Mutex::new(None)))
        .oneshot(Request::get("/api/v1/history").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 100_000).await.unwrap()).unwrap();
    assert_eq!(value["persistent"], true);
    assert_eq!(value["stored_readings"], 1);
    assert_eq!(value["samples"][0]["at"], "2025-01-02T03:04:05+00:00");
    assert_eq!(value["samples"][0]["total_power_watts"], 123.0);
    assert_eq!(value["samples"][0]["frequency_hz"], 49.99);
    assert_eq!(
        value["samples"][0]["thd_current_pct"][2],
        serde_json::Value::Null
    );
    let live = source.snapshot().unwrap();
    assert_eq!(live.readings_received, 0);
    assert_eq!(live.total_power_watts, None);
    assert_eq!(live.transport.state, "waiting");
    assert!(live.devices.iter().all(|device| !device.active));

    // New session readings replace the old displayed window, but do not erase its archive.
    source
        .meter
        .lock()
        .unwrap()
        .apply_reading(200.0.into(), &mut ClosestPowerMatch::new(3.0));
    let series = source.history().unwrap();
    assert_eq!(series.stored_readings, 2);
    assert_eq!(series.samples.len(), 1);
    assert_eq!(series.samples[0].total_power_watts, 200.0);
    assert!(source.snapshot().unwrap().last_change.is_none());
}

#[tokio::test]
async fn forecast_endpoint_accepts_and_serves_predictions() {
    let source = source();
    let app = router(source, Arc::new(Mutex::new(None)));

    // Initial state has no forecast.
    let response = app
        .clone()
        .oneshot(
            Request::get("/api/v1/forecast")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    // Post a forecast.
    let forecast_payload = serde_json::json!({
        "generated_at": "2026-09-11T10:00:00Z",
        "horizon_seconds": 15.0,
        "model_name": "River Online SGDRegressor",
        "mae": 1.25,
        "points": [
            {
                "at": "2026-09-11T10:00:05Z",
                "predicted_watts": 142.5,
                "lower_bound_watts": 138.0,
                "upper_bound_watts": 147.0
            },
            {
                "at": "2026-09-11T10:00:10Z",
                "predicted_watts": 143.0,
                "lower_bound_watts": 137.5,
                "upper_bound_watts": 148.5
            }
        ]
    });

    let response = app
        .clone()
        .oneshot(
            Request::post("/api/v1/forecast")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_vec(&forecast_payload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    // Get the forecast.
    let response = app
        .clone()
        .oneshot(
            Request::get("/api/v1/forecast")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 10_000).await.unwrap()).unwrap();
    assert_eq!(value["model_name"], "River Online SGDRegressor");
    assert_eq!(value["points"].as_array().unwrap().len(), 2);
    assert_eq!(value["points"][0]["predicted_watts"], 142.5);

    // History series now includes forecast.
    let response = app
        .oneshot(Request::get("/api/v1/history").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 100_000).await.unwrap()).unwrap();
    assert_eq!(value["forecast"]["model_name"], "River Online SGDRegressor");
    assert_eq!(value["forecast"]["points"].as_array().unwrap().len(), 2);
}
