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

/// Absolute change thresholds for discarding noise during monitoring.
/// Values are calibrated against real-world idling meter measurements (see `log.txt`).
const THRESHOLD_FREQUENCY_HZ: f32 = 0.2;
const THRESHOLD_VOLTAGE_V: f32 = 1.5;
const THRESHOLD_CURRENT_A: f32 = 0.06;
const THRESHOLD_REAL_POWER_W: f32 = 4.0;
const THRESHOLD_APPARENT_POWER_VA: f32 = 8.0;
const THRESHOLD_REACTIVE_POWER_VAR: f32 = 3.0;
const THRESHOLD_COS_PHI: f32 = 0.08;
const THRESHOLD_THD_CURRENT_PCT: f32 = 15.0;

/// How long a changed set of readings must remain stable before establishing a new baseline.
const SETTLING_WINDOW: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug)]
struct BaselineReading {
    frequency: f32,
    voltage_l1: f32,
    current_l1: f32,
    real_power_l1: f32,
    apparent_power_l1: f32,
    reactive_power_l1: f32,
    cos_phi_l1: f32,
    #[allow(dead_code)]
    real_energy_consumed_l1: f32,
    thd_current_l1: f32,
}

impl BaselineReading {
    fn exceeds_threshold(&self, new: &BaselineReading) -> bool {
        (new.frequency - self.frequency).abs() > THRESHOLD_FREQUENCY_HZ
            || (new.voltage_l1 - self.voltage_l1).abs() > THRESHOLD_VOLTAGE_V
            || (new.current_l1 - self.current_l1).abs() > THRESHOLD_CURRENT_A
            || (new.real_power_l1 - self.real_power_l1).abs() > THRESHOLD_REAL_POWER_W
            || (new.apparent_power_l1 - self.apparent_power_l1).abs() > THRESHOLD_APPARENT_POWER_VA
            || (new.reactive_power_l1 - self.reactive_power_l1).abs() > THRESHOLD_REACTIVE_POWER_VAR
            || (new.cos_phi_l1 - self.cos_phi_l1).abs() > THRESHOLD_COS_PHI
            || (new.thd_current_l1 - self.thd_current_l1).abs() > THRESHOLD_THD_CURRENT_PCT
    }
}

#[derive(Clone, Copy, Debug)]
struct SettlingCandidate {
    reading: BaselineReading,
    first_seen: Instant,
}

struct SettledMonitor {
    settling_window: Duration,
    baseline: Option<BaselineReading>,
    candidate: Option<SettlingCandidate>,
}

impl SettledMonitor {
    fn new(settling_window: Duration) -> Self {
        Self {
            settling_window,
            baseline: None,
            candidate: None,
        }
    }

    /// Processes a new reading. Returns `true` if this reading should be printed
    /// (and committed as the new baseline).
    fn process_reading(&mut self, current: BaselineReading, now: Instant) -> bool {
        let Some(base) = self.baseline else {
            // First reading establishes the baseline immediately.
            self.baseline = Some(current);
            return true;
        };

        if !base.exceeds_threshold(&current) {
            // Within noise margin of current baseline: discard any transient candidate.
            self.candidate = None;
            return false;
        }

        // Reading exceeds baseline threshold: we are observing a potential state change.
        match self.candidate {
            Some(cand) if !cand.reading.exceeds_threshold(&current) => {
                // Reading is stable with respect to the candidate level.
                if now.checked_duration_since(cand.first_seen).unwrap_or_default()
                    >= self.settling_window
                {
                    self.baseline = Some(current);
                    self.candidate = None;
                    true
                } else {
                    // Still settling within the window.
                    false
                }
            }
            _ => {
                // First reading of a new candidate level, or level changed before settling.
                self.candidate = Some(SettlingCandidate {
                    reading: current,
                    first_seen: now,
                });
                false
            }
        }
    }
}

