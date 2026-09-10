use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_modbus::Slave;
use umg605_modbus_client::{ConnectError, ReadError, Umg605ProClient, reg};

fn encode_f32(val: f32) -> (u16, u16) {
    let bits = val.to_bits();
    ((bits >> 16) as u16, (bits & 0xFFFF) as u16)
}

fn encode_i32(val: i32) -> (u16, u16) {
    let bits = val as u32;
    ((bits >> 16) as u16, (bits & 0xFFFF) as u16)
}

/// Spawns a mock Modbus TCP server that responds to holding register read requests.
async fn spawn_mock_modbus_server() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut header = [0u8; 12];
                while stream.read_exact(&mut header).await.is_ok() {
                    let tid = u16::from_be_bytes([header[0], header[1]]);
                    let uid = header[6];
                    let fc = header[7];
                    let start_addr = u16::from_be_bytes([header[8], header[9]]);
                    let count = u16::from_be_bytes([header[10], header[11]]);

                    if fc != 3 {
                        // Return exception: illegal function
                        let resp = [header[0], header[1], 0, 0, 0, 3, uid, fc | 0x80, 0x01];
                        let _ = stream.write_all(&resp).await;
                        continue;
                    }

                    // Special test addresses:
                    if start_addr == 9999 {
                        // Return exception: illegal data address
                        let resp = [header[0], header[1], 0, 0, 0, 3, uid, 0x83, 0x02];
                        let _ = stream.write_all(&resp).await;
                        continue;
                    }

                    if start_addr == 8888 {
                        // Simulate delay exceeding client timeout
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }

                    let mut regs = vec![0u16; count as usize];
                    if start_addr == reg::SYSTIME && count == 2 {
                        let (hi, lo) = encode_i32(1_700_000_000);
                        regs[0] = hi;
                        regs[1] = lo;
                    } else if start_addr == reg::THD_CURRENT_L1 && count == 2 {
                        let (hi, lo) = encode_f32(1.85);
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
                        set_f32(&mut regs, reg::VOLTAGE_L1, 230.5);
                        set_f32(&mut regs, reg::CURRENT_L1, 4.2);
                        set_f32(&mut regs, reg::REAL_POWER_L1, 968.1);
                        set_f32(&mut regs, reg::APPARENT_POWER_L1, 968.5);
                        set_f32(&mut regs, reg::REACTIVE_POWER_L1, 15.0);
                        set_f32(&mut regs, reg::COS_PHI_L1, 0.99);
                        set_f32(&mut regs, reg::FREQUENCY, 50.02);
                        set_f32(&mut regs, reg::REAL_ENERGY_CONSUMED_L1, 123456.0);
                    } else if count == 2 {
                        let (hi, lo) = encode_f32(42.0);
                        regs[0] = hi;
                        regs[1] = lo;
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

#[tokio::test]
async fn connects_and_reads_snapshot_end_to_end() {
    let (addr, _server) = spawn_mock_modbus_server().await;
    let mut client = Umg605ProClient::connect_tcp(addr, Slave(1), Duration::from_secs(2))
        .await
        .expect("should connect to mock server");

    let snapshot = client.snapshot().await.expect("snapshot read failed");

    assert_eq!(snapshot.systime, 1_700_000_000);
    assert_eq!(snapshot.voltage_l1, 230.5);
    assert_eq!(snapshot.current_l1, 4.2);
    assert_eq!(snapshot.real_power_l1, 968.1);
    assert_eq!(snapshot.apparent_power_l1, 968.5);
    assert_eq!(snapshot.reactive_power_l1, 15.0);
    assert_eq!(snapshot.cos_phi_l1, 0.99);
    assert_eq!(snapshot.frequency, 50.02);
    assert_eq!(snapshot.real_energy_consumed_l1, 123456.0);
    assert_eq!(snapshot.thd_current_l1, 1.85);
}

#[tokio::test]
async fn reads_individual_registers_and_blocks() {
    let (addr, _server) = spawn_mock_modbus_server().await;
    let mut client = Umg605ProClient::connect_tcp(addr, Slave(1), Duration::from_secs(2))
        .await
        .expect("should connect");

    // Single f32 read
    let power = client.real_power_l1().await.expect("reading power failed");
    assert_eq!(power, 42.0);

    // Single i32 read
    let systime = client.systime().await.expect("reading systime failed");
    assert_eq!(systime, 1_700_000_000);

    // Read block
    let block = client.read_block(reg::SYSTIME, 2).await.unwrap();
    assert_eq!(block.i32_at(reg::SYSTIME).unwrap(), 1_700_000_000);
}

#[tokio::test]
async fn handles_modbus_exceptions() {
    let (addr, _server) = spawn_mock_modbus_server().await;
    let mut client = Umg605ProClient::connect_tcp(addr, Slave(1), Duration::from_secs(2))
        .await
        .expect("should connect");

    // Address 9999 triggers exception 0x02
    let err = client.read_holding_registers(9999, 2).await;
    match err {
        Err(ReadError::ModbusException(code)) => {
            assert_eq!(code, tokio_modbus::ExceptionCode::IllegalDataAddress);
        }
        other => panic!("expected ModbusException, got {other:?}"),
    }
}

#[tokio::test]
async fn handles_read_timeout() {
    let (addr, _server) = spawn_mock_modbus_server().await;
    // Timeout set to 100ms, while address 8888 delays by 500ms
    let mut client = Umg605ProClient::connect_tcp(addr, Slave(1), Duration::from_millis(100))
        .await
        .expect("should connect");

    let err = client.read_holding_registers(8888, 2).await;
    assert!(matches!(err, Err(ReadError::Timeout(_))));
}

#[tokio::test]
async fn handles_connection_failure() {
    // Attempting to connect to an unused port
    let dead_addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
    let res = Umg605ProClient::connect_tcp(dead_addr, Slave(1), Duration::from_millis(100)).await;
    assert!(matches!(
        res,
        Err(ConnectError::Connect(_)) | Err(ConnectError::Timeout(_, _))
    ));
}
