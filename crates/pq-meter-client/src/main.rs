//! Client side of the Energy Data Hackdays gateway challenge.
//!
//! Sends one message to `pq-meter-server` over SCION and prints the answer. Run it on the
//! gateway (Raspberry Pi); run `pq-meter-server` on the laptop.
//!
//! The work is done by [`scion_http3::Client`], the high-level HTTP/3 client of the SDK. It
//! keeps a pool of connections, so a program that sends measurements in a loop can reuse one
//! client and pay for the connection only once.
//!
//! The client needs two addresses:
//!
//! * `--endhost-api`: the URL of the endhost API of its own AS. This is where the client asks
//!   for paths and for the SNAP that carries its packets. The server prints this URL when it
//!   starts.
//! * `--server`: the SCION address of the HTTP/3 server, also printed by the server.

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::Parser;
use scion_http3::{Client, Config, Request, scion_quic::quic::config::QuicConfig};
use sciparse::address::ip_socket_addr::ScionSocketIpAddr;
use tokio_modbus::Slave;
use umg605_modbus_client::{DEFAULT_MODBUS_PORT, Snapshot, Umg605ProClient};
use url::Url;

/// SCION AS of the server.
const SERVER_AS: &str = "2-ff00:0:212";

/// Port of the HTTP/3 server.
const SERVER_PORT: u16 = 60000;

/// Default port of the gateway endhost API.
const GATEWAY_ENDHOST_API_PORT: u16 = 31000;

/// Unit id for a directly addressed Modbus TCP device, per the Modbus TCP spec.
const DEFAULT_MODBUS_UNIT: u8 = 1;

const DEFAULT_TIMEOUT_SECS: u64 = 5;

/// Default interval in milliseconds at which meter values are read.
const DEFAULT_METER_INTERVAL_MS: u64 = 200;

/// TLS name the server's certificate is issued for.
const SERVER_NAME: &str = "pq-meter-server";

/// Largest response body we read. The answer of the server is a few bytes.
const MAX_BODY_SIZE: usize = 4096;

/// Command line arguments.
#[derive(Debug, Parser)]
#[command(
    version,
    about = "Sends meter data to pq-meter-server over SCION HTTP/3"
)]
struct Args {
    /// IP address of the server (e.g. `192.168.1.42`), or full SCION address.
    #[arg(long, visible_alias = "server-ip")]
    server: Option<String>,

    /// IP address of the server (e.g. `192.168.1.42`), or full SCION address.
    #[arg(value_name = "SERVER_IP")]
    server_pos: Option<String>,

    /// URL of the endhost API this client attaches to. If omitted, it is derived
    /// from the server IP using port 31000 (e.g. `http://<server-ip>:31000`).
    #[arg(long)]
    endhost_api: Option<Url>,

    /// Path to POST to on the server.
    #[arg(long, default_value = "/edh/v1/hello")]
    path: String,

    /// The IP address of the Umg605Pro device.
    #[arg(long, visible_alias = "ip", default_value = "10.10.0.2")]
    meter_ip: IpAddr,

    /// The Modbus TCP port of the Umg605Pro device.
    #[arg(long, default_value_t = DEFAULT_MODBUS_PORT)]
    meter_port: u16,

    /// The Modbus unit id, i.e. the device address configured on the meter.
    #[arg(long, default_value_t = DEFAULT_MODBUS_UNIT)]
    meter_unit: u8,

