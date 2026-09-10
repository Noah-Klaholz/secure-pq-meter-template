use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_modbus::Slave;
use tower::ServiceExt;
use umg605_modbus_client::{Snapshot, Umg605ProClient, reg};

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
                    } else if start_addr == reg::THD_CURRENT_L1 && count == 2 {
                        let (hi, lo) = encode_f32(current_snapshot.thd_current_l1);
                        regs[0] = hi;
                        regs[1] = lo;
                    } else if start_addr == reg::VOLTAGE_L1 {
                        let set_f32 = |regs: &mut [u16], addr: u16, val: f32| {
                            let offset = (addr - reg::VOLTAGE_L1) as usize;
                            if offset + 1 < regs.len() {
                                let (hi, lo) = encode_f32(val);
                                regs[offset] = hi;
                                regs[offset + 1] = lo;
                            }
                        };
                        set_f32(&mut regs, reg::VOLTAGE_L1, current_snapshot.voltage_l1);
                        set_f32(&mut regs, reg::CURRENT_L1, current_snapshot.current_l1);
                        set_f32(&mut regs, reg::REAL_POWER_L1, current_snapshot.real_power_l1);
                        set_f32(
                            &mut regs,
                            reg::APPARENT_POWER_L1,
                            current_snapshot.apparent_power_l1,
                        );
                        set_f32(
                            &mut regs,
                            reg::REACTIVE_POWER_L1,
                            current_snapshot.reactive_power_l1,
                        );
                        set_f32(&mut regs, reg::COS_PHI_L1, current_snapshot.cos_phi_l1);
                        set_f32(&mut regs, reg::FREQUENCY, current_snapshot.frequency);
                        set_f32(
                            &mut regs,
                            reg::REAL_ENERGY_CONSUMED_L1,
                            current_snapshot.real_energy_consumed_l1,
                        );
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

/// Helper: converts a meter snapshot into the client JSON batch payload format.
fn format_client_batch_json(snapshot: &Snapshot) -> Vec<u8> {
    let item = serde_json::json!({
        "total_power": snapshot.real_power_l1,
        "systime": snapshot.systime,
        "frequency_hz": snapshot.frequency,
        "l1": {
            "voltage_v": snapshot.voltage_l1,
            "current_a": snapshot.current_l1,
            "real_power_w": snapshot.real_power_l1,
            "apparent_power_va": snapshot.apparent_power_l1,
            "reactive_power_var": snapshot.reactive_power_l1,
            "cos_phi": snapshot.cos_phi_l1,
            "real_energy_consumed_wh": snapshot.real_energy_consumed_l1,
            "thd_current_pct": snapshot.thd_current_l1,
        }
    });
    serde_json::to_vec(&vec![item]).unwrap()
}

#[tokio::test]
async fn full_e2e_pipeline_modbus_to_client_to_server_to_dashboard() {
    // 1. Setup mock Modbus hardware meter
    let mock_meter_state = Arc::new(Mutex::new(Snapshot {
        systime: 1_700_000_001,
        frequency: 50.0,
        voltage_l1: 230.0,
        current_l1: 0.435,
        real_power_l1: 100.0, // baseline 100.0 W
        apparent_power_l1: 100.0,
        reactive_power_l1: 0.0,
        cos_phi_l1: 1.0,
        real_energy_consumed_l1: 5000.0,
        thd_current_l1: 1.5,
    }));

    let (meter_addr, _server_task) = spawn_dynamic_mock_meter(mock_meter_state.clone()).await;

    // 2. Setup Umg605ProClient (client-side Modbus driver)
    let mut modbus_client =
        Umg605ProClient::connect_tcp(meter_addr, Slave(1), Duration::from_secs(2))
            .await
            .expect("client connects to mock modbus");

    // 3. Setup server components
    let meter = Arc::new(Mutex::new(MeterState::new(DUMMY_DEVICE_CATALOG.to_vec())));
    // Settled matching: min change 5W, settle tolerance 3W, 2 stable readings, match tolerance 5W
    let decision_method = Arc::new(Mutex::new(Box::new(SettledPowerMatch::new(
        5.0, 3.0, 2, 5.0,
    )) as Box<dyn crate::decision::DecisionMethod>));
    let reading_decoder = Arc::new(JsonReadingDecoder);

    let app_state = AppState {
        meter: meter.clone(),
        decision_method,
        reading_decoder,
    };
    let ingestion_app = api_router("/edh/v1/hello", app_state);

    let dashboard_source = Arc::new(LiveMeterSource {
        meter: meter.clone(),
        decision_method: "settled",
    });
    let dashboard_app = dashboard::router(dashboard_source);

    // ==========================================
    // Phase 1: Record Baseline (100.0 W)
    // ==========================================
    let snapshot = modbus_client.snapshot().await.unwrap();
    assert_eq!(snapshot.real_power_l1, 100.0);

    let batch_json = format_client_batch_json(&snapshot);
    let req = Request::post("/edh/v1/hello")
        .header("content-type", "application/json")
        .body(Body::from(batch_json))
        .unwrap();
    let resp = ingestion_app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_text = String::from_utf8(to_bytes(resp.into_body(), 1000).await.unwrap().to_vec()).unwrap();
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
    mock_meter_state.lock().unwrap().real_power_l1 = 123.0;
    mock_meter_state.lock().unwrap().systime += 1;

    // Reading 1 of device turn-on (candidate starts settling)
    let snapshot = modbus_client.snapshot().await.unwrap();
    assert_eq!(snapshot.real_power_l1, 123.0);
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
    let body_text = String::from_utf8(to_bytes(resp.into_body(), 1000).await.unwrap().to_vec()).unwrap();
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
    mock_meter_state.lock().unwrap().real_power_l1 = 100.0;
    mock_meter_state.lock().unwrap().systime += 1;

    // Reading 1 of removal
    let snapshot = modbus_client.snapshot().await.unwrap();
    assert_eq!(snapshot.real_power_l1, 100.0);
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
    let body_text = String::from_utf8(to_bytes(resp.into_body(), 1000).await.unwrap().to_vec()).unwrap();
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
