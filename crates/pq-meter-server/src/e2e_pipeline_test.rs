use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_modbus::Slave;
use tower::ServiceExt;
use umg605_modbus_client::{Phases, Snapshot, Umg605ProClient, reg};

use crate::{
    api::{AppState, router as api_router},
    dashboard::{self, model::LiveMeterSource},
    decision::{DUMMY_DEVICE_CATALOG, SettledPowerMatch},
    input::JsonReadingDecoder,
    meter::MeterState,
};

fn encode_f32(val: f32) -> (u16, u16) {
    let bits = val.to_bits();
    ((bits >> 16) as u16, (bits & 0xFFFF) as u16)
}

fn encode_i32(val: i32) -> (u16, u16) {
    let bits = val as u32;
    ((bits >> 16) as u16, (bits & 0xFFFF) as u16)
}

/// Spawns a mock Modbus TCP server whose register values are dynamically controlled.
async fn spawn_dynamic_mock_meter(
    state: Arc<Mutex<Snapshot>>,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let state = state.clone();
            tokio::spawn(async move {
                let mut header = [0u8; 12];
                while stream.read_exact(&mut header).await.is_ok() {
                    let tid = u16::from_be_bytes([header[0], header[1]]);
                    let uid = header[6];
                    let fc = header[7];
                    let start_addr = u16::from_be_bytes([header[8], header[9]]);
                    let count = u16::from_be_bytes([header[10], header[11]]);

                    if fc != 3 {
                        let resp = [header[0], header[1], 0, 0, 0, 3, uid, fc | 0x80, 0x01];
                        let _ = stream.write_all(&resp).await;
                        continue;
                    }

                    let current_snapshot = *state.lock().unwrap();
                    let mut regs = vec![0u16; count as usize];

                    if start_addr == reg::SYSTIME && count == 2 {
                        let (hi, lo) = encode_i32(current_snapshot.systime);
                        regs[0] = hi;
                        regs[1] = lo;
                    } else if start_addr == reg::MEASUREMENT_BLOCK_START {
                        let set_f32 = |regs: &mut [u16], addr: u16, val: f32| {
                            let offset = (addr - reg::MEASUREMENT_BLOCK_START) as usize;
                            if offset + 1 < regs.len() {
                                let (hi, lo) = encode_f32(val);
                                regs[offset] = hi;
                                regs[offset + 1] = lo;
                            }
                        };
                        let set_phases = |regs: &mut [u16], addr: u16, values: Phases| {
                            for (phase, value) in values.into_iter().enumerate() {
                                set_f32(regs, addr + reg::PHASE_STRIDE * phase as u16, value);
                            }
                        };
                        set_phases(&mut regs, reg::VOLTAGE_L1, current_snapshot.voltage);
                        set_phases(&mut regs, reg::CURRENT_L1, current_snapshot.current);
                        set_phases(&mut regs, reg::REAL_POWER_L1, current_snapshot.real_power);
                        set_phases(
                            &mut regs,
                            reg::APPARENT_POWER_L1,
                            current_snapshot.apparent_power,
                        );
                        set_phases(
                            &mut regs,
                            reg::REACTIVE_POWER_L1,
                            current_snapshot.reactive_power,
                        );
                        set_phases(&mut regs, reg::COS_PHI_L1, current_snapshot.cos_phi);
                        set_phases(
                            &mut regs,
                            reg::REAL_ENERGY_CONSUMED_L1,
                            current_snapshot.real_energy_consumed,
                        );
                        set_phases(&mut regs, reg::THD_VOLTAGE_L1, current_snapshot.thd_voltage);
                        set_phases(&mut regs, reg::THD_CURRENT_L1, current_snapshot.thd_current);
                        set_f32(
                            &mut regs,
                            reg::REAL_POWER_SUM3,
                            current_snapshot.real_power_sum3,
                        );
                        set_f32(
                            &mut regs,
                            reg::APPARENT_POWER_SUM3,
                            current_snapshot.apparent_power_sum3,
                        );
                        set_f32(
                            &mut regs,
                            reg::REACTIVE_POWER_SUM3,
                            current_snapshot.reactive_power_sum3,
                        );
                        set_f32(&mut regs, reg::FREQUENCY, current_snapshot.frequency);
                    }

                    let byte_count = (count * 2) as u8;
                    let len = (3 + byte_count as usize) as u16;
                    let mut resp = Vec::with_capacity(9 + regs.len() * 2);
                    resp.extend_from_slice(&tid.to_be_bytes());
                    resp.extend_from_slice(&0u16.to_be_bytes());
                    resp.extend_from_slice(&len.to_be_bytes());
                    resp.push(uid);
                    resp.push(3);
                    resp.push(byte_count);
                    for reg in regs {
                        resp.extend_from_slice(&reg.to_be_bytes());
                    }

                    if stream.write_all(&resp).await.is_err() {
                        break;
                    }
                }
            });
        }
    });

    (addr, handle)
}