    /// Timeout in seconds for connecting and for each register read.
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS)]
    meter_timeout: u64,

    /// Interval in milliseconds between meter readings.
    #[arg(long, visible_alias = "interval", visible_alias = "meter-interval", default_value_t = DEFAULT_METER_INTERVAL_MS)]
    meter_interval_ms: u64,

    /// Number of measurements to batch before sending (size trigger).
    #[arg(long, default_value_t = 10)]
    batch_size: usize,

    /// Maximum time to wait in milliseconds before sending a batch (time trigger).
    #[arg(long, visible_alias = "batch-time", visible_alias = "batch-timeout", default_value_t = 1000)]
    batch_timeout_ms: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();

    let server_raw = args
        .server
        .or(args.server_pos)
        .ok_or_else(|| anyhow::anyhow!("server IP address must be supplied (e.g. `pq-meter-client 192.168.1.42` or `--server 192.168.1.42`)"))?;

    let (server_scion, server_ip): (ScionSocketIpAddr, IpAddr) = if let Ok(ip) =
        server_raw.parse::<IpAddr>()
    {
        let scion_addr: ScionSocketIpAddr = format!("[{SERVER_AS},{ip}]:{SERVER_PORT}")
            .parse()
            .context("constructing SCION address from server IP")?;
        (scion_addr, ip)
    } else if let Ok(scion_addr) = server_raw.parse::<ScionSocketIpAddr>() {
        let ip = scion_addr.ip();
        (scion_addr, ip)
    } else {
        anyhow::bail!(
            "invalid server address: expected an IP address (e.g. 192.168.1.42) or SCION address, got '{server_raw}'"
        );
    };

    let endhost_api = match args.endhost_api {
        Some(url) => url,
        None => format!("http://{server_ip}:{GATEWAY_ENDHOST_API_PORT}/")
            .parse()
            .context("constructing endhost API URL from server IP")?,
    };

    // The SDK uses rustls for its control plane; pick a crypto backend.
    scion_sdk_utils::rustls::select_ring_crypto_provider();

    // One client per program: it holds the connection pool. Building it does no I/O, the
    // connection is established with the first request.
    let client = Client::new(
        Config::new(endhost_api)
            // A dummy token. Normally a client asks the AA (the authentication and
            // authorization service) for a SNAP token; here the AA is left out.
            .with_auth_token(snap_tokens::v0::dummy_snap_token())
            // The server uses a self-signed certificate, so its identity is not verified.
            .with_quic_config(QuicConfig::builder().verify_peer(false).build()),
    );

    let meter_socket_addr = SocketAddr::new(args.meter_ip, args.meter_port);
    let meter_timeout = Duration::from_secs(args.meter_timeout);
    let mut meter_client =
        Umg605ProClient::connect_tcp(meter_socket_addr, Slave(args.meter_unit), meter_timeout)
            .await?;

    let meter_interval = Duration::from_millis(args.meter_interval_ms.max(1));
    let batch_size = args.batch_size.max(1);
    let batch_timeout = Duration::from_millis(args.batch_timeout_ms.max(1));

    println!(
        "streaming meter data to {server_scion}{} (interval: {meter_interval:.2?}, batch size: {batch_size}, timeout: {batch_timeout:.2?}) ...",
        args.path
    );

    monitor(
        &mut meter_client,
        &client,
        &server_scion,
        &args.path,
        meter_interval,
        batch_size,
        batch_timeout,
    )
    .await?;

    client.close().await;

    Ok(())
}