/// Reads the meter every `period` and pushes batched readings to the server over SCION
/// when either the size trigger (`batch_size`) or time trigger (`batch_timeout`) fires.
///
/// Output is printed to the console only when changes exceed the noise threshold
/// and remain settled for `SETTLING_WINDOW`.
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
    let mut settled_monitor = SettledMonitor::new(SETTLING_WINDOW);

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

                let current_reading = BaselineReading {
                    frequency,
                    voltage_l1,
                    current_l1,
                    real_power_l1,
                    apparent_power_l1,
                    reactive_power_l1,
                    cos_phi_l1,
                    real_energy_consumed_l1,
                    thd_current_l1,
                };

                if settled_monitor.process_reading(current_reading, start) {
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
                }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_positional_server_argument_with_defaults() {
        let args = Args::try_parse_from(["pq-meter-client", "192.168.1.42"]).unwrap();
        assert_eq!(args.server_pos.as_deref(), Some("192.168.1.42"));
        assert_eq!(args.server, None);
        assert_eq!(args.path, "/edh/v1/hello");
        assert_eq!(args.meter_port, DEFAULT_MODBUS_PORT);
        assert_eq!(args.meter_unit, DEFAULT_MODBUS_UNIT);
        assert_eq!(args.meter_timeout, DEFAULT_TIMEOUT_SECS);
        assert_eq!(args.meter_interval_ms, DEFAULT_METER_INTERVAL_MS);
        assert_eq!(args.batch_size, 10);
        assert_eq!(args.batch_timeout_ms, 1000);
    }

    #[test]
    fn parses_server_flag_and_aliases() {
        let args = Args::try_parse_from([
            "pq-meter-client",
            "--server",
            "10.0.0.1",
            "--interval",
            "50",
            "--batch-time",
            "500",
            "--batch-size",
            "25",
        ])
        .unwrap();
        assert_eq!(args.server.as_deref(), Some("10.0.0.1"));
        assert_eq!(args.meter_interval_ms, 50);
        assert_eq!(args.batch_timeout_ms, 500);
        assert_eq!(args.batch_size, 25);
    }

    #[test]
    fn server_ip_alias_works() {
        let args = Args::try_parse_from(["pq-meter-client", "--server-ip", "10.0.0.5"]).unwrap();
        assert_eq!(args.server.as_deref(), Some("10.0.0.5"));
    }

    #[test]
    fn rejects_missing_required_arguments_or_invalid_port() {
        assert!(Args::try_parse_from(["pq-meter-client", "--meter-port", "invalid"]).is_err());
    }

    #[test]
    fn derives_correct_scion_and_endhost_urls_from_ip() {
        let ip_str = "127.0.0.1";
        let ip: IpAddr = ip_str.parse().unwrap();
        let scion_addr: ScionSocketIpAddr =
            format!("[{SERVER_AS},{ip}]:{SERVER_PORT}").parse().unwrap();
        assert_eq!(scion_addr.ip(), ip);
        assert_eq!(scion_addr.port(), SERVER_PORT);

        let endhost_api: Url = format!("http://{ip}:{GATEWAY_ENDHOST_API_PORT}/")
            .parse()
            .unwrap();
        assert_eq!(endhost_api.as_str(), "http://127.0.0.1:31000/");
    }

    fn sample_reading() -> BaselineReading {
        BaselineReading {
            frequency: 50.0,
            voltage_l1: 230.0,
            current_l1: 1.0,
            real_power_l1: 230.0,
            apparent_power_l1: 230.0,
            reactive_power_l1: 0.0,
            cos_phi_l1: 1.0,
            real_energy_consumed_l1: 100.0,
            thd_current_l1: 1.5,
        }
    }

    #[test]
    fn ignores_small_noise_fluctuations() {
        let baseline = sample_reading();
        let noisy = BaselineReading {
            frequency: 50.05,              // delta 0.05 <= 0.2
            voltage_l1: 230.4,             // delta 0.4 <= 1.0
            current_l1: 1.02,              // delta 0.02 <= 0.05
            real_power_l1: 231.5,          // delta 1.5 <= 3.0
            apparent_power_l1: 232.0,      // delta 2.0 <= 5.0
            reactive_power_l1: 0.8,        // delta 0.8 <= 2.0
            cos_phi_l1: 0.98,              // delta 0.02 <= 0.05
            real_energy_consumed_l1: 105.0, // ignored for state changes
            thd_current_l1: 4.5,           // delta 3.0 <= 10.0
        };
        assert!(!baseline.exceeds_threshold(&noisy));
    }

    #[test]
    fn discards_real_world_noise_from_log() {
        // Initial baseline reading from log.txt
        let baseline = BaselineReading {
            frequency: 49.99,
            voltage_l1: 238.43,
            current_l1: 0.29,
            real_power_l1: 23.38,
            apparent_power_l1: 68.38,
            reactive_power_l1: -34.37,
            cos_phi_l1: 0.56,
            real_energy_consumed_l1: 322.77,
            thd_current_l1: 130.81,
        };

        // Extremes observed in log.txt (lowest voltage, power, apparent power, THD)
        let extreme_reading = BaselineReading {
            frequency: 49.99,
            voltage_l1: 238.26,
            current_l1: 0.28,
            real_power_l1: 22.39,
            apparent_power_l1: 66.10,
            reactive_power_l1: -34.57,
            cos_phi_l1: 0.54,
            real_energy_consumed_l1: 322.81,
            thd_current_l1: 125.79,
        };

        assert!(!baseline.exceeds_threshold(&extreme_reading));
    }

    #[test]
    fn discards_extended_idling_noise() {
        let reading_a = BaselineReading {
            frequency: 49.95,
            voltage_l1: 238.59,
            current_l1: 0.29,
            real_power_l1: 23.21,
            apparent_power_l1: 68.08,
            reactive_power_l1: -34.36,
            cos_phi_l1: 0.56,
            real_energy_consumed_l1: 323.94,
            thd_current_l1: 130.45,
        };

        let reading_b = BaselineReading {
            frequency: 49.95,
            voltage_l1: 238.73,
            current_l1: 0.27,
            real_power_l1: 21.66,
            apparent_power_l1: 64.01,
            reactive_power_l1: -34.99,
            cos_phi_l1: 0.52,
            real_energy_consumed_l1: 324.06,
            thd_current_l1: 119.26,
        };

        assert!(!reading_a.exceeds_threshold(&reading_b));
    }

    #[test]
    fn triggers_when_any_metric_exceeds_threshold() {
        let baseline = sample_reading();

        let mut reading = baseline;
        reading.voltage_l1 += 2.0; // > 1.5
        assert!(baseline.exceeds_threshold(&reading));

        let mut reading = baseline;
        reading.current_l1 += 0.10; // > 0.06
        assert!(baseline.exceeds_threshold(&reading));

        let mut reading = baseline;
        reading.frequency -= 0.30; // > 0.2
        assert!(baseline.exceeds_threshold(&reading));

        let mut reading = baseline;
        reading.real_power_l1 += 5.0; // > 4.0
        assert!(baseline.exceeds_threshold(&reading));

        let mut reading = baseline;
        reading.apparent_power_l1 += 9.0; // > 8.0
        assert!(baseline.exceeds_threshold(&reading));

        let mut reading = baseline;
        reading.reactive_power_l1 += 3.5; // > 3.0
        assert!(baseline.exceeds_threshold(&reading));

        let mut reading = baseline;
        reading.cos_phi_l1 -= 0.10; // > 0.08
        assert!(baseline.exceeds_threshold(&reading));

        let mut reading = baseline;
        reading.thd_current_l1 += 16.0; // > 15.0
        assert!(baseline.exceeds_threshold(&reading));
    }

    #[test]
    fn new_baseline_discards_subsequent_noise() {
        let mut baseline = sample_reading();
        let mut new_state = baseline;
        new_state.voltage_l1 = 235.0; // significant jump > 1.5V

        assert!(baseline.exceeds_threshold(&new_state));
        baseline = new_state;

        let small_fluctuation = BaselineReading {
            voltage_l1: 235.4, // delta 0.4 from new baseline 235.0 <= 1.5
            ..new_state
        };
        assert!(!baseline.exceeds_threshold(&small_fluctuation));
    }

    #[test]
    fn settled_monitor_establishes_initial_baseline_immediately() {
        let mut monitor = SettledMonitor::new(Duration::from_secs(2));
        let t0 = Instant::now();
        let reading = sample_reading();

        assert!(monitor.process_reading(reading, t0));
    }

    #[test]
    fn settled_monitor_ignores_transient_spike_shorter_than_window() {
        let mut monitor = SettledMonitor::new(Duration::from_secs(2));
        let t0 = Instant::now();
        let baseline = sample_reading();
        assert!(monitor.process_reading(baseline, t0));

        let mut spike = baseline;
        spike.real_power_l1 += 40.0;

        // Spike appears at t0 + 1s (new candidate)
        assert!(!monitor.process_reading(spike, t0 + Duration::from_secs(1)));

        // Still at spike level at t0 + 2s (1s of settling, < 2s window)
        assert!(!monitor.process_reading(spike, t0 + Duration::from_secs(2)));

        // Drops back to baseline at t0 + 2.5s (< 2s after spike started)
        assert!(!monitor.process_reading(baseline, t0 + Duration::from_millis(2500)));

        // Still at baseline at t0 + 5s: never triggered a state change
        assert!(!monitor.process_reading(baseline, t0 + Duration::from_secs(5)));
    }

    #[test]
    fn settled_monitor_commits_change_after_two_seconds_stable() {
        let mut monitor = SettledMonitor::new(Duration::from_secs(2));
        let t0 = Instant::now();
        let baseline = sample_reading();
        assert!(monitor.process_reading(baseline, t0));

        let mut higher = baseline;
        higher.real_power_l1 += 40.0;

        // Jumps to higher power at t0 + 1s
        assert!(!monitor.process_reading(higher, t0 + Duration::from_secs(1)));

        // 1.5s after jump (t0 + 2.5s): still settling (< 2s)
        assert!(!monitor.process_reading(higher, t0 + Duration::from_millis(2500)));

        // 2.0s after jump (t0 + 3.0s): settled! Returns true and commits new baseline
        assert!(monitor.process_reading(higher, t0 + Duration::from_secs(3)));

        // Subsequent readings at the new baseline are ignored as steady state
        assert!(!monitor.process_reading(higher, t0 + Duration::from_millis(3200)));
    }

    #[test]
    fn settled_monitor_collapses_rapid_intermediate_steps() {
        let mut monitor = SettledMonitor::new(Duration::from_secs(2));
        let t0 = Instant::now();
        let baseline = sample_reading();
        assert!(monitor.process_reading(baseline, t0));

        // Rapid USB-PD steps: 30W -> 45W -> 28W -> 65W within 1 second
        let mut r1 = baseline;
        r1.real_power_l1 = 30.0;
        let mut r2 = baseline;
        r2.real_power_l1 = 45.0;
        let mut r3 = baseline;
        r3.real_power_l1 = 28.0;
        let mut r4 = baseline;
        r4.real_power_l1 = 65.0;

        assert!(!monitor.process_reading(r1, t0 + Duration::from_millis(200)));
        assert!(!monitor.process_reading(r2, t0 + Duration::from_millis(500)));
        assert!(!monitor.process_reading(r3, t0 + Duration::from_millis(800)));
        assert!(!monitor.process_reading(r4, t0 + Duration::from_millis(1000)));

        // Stable at 65W until 2 seconds have passed since t0 + 1000ms (i.e. t0 + 3000ms)
        assert!(!monitor.process_reading(r4, t0 + Duration::from_millis(2500)));
        assert!(monitor.process_reading(r4, t0 + Duration::from_millis(3000)));
    }
}