/// A value the meter could determine, or `None` for one it reported as NaN. Mirrors the
/// mapping the gateway client applies before sending.
fn measured(value: f32) -> Option<f32> {
    value.is_finite().then_some(value)
}

/// Helper: converts a meter snapshot into the client JSON batch payload format.
fn format_client_batch_json(snapshot: &Snapshot) -> Vec<u8> {
    let phase = |phase: usize| {
        serde_json::json!({
            "voltage_v": measured(snapshot.voltage[phase]),
            "current_a": measured(snapshot.current[phase]),
            "real_power_w": measured(snapshot.real_power[phase]),
            "apparent_power_va": measured(snapshot.apparent_power[phase]),
            "reactive_power_var": measured(snapshot.reactive_power[phase]),
            "cos_phi": measured(snapshot.cos_phi[phase]),
            "real_energy_consumed_wh": measured(snapshot.real_energy_consumed[phase]),
            "thd_voltage_pct": measured(snapshot.thd_voltage[phase]),
            "thd_current_pct": measured(snapshot.thd_current[phase]),
        })
    };
    let item = serde_json::json!({
        "total_power": snapshot.real_power_sum3,
        "systime": snapshot.systime,
        "frequency_hz": measured(snapshot.frequency),
        "l1": phase(0),
        "l2": phase(1),
        "l3": phase(2),
        "totals": {
            "real_power_w": measured(snapshot.real_power_sum3),
            "apparent_power_va": measured(snapshot.apparent_power_sum3),
            "reactive_power_var": measured(snapshot.reactive_power_sum3),
        }
    });
    serde_json::to_vec(&vec![item]).unwrap()
}

/// The meter in the lab: L1 carries the load, the other two phases are not connected, so
/// the meter reports the distortion of their absent current as NaN.
fn lab_snapshot(systime: i32, real_power_l1: f32) -> Snapshot {
    Snapshot {
        systime,
        frequency: 50.0,
        voltage: [230.0, 0.0, 0.0],
        current: [0.435, 0.0, 0.0],
        real_power: [real_power_l1, 0.0, 0.0],
        real_power_sum3: real_power_l1,
        apparent_power: [100.0, 0.0, 0.0],
        apparent_power_sum3: 100.0,
        reactive_power: [0.0, 0.0, 0.0],
        reactive_power_sum3: 0.0,
        cos_phi: [1.0, 1.0, 1.0],
        real_energy_consumed: [5000.0, 0.0, 0.0],
        thd_voltage: [1.5, f32::NAN, f32::NAN],
        thd_current: [1.5, f32::NAN, f32::NAN],
    }
}

/// Sets the load on L1 and keeps the three-phase sum consistent with it.
fn set_real_power(snapshot: &mut Snapshot, watts: f32) {
    snapshot.real_power[0] = watts;
    snapshot.real_power_sum3 = watts + snapshot.real_power[1] + snapshot.real_power[2];
}