/// Reads the meter every `period` and pushes batched readings to the server over SCION
/// when either the size trigger (`batch_size`) or time trigger (`batch_timeout`) fires.
///
/// `http` is reused across flushes, so only the first request pays for the QUIC handshake.
/// A failed request is reported and the loop continues: a gateway that gives up on the
/// first network hiccup is of no use in the field.
async fn monitor(
    meter: &mut Umg605ProClient,
    http: &Client,
    server: &ScionSocketIpAddr,
    path: &str,
    period: Duration,
    batch_size: usize,
    batch_timeout: Duration,
) -> anyhow::Result<()> {
    // The URL holds the server name and the port. `target` gives the SCION address the
    // packets go to, so the simulated network needs no DNS.
    let url = format!("https://{SERVER_NAME}:{}{}", server.port(), path);

    let mut interval = tokio::time::interval(period);
    let mut batch: Vec<serde_json::Value> = Vec::with_capacity(batch_size);
    let mut flush_deadline = tokio::time::Instant::now() + batch_timeout;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let start = Instant::now();

                let Snapshot {
                    systime,
                    frequency,
                    voltage_l1,
                    current_l1,
                    real_power_l1,
                    apparent_power_l1,
                    reactive_power_l1,
                    cos_phi_l1,
                    real_energy_consumed_l1,
                    thd_current_l1,
                } = meter.snapshot().await?;

                let read_elapsed = start.elapsed();

                let measurement = serde_json::json!({
                    // Device detection uses total_power; the server also validates and retains
                    // the latest timestamp, frequency, and L1 context.
                    "total_power": real_power_l1,
                    "systime": systime,
                    "frequency_hz": frequency,
                    "l1": {
                        "voltage_v": voltage_l1,
                        "current_a": current_l1,
                        "real_power_w": real_power_l1,
                        "apparent_power_va": apparent_power_l1,
                        "reactive_power_var": reactive_power_l1,
                        "cos_phi": cos_phi_l1,
                        "real_energy_consumed_wh": real_energy_consumed_l1,
                        "thd_current_pct": thd_current_l1,
                    }
                });

                if batch.is_empty() {
                    flush_deadline = tokio::time::Instant::now() + batch_timeout;
                }
                batch.push(measurement);

                println!(
                    "Time: {} | Freq: {:.2}Hz | L1 [U: {:.2}V, I: {:.2}A, P: {:.2}W, S: {:.2}VA, Q: {:.2}var, PF: {:.2}, Energy: {:.2}Wh, THD_I: {:.2}%] | Batch: {}/{}",
                    systime,
                    frequency,
                    voltage_l1,
                    current_l1,
                    real_power_l1,
                    apparent_power_l1,
                    reactive_power_l1,
                    cos_phi_l1,
                    real_energy_consumed_l1,
                    thd_current_l1,
                    batch.len(),
                    batch_size
                );

                if read_elapsed > period {
                    eprintln!(
                        "Warning: Reading took longer than the {:.2?} interval: {:.2?}",
                        period, read_elapsed
                    );
                }

                let should_flush_size = batch.len() >= batch_size;
                let should_flush_time = tokio::time::Instant::now() >= flush_deadline;

                if should_flush_size || should_flush_time {
                    let reason = if should_flush_size { "size trigger" } else { "time trigger" };
                    send_batch(http, &url, server, &mut batch, reason).await;
                    flush_deadline = tokio::time::Instant::now() + batch_timeout;
                }
            }
            _ = tokio::time::sleep_until(flush_deadline), if !batch.is_empty() => {
                send_batch(http, &url, server, &mut batch, "time trigger").await;
                flush_deadline = tokio::time::Instant::now() + batch_timeout;
            }
        }
    }
}

/// Sends buffered measurements to the server over SCION HTTP/3.
async fn send_batch(
    http: &Client,
    url: &str,
    server: &ScionSocketIpAddr,
    batch: &mut Vec<serde_json::Value>,
    reason: &str,
) {
    if batch.is_empty() {
        return;
    }

    let count = batch.len();
    let body = match serde_json::to_vec(batch) {
        Ok(b) => b,
        Err(err) => {
            eprintln!("Warning: encoding the measurement batch failed: {err}");
            batch.clear();
            return;
        }
    };
    batch.clear();

    let request = match Request::post(url)
        .header("content-type", "application/json")
        .target(server.host())
        .body(body)
        .build()
    {
        Ok(req) => req,
        Err(err) => {
            eprintln!("Warning: building the batch request failed: {err}");
            return;
        }
    };

    let send_start = Instant::now();
    match http.request(request).await {
        Ok(response) => {
            let status = response.status();
            let elapsed = send_start.elapsed();
            if !status.is_success() {
                eprintln!("Warning: server answered with {status} ({elapsed:.2?})");
            }
            match response.text(Some(MAX_BODY_SIZE)).await {
                Ok((text, _)) => {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        println!("Flushed {count} measurement(s) ({reason}) in {elapsed:.2?} -> server answered {status}: {trimmed}");
                    } else {
                        println!("Flushed {count} measurement(s) ({reason}) in {elapsed:.2?} -> server answered {status}");
                    }
                }
                Err(_) => {
                    println!("Flushed {count} measurement(s) ({reason}) in {elapsed:.2?} -> server answered {status}");
                }
            }
        }
        Err(error) => eprintln!("Warning: sending the batch failed: {error}"),
    }
}