#[tokio::test]
async fn full_e2e_pipeline_modbus_to_client_to_server_to_dashboard() {
    // 1. Setup mock Modbus hardware meter (baseline 100.0 W)
    let mock_meter_state = Arc::new(Mutex::new(lab_snapshot(1_700_000_001, 100.0)));

    let (meter_addr, _server_task) = spawn_dynamic_mock_meter(mock_meter_state.clone()).await;

    // 2. Setup Umg605ProClient (client-side Modbus driver)
    let mut modbus_client =
        Umg605ProClient::connect_tcp(meter_addr, Slave(1), Duration::from_secs(2))
            .await
            .expect("client connects to mock modbus");

    // 3. Setup server components
    let meter = Arc::new(Mutex::new(MeterState::new(DUMMY_DEVICE_CATALOG.to_vec())));
    // Settled matching: min change 5W, settle tolerance 3W, 2 stable readings, match tolerance 5W
    let decision_method = Arc::new(Mutex::new(
        Box::new(SettledPowerMatch::new(5.0, 3.0, 2, 5.0))
            as Box<dyn crate::decision::DecisionMethod>,
    ));
    let reading_decoder = Arc::new(JsonReadingDecoder);

    let app_state = AppState {
        meter: meter.clone(),
        decision_method,
        reading_decoder,
        pending_config: Arc::new(Mutex::new(None)),
    };
    let ingestion_app = api_router("/edh/v1/hello", app_state);

    let dashboard_source = Arc::new(LiveMeterSource {
        meter: meter.clone(),
        decision_method: "settled",
    });
    let dashboard_app = dashboard::router(dashboard_source, Arc::new(Mutex::new(None)));

    // ==========================================
    // Phase 1: Record Baseline (100.0 W)
    // ==========================================
    let snapshot = modbus_client.snapshot().await.unwrap();
    assert_eq!(snapshot.real_power_sum3, 100.0);

    let batch_json = format_client_batch_json(&snapshot);
    let req = Request::post("/edh/v1/hello")
        .header("content-type", "application/json")
        .body(Body::from(batch_json))
        .unwrap();
    let resp = ingestion_app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_text =
        String::from_utf8(to_bytes(resp.into_body(), 1000).await.unwrap().to_vec()).unwrap();
    assert_eq!(body_text, "baseline recorded at 100.0 W\n");

    // Verify dashboard reflects baseline
    let dash_req = Request::get("/api/v1/state").body(Body::empty()).unwrap();
    let dash_resp = dashboard_app.clone().oneshot(dash_req).await.unwrap();
    let dash_json: serde_json::Value =
        serde_json::from_slice(&to_bytes(dash_resp.into_body(), 10_000).await.unwrap()).unwrap();
    assert_eq!(dash_json["total_power_watts"], 100.0);
    assert_eq!(dash_json["inferred_power_watts"], 0.0);
    assert_eq!(dash_json["readings_received"], 1);

    // ==========================================
    // Phase 2: Device Turned On (+23.0 W Raspberry Pi)
    // ==========================================
    // Meter power jumps to 123.0 W
    set_real_power(&mut mock_meter_state.lock().unwrap(), 123.0);
    mock_meter_state.lock().unwrap().systime += 1;

    // Reading 1 of device turn-on (candidate starts settling)
    let snapshot = modbus_client.snapshot().await.unwrap();
    assert_eq!(snapshot.real_power_sum3, 123.0);
    let req = Request::post("/edh/v1/hello")
        .body(Body::from(format_client_batch_json(&snapshot)))
        .unwrap();
    let resp = ingestion_app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Reading 2 of device turn-on (settles and triggers detection)
    mock_meter_state.lock().unwrap().systime += 1;
    let snapshot = modbus_client.snapshot().await.unwrap();
    let req = Request::post("/edh/v1/hello")
        .body(Body::from(format_client_batch_json(&snapshot)))
        .unwrap();
    let resp = ingestion_app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_text =
        String::from_utf8(to_bytes(resp.into_body(), 1000).await.unwrap().to_vec()).unwrap();
    assert!(body_text.starts_with("added Baseline"));

    // Verify dashboard reflects device addition
    let dash_req = Request::get("/api/v1/state").body(Body::empty()).unwrap();
    let dash_resp = dashboard_app.clone().oneshot(dash_req).await.unwrap();
    let dash_json: serde_json::Value =
        serde_json::from_slice(&to_bytes(dash_resp.into_body(), 10_000).await.unwrap()).unwrap();
    assert_eq!(dash_json["total_power_watts"], 123.0);
    assert_eq!(dash_json["inferred_power_watts"], 23.0);
    assert_eq!(dash_json["readings_received"], 3);

    let devices = dash_json["devices"].as_array().unwrap();
    let baseline_dev = devices
        .iter()
        .find(|d| d["id"] == "baseline-2-raspberry-pi")
        .expect("baseline device found");
    assert_eq!(baseline_dev["active"], true);

    // ==========================================
    // Phase 3: Device Turned Off (-23.0 W Raspberry Pi)
    // ==========================================
    // Meter power drops back to 100.0 W
    set_real_power(&mut mock_meter_state.lock().unwrap(), 100.0);
    mock_meter_state.lock().unwrap().systime += 1;

    // Reading 1 of removal
    let snapshot = modbus_client.snapshot().await.unwrap();
    assert_eq!(snapshot.real_power_sum3, 100.0);
    let req = Request::post("/edh/v1/hello")
        .body(Body::from(format_client_batch_json(&snapshot)))
        .unwrap();
    let resp = ingestion_app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Reading 2 of removal (settles and removes device)
    mock_meter_state.lock().unwrap().systime += 1;
    let snapshot = modbus_client.snapshot().await.unwrap();
    let req = Request::post("/edh/v1/hello")
        .body(Body::from(format_client_batch_json(&snapshot)))
        .unwrap();
    let resp = ingestion_app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_text =
        String::from_utf8(to_bytes(resp.into_body(), 1000).await.unwrap().to_vec()).unwrap();
    assert!(body_text.starts_with("removed Baseline"));

    // Verify dashboard reflects device removal
    let dash_req = Request::get("/api/v1/state").body(Body::empty()).unwrap();
    let dash_resp = dashboard_app.clone().oneshot(dash_req).await.unwrap();
    let dash_json: serde_json::Value =
        serde_json::from_slice(&to_bytes(dash_resp.into_body(), 10_000).await.unwrap()).unwrap();
    assert_eq!(dash_json["total_power_watts"], 100.0);
    assert_eq!(dash_json["inferred_power_watts"], 0.0);
    assert_eq!(dash_json["readings_received"], 5);

    let devices = dash_json["devices"].as_array().unwrap();
    let baseline_dev = devices
        .iter()
        .find(|d| d["id"] == "baseline-2-raspberry-pi")
        .unwrap();
    assert_eq!(baseline_dev["active"], false);
}
