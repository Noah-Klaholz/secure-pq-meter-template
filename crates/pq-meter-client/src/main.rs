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

mod link;

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::Parser;
use scion_http3::{Client, Config, Request, scion_quic::quic::config::QuicConfig};
use sciparse::address::ip_socket_addr::ScionSocketIpAddr;
use tokio_modbus::Slave;
use umg605_modbus_client::{DEFAULT_MODBUS_PORT, PHASE_COUNT, Phases, Snapshot, Umg605ProClient};
use url::Url;

use crate::link::{
    DeliveryStats, HEADER_ACK_LATENCY, HEADER_FAILOVERS, HEADER_PATH, HEADER_QUEUED, ScionLink,
};

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
    #[arg(
        long,
        visible_alias = "batch-time",
        visible_alias = "batch-timeout",
        default_value_t = 1000
    )]
    batch_timeout_ms: u64,

    /// Longest gap in milliseconds between recorded readings while nothing changes.
    ///
    /// The threshold filter exists to suppress noise, but a power-quality dashboard needs a
    /// continuous trend and a link that keeps proving it is alive. This records a reading
    /// even when nothing moved, so voltage and frequency stay plotted through an idle
    /// installation. Set to 0 to record only threshold crossings.
    #[arg(long, visible_alias = "heartbeat", default_value_t = 2000)]
    heartbeat_ms: u64,
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

    let endhost_api_for_paths = endhost_api.clone();

    // One client per program: it holds the connection pool. Building it does no I/O, the
    // connection is established with the first request.
    let client = Client::new(
        Config::new(endhost_api)
            // TODO(security): development credential, not a real one. A gateway should ask
            // the AA (the authentication and authorization service) for a SNAP token that
            // identifies *this* device, so the network can refuse an unknown one before its
            // packets reach the application. The dummy token identifies nobody.
            .with_auth_token(snap_tokens::v0::dummy_snap_token())
            // TODO(security): the connection is encrypted but the peer is unauthenticated.
            // `verify_peer(false)` accepts any certificate, so anything that can answer on
            // the address can impersonate the receiver and collect the meter data. Pin the
            // backend certificate, or verify against a CA the gateway is provisioned with.
            .with_quic_config(QuicConfig::builder().verify_peer(false).build()),
    );

    // A second, read-only view of the network, used only to report which path is in use.
    // The gateway's job is delivering readings, so failing to attach is a warning, not an
    // error: it costs the transport panel on the dashboard and nothing else.
    let server_as: sciparse::identifier::isd_asn::IsdAsn = SERVER_AS
        .parse()
        .context("parsing the SCION AS of the server")?;
    let mut scion_link = match ScionLink::attach(endhost_api_for_paths, server_as).await {
        Ok(link) => Some(link),
        Err(error) => {
            eprintln!("Warning: cannot report the SCION path in use: {error}");
            None
        }
    };

    let meter_socket_addr = SocketAddr::new(args.meter_ip, args.meter_port);
    let meter_timeout = Duration::from_secs(args.meter_timeout);
    let mut meter_client =
        Umg605ProClient::connect_tcp(meter_socket_addr, Slave(args.meter_unit), meter_timeout)
            .await?;

    let meter_interval = Duration::from_millis(args.meter_interval_ms.max(1));
    let batch_size = args.batch_size.max(1);
    let batch_timeout = Duration::from_millis(args.batch_timeout_ms.max(1));
    let heartbeat = (args.heartbeat_ms > 0).then(|| Duration::from_millis(args.heartbeat_ms));

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
        heartbeat,
        &mut scion_link,
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
const THRESHOLD_THD_VOLTAGE_PCT: f32 = 1.0;
const THRESHOLD_THD_CURRENT_PCT: f32 = 15.0;

/// The thresholds above hold for a single phase. A three-phase sum carries the noise of
/// all three, so its threshold is the per-phase one for every phase that feeds into it.
const THRESHOLD_REAL_POWER_SUM_W: f32 = PHASE_COUNT as f32 * THRESHOLD_REAL_POWER_W;
const THRESHOLD_APPARENT_POWER_SUM_VA: f32 = PHASE_COUNT as f32 * THRESHOLD_APPARENT_POWER_VA;
const THRESHOLD_REACTIVE_POWER_SUM_VAR: f32 = PHASE_COUNT as f32 * THRESHOLD_REACTIVE_POWER_VAR;

/// How long a changed set of readings must remain stable before establishing a new baseline.
const SETTLING_WINDOW: Duration = Duration::from_secs(2);

/// The values whose movement makes a reading worth sending, taken from one meter snapshot.
///
/// The energy counters are deliberately left out: they only ever climb, so including them
/// would make every single reading look like a state change.
#[derive(Clone, Copy, Debug)]
struct BaselineReading {
    frequency: f32,
    voltage: Phases,
    current: Phases,
    real_power: Phases,
    real_power_sum3: f32,
    apparent_power_sum3: f32,
    reactive_power_sum3: f32,
    cos_phi: Phases,
    thd_voltage: Phases,
    thd_current: Phases,
}

impl BaselineReading {
    fn from_snapshot(snapshot: &Snapshot) -> Self {
        Self {
            frequency: snapshot.frequency,
            voltage: snapshot.voltage,
            current: snapshot.current,
            real_power: snapshot.real_power,
            real_power_sum3: snapshot.real_power_sum3,
            apparent_power_sum3: snapshot.apparent_power_sum3,
            reactive_power_sum3: snapshot.reactive_power_sum3,
            cos_phi: snapshot.cos_phi,
            thd_voltage: snapshot.thd_voltage,
            thd_current: snapshot.thd_current,
        }
    }

    fn exceeds_threshold(&self, new: &BaselineReading) -> bool {
        exceeds(self.frequency, new.frequency, THRESHOLD_FREQUENCY_HZ)
            || exceeds(
                self.real_power_sum3,
                new.real_power_sum3,
                THRESHOLD_REAL_POWER_SUM_W,
            )
            || exceeds(
                self.apparent_power_sum3,
                new.apparent_power_sum3,
                THRESHOLD_APPARENT_POWER_SUM_VA,
            )
            || exceeds(
                self.reactive_power_sum3,
                new.reactive_power_sum3,
                THRESHOLD_REACTIVE_POWER_SUM_VAR,
            )
            || exceeds_any_phase(self.voltage, new.voltage, THRESHOLD_VOLTAGE_V)
            || exceeds_any_phase(self.current, new.current, THRESHOLD_CURRENT_A)
            || exceeds_any_phase(self.real_power, new.real_power, THRESHOLD_REAL_POWER_W)
            || exceeds_any_phase(self.cos_phi, new.cos_phi, THRESHOLD_COS_PHI)
            || exceeds_any_phase(self.thd_voltage, new.thd_voltage, THRESHOLD_THD_VOLTAGE_PCT)
            || exceeds_any_phase(self.thd_current, new.thd_current, THRESHOLD_THD_CURRENT_PCT)
    }
}

/// Whether a value moved far enough from `old` to count as a change.
///
/// A comparison against a value the meter could not determine is never a change: NaN
/// compares false either way, which is what a meter wired up on one phase needs — the
/// harmonic distortion of the two phases with no current must not keep the gateway busy.
fn exceeds(old: f32, new: f32, threshold: f32) -> bool {
    (new - old).abs() > threshold
}

/// Whether any single phase of a quantity moved far enough to count as a change.
fn exceeds_any_phase(old: Phases, new: Phases, threshold: f32) -> bool {
    old.into_iter()
        .zip(new)
        .any(|(old, new)| exceeds(old, new, threshold))
}

/// A value the meter could determine, or `None` for one it could not.
///
/// The meter answers with NaN for quantities that do not exist in its current wiring, and
/// JSON has no way to spell that; the server reads `null` as "not available".
fn measured(value: f32) -> Option<f32> {
    value.is_finite().then_some(value)
}

/// The measured values of one phase, as sent to the server.
fn phase_json(snapshot: &Snapshot, phase: usize) -> serde_json::Value {
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
}

/// One complete three-phase measurement, as sent to the server.
///
/// `heartbeat` marks a reading sent only to keep the trend and the link alive. The receiver
/// keeps it as a measurement but leaves it out of device inference: while a level is still
/// settling this value can sit anywhere between the old level and the new one.
fn measurement_json(snapshot: &Snapshot, total_power: f32, heartbeat: bool) -> serde_json::Value {
    serde_json::json!({
        // Device detection uses total_power; the server also validates and retains the
        // timestamp, the frequency, and the per-phase context.
        "total_power": total_power,
        "systime": snapshot.systime,
        "frequency_hz": measured(snapshot.frequency),
        "l1": phase_json(snapshot, 0),
        "l2": phase_json(snapshot, 1),
        "l3": phase_json(snapshot, 2),
        // The sums the meter measures itself, rather than sums derived from the phases.
        "totals": {
            "real_power_w": measured(snapshot.real_power_sum3),
            "apparent_power_va": measured(snapshot.apparent_power_sum3),
            "reactive_power_var": measured(snapshot.reactive_power_sum3),
        },
        "heartbeat": heartbeat,
    })
}

/// Formats a measured value, or `n/a` for one the meter could not determine.
fn show(value: f32, decimals: usize) -> String {
    match measured(value) {
        Some(value) => format!("{value:.decimals$}"),
        None => "n/a".to_owned(),
    }
}

/// The one-line summary printed for every recorded reading: three-phase sums, then phases.
fn summary(snapshot: &Snapshot) -> String {
    let phases: Vec<String> = (0..PHASE_COUNT)
        .map(|phase| {
            format!(
                "L{} [U: {}V, I: {}A, P: {}W, PF: {}, THD_U: {}%, THD_I: {}%]",
                phase + 1,
                show(snapshot.voltage[phase], 2),
                show(snapshot.current[phase], 2),
                show(snapshot.real_power[phase], 2),
                show(snapshot.cos_phi[phase], 2),
                show(snapshot.thd_voltage[phase], 2),
                show(snapshot.thd_current[phase], 2),
            )
        })
        .collect();

    format!(
        "Time: {} | Freq: {}Hz | Sum3 [P: {}W, S: {}VA, Q: {}var] | {}",
        snapshot.systime,
        show(snapshot.frequency, 2),
        show(snapshot.real_power_sum3, 2),
        show(snapshot.apparent_power_sum3, 2),
        show(snapshot.reactive_power_sum3, 2),
        phases.join(" | "),
    )
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

    /// Processes a new reading. Returns `true` if this reading should be recorded
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
                if now
                    .checked_duration_since(cand.first_seen)
                    .unwrap_or_default()
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

/// Whether this reading is worth recording and sending.
///
/// A settled change always is. So is the occasional reading while nothing changes: the
/// receiver plots a trend from what arrives and judges the link by when it last heard
/// anything, and total silence is indistinguishable from a gateway that has fallen over.
/// Without a heartbeat an idle installation would produce an empty chart and a link that
/// reports itself stale while it is in fact healthy.
fn should_record(
    changed: bool,
    heartbeat: Option<Duration>,
    last_recorded: Option<Instant>,
    now: Instant,
) -> bool {
    if changed {
        return true;
    }
    heartbeat.is_some_and(|interval| {
        last_recorded
            .is_none_or(|last| now.checked_duration_since(last).unwrap_or_default() >= interval)
    })
}

/// Reads the meter every `period` and pushes batched readings to the server over SCION
/// when either the size trigger (`batch_size`) or time trigger (`batch_timeout`) fires.
///
/// Readings are recorded and printed only when changes exceed the noise threshold
/// and remain settled for `SETTLING_WINDOW`.
#[allow(clippy::too_many_arguments)]
async fn monitor(
    meter: &mut Umg605ProClient,
    http: &Client,
    server: &ScionSocketIpAddr,
    path: &str,
    period: Duration,
    batch_size: usize,
    batch_timeout: Duration,
    heartbeat: Option<Duration>,
    scion_link: &mut Option<ScionLink>,
) -> anyhow::Result<()> {
    // The URL holds the server name and the port. `target` gives the SCION address the
    // packets go to, so the simulated network needs no DNS.
    let url = format!("https://{SERVER_NAME}:{}{}", server.port(), path);

    let mut interval = tokio::time::interval(period);
    let mut batch: Vec<serde_json::Value> = Vec::with_capacity(batch_size);
    let mut flush_deadline = tokio::time::Instant::now() + batch_timeout;
    let mut settled_monitor = SettledMonitor::new(SETTLING_WINDOW);
    let mut delivery = DeliveryStats::default();
    let mut last_recorded: Option<Instant> = None;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let start = Instant::now();

                let snapshot = meter.snapshot().await?;

                let read_elapsed = start.elapsed();

                // The meter measures the three-phase sum itself. It is signed: a grid
                // connection that exports more than it draws reports negative real power.
                match measured(snapshot.real_power_sum3) {
                    None => eprintln!(
                        "Warning: the meter reported no three-phase real power, skipping this reading"
                    ),
                    Some(total_power) => {
                        let current_reading = BaselineReading::from_snapshot(&snapshot);

                        let changed = settled_monitor.process_reading(current_reading, start);
                        if should_record(changed, heartbeat, last_recorded, start) {
                            last_recorded = Some(start);
                            if batch.is_empty() {
                                flush_deadline = tokio::time::Instant::now() + batch_timeout;
                            }
                            batch.push(measurement_json(&snapshot, total_power, !changed));

                            println!(
                                "{} | Batch: {}/{}{}",
                                summary(&snapshot),
                                batch.len(),
                                batch_size,
                                if changed { "" } else { " (heartbeat)" }
                            );
                        }
                    }
                }

                if read_elapsed > period {
                    eprintln!(
                        "Warning: Reading took longer than the {:.2?} interval: {:.2?}",
                        period, read_elapsed
                    );
                }

                let should_flush_size = batch.len() >= batch_size;
                let should_flush_time =
                    !batch.is_empty() && tokio::time::Instant::now() >= flush_deadline;

                if should_flush_size || should_flush_time {
                    let reason = if should_flush_size { "size trigger" } else { "time trigger" };
                    if let Some(link) = scion_link.as_mut() {
                        link.refresh().await;
                    }
                    send_batch(
                        http,
                        &url,
                        server,
                        &mut batch,
                        reason,
                        scion_link.as_ref(),
                        &mut delivery,
                    )
                    .await;
                    flush_deadline = tokio::time::Instant::now() + batch_timeout;
                }
            }
            _ = tokio::time::sleep_until(flush_deadline), if !batch.is_empty() => {
                if let Some(link) = scion_link.as_mut() {
                    link.refresh().await;
                }
                send_batch(
                    http,
                    &url,
                    server,
                    &mut batch,
                    "time trigger",
                    scion_link.as_ref(),
                    &mut delivery,
                )
                .await;
                flush_deadline = tokio::time::Instant::now() + batch_timeout;
            }
        }
    }
}

/// Sends buffered measurements to the server over SCION HTTP/3.
///
/// The gateway's view of the link rides along as headers: which path it is using, how many
/// readings were waiting, how long the previous acknowledgement took, and how often the
/// path has changed. That keeps the measurement body exactly as the receiver documents it.
async fn send_batch(
    http: &Client,
    url: &str,
    server: &ScionSocketIpAddr,
    batch: &mut Vec<serde_json::Value>,
    reason: &str,
    link: Option<&ScionLink>,
    delivery: &mut DeliveryStats,
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

    // These readings are in flight until the server answers.
    delivery.queued_readings = count;

    let mut builder = Request::post(url)
        .header("content-type", "application/json")
        .header(HEADER_QUEUED, delivery.queued_readings.to_string());
    if let Some(latency) = delivery.last_ack_latency_ms {
        builder = builder.header(HEADER_ACK_LATENCY, format!("{latency:.1}"));
    }
    if let Some(link) = link {
        builder = builder.header(HEADER_FAILOVERS, link.failover_count().to_string());
        if let Some(path) = link.current_path() {
            builder = builder.header(HEADER_PATH, path);
        }
    }

    let request = match builder.target(server.host()).body(body).build() {
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
            // Acknowledged: nothing is waiting here any more, and the next batch can say
            // how long this one took.
            delivery.queued_readings = 0;
            delivery.last_ack_latency_ms = Some(elapsed.as_secs_f32() * 1000.0);
            if !status.is_success() {
                eprintln!("Warning: server answered with {status} ({elapsed:.2?})");
            }
            match response.text(Some(MAX_BODY_SIZE)).await {
                Ok((text, _)) => {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        println!(
                            "Flushed {count} measurement(s) ({reason}) in {elapsed:.2?} -> server answered {status}: {trimmed}"
                        );
                    } else {
                        println!(
                            "Flushed {count} measurement(s) ({reason}) in {elapsed:.2?} -> server answered {status}"
                        );
                    }
                }
                Err(_) => {
                    println!(
                        "Flushed {count} measurement(s) ({reason}) in {elapsed:.2?} -> server answered {status}"
                    );
                }
            }
        }
        Err(error) => eprintln!("Warning: sending the batch failed: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    /// The lab setup: the meter is wired up on L1 only, so the other two phases carry no
    /// voltage or current, and the meter reports the harmonic distortion of a current that
    /// is not there as NaN.
    fn sample_reading() -> BaselineReading {
        BaselineReading {
            frequency: 50.0,
            voltage: [230.0, 0.0, 0.0],
            current: [1.0, 0.0, 0.0],
            real_power: [230.0, 0.0, 0.0],
            real_power_sum3: 230.0,
            apparent_power_sum3: 230.0,
            reactive_power_sum3: 0.0,
            cos_phi: [1.0, 1.0, 1.0],
            thd_voltage: [1.5, f32::NAN, f32::NAN],
            thd_current: [1.5, f32::NAN, f32::NAN],
        }
    }

    /// Moves the load on L1, keeping the three-phase sum consistent with the phases.
    fn with_real_power(reading: BaselineReading, watts: f32) -> BaselineReading {
        BaselineReading {
            real_power: [watts, reading.real_power[1], reading.real_power[2]],
            real_power_sum3: watts + reading.real_power[1] + reading.real_power[2],
            ..reading
        }
    }

    #[test]
    fn ignores_small_noise_fluctuations() {
        let baseline = sample_reading();
        let noisy = BaselineReading {
            frequency: 50.05,                       // delta 0.05 <= 0.2
            voltage: [230.4, 0.0, 0.0],             // delta 0.4 <= 1.5
            current: [1.02, 0.0, 0.0],              // delta 0.02 <= 0.06
            real_power: [231.5, 0.0, 0.0],          // delta 1.5 <= 4.0
            real_power_sum3: 231.5,                 // delta 1.5 <= 12.0
            apparent_power_sum3: 232.0,             // delta 2.0 <= 24.0
            reactive_power_sum3: 0.8,               // delta 0.8 <= 9.0
            cos_phi: [0.98, 1.0, 1.0],              // delta 0.02 <= 0.08
            thd_voltage: [1.9, f32::NAN, f32::NAN], // delta 0.4 <= 1.0
            thd_current: [4.5, f32::NAN, f32::NAN], // delta 3.0 <= 15.0
        };
        assert!(!baseline.exceeds_threshold(&noisy));
    }

    #[test]
    fn discards_real_world_noise_from_log() {
        // Initial baseline reading from log.txt
        let baseline = BaselineReading {
            frequency: 49.99,
            voltage: [238.43, 0.0, 0.0],
            current: [0.29, 0.0, 0.0],
            real_power: [23.38, 0.0, 0.0],
            real_power_sum3: 23.38,
            apparent_power_sum3: 68.38,
            reactive_power_sum3: -34.37,
            cos_phi: [0.56, 1.0, 1.0],
            thd_voltage: [1.85, f32::NAN, f32::NAN],
            thd_current: [130.81, f32::NAN, f32::NAN],
        };

        // Extremes observed in log.txt (lowest voltage, power, apparent power, THD)
        let extreme_reading = BaselineReading {
            frequency: 49.99,
            voltage: [238.26, 0.0, 0.0],
            current: [0.28, 0.0, 0.0],
            real_power: [22.39, 0.0, 0.0],
            real_power_sum3: 22.39,
            apparent_power_sum3: 66.10,
            reactive_power_sum3: -34.57,
            cos_phi: [0.54, 1.0, 1.0],
            thd_voltage: [1.88, f32::NAN, f32::NAN],
            thd_current: [125.79, f32::NAN, f32::NAN],
        };

        assert!(!baseline.exceeds_threshold(&extreme_reading));
    }

    #[test]
    fn discards_extended_idling_noise() {
        let reading_a = BaselineReading {
            frequency: 49.95,
            voltage: [238.59, 0.0, 0.0],
            current: [0.29, 0.0, 0.0],
            real_power: [23.21, 0.0, 0.0],
            real_power_sum3: 23.21,
            apparent_power_sum3: 68.08,
            reactive_power_sum3: -34.36,
            cos_phi: [0.56, 1.0, 1.0],
            thd_voltage: [1.84, f32::NAN, f32::NAN],
            thd_current: [130.45, f32::NAN, f32::NAN],
        };

        let reading_b = BaselineReading {
            frequency: 49.95,
            voltage: [238.73, 0.0, 0.0],
            current: [0.27, 0.0, 0.0],
            real_power: [21.66, 0.0, 0.0],
            real_power_sum3: 21.66,
            apparent_power_sum3: 64.01,
            reactive_power_sum3: -34.99,
            cos_phi: [0.52, 1.0, 1.0],
            thd_voltage: [1.90, f32::NAN, f32::NAN],
            thd_current: [119.26, f32::NAN, f32::NAN],
        };

        assert!(!reading_a.exceeds_threshold(&reading_b));
    }

    #[test]
    fn triggers_when_any_metric_exceeds_threshold() {
        let baseline = sample_reading();

        let mut reading = baseline;
        reading.voltage[0] += 2.0; // > 1.5
        assert!(baseline.exceeds_threshold(&reading));

        let mut reading = baseline;
        reading.current[0] += 0.10; // > 0.06
        assert!(baseline.exceeds_threshold(&reading));

        let mut reading = baseline;
        reading.frequency -= 0.30; // > 0.2
        assert!(baseline.exceeds_threshold(&reading));

        let mut reading = baseline;
        reading.real_power[0] += 5.0; // > 4.0
        assert!(baseline.exceeds_threshold(&reading));

        let mut reading = baseline;
        reading.real_power_sum3 += 13.0; // > 3 * 4.0
        assert!(baseline.exceeds_threshold(&reading));

        let mut reading = baseline;
        reading.apparent_power_sum3 += 25.0; // > 3 * 8.0
        assert!(baseline.exceeds_threshold(&reading));

        let mut reading = baseline;
        reading.reactive_power_sum3 += 10.0; // > 3 * 3.0
        assert!(baseline.exceeds_threshold(&reading));

        let mut reading = baseline;
        reading.cos_phi[0] -= 0.10; // > 0.08
        assert!(baseline.exceeds_threshold(&reading));

        let mut reading = baseline;
        reading.thd_voltage[0] += 1.5; // > 1.0
        assert!(baseline.exceeds_threshold(&reading));

        let mut reading = baseline;
        reading.thd_current[0] += 16.0; // > 15.0
        assert!(baseline.exceeds_threshold(&reading));
    }

    #[test]
    fn triggers_on_a_change_in_any_phase() {
        let baseline = sample_reading();

        // A load appearing on a phase that carried nothing has to be reported, whichever
        // phase it is. Watching L1 alone would miss two thirds of the installation.
        for phase in 1..PHASE_COUNT {
            let mut reading = baseline;
            reading.voltage[phase] = 230.0;
            reading.current[phase] = 1.0;
            reading.real_power[phase] = 230.0;
            assert!(
                baseline.exceeds_threshold(&reading),
                "a load on L{} must count as a change",
                phase + 1
            );
        }
    }

    #[test]
    fn values_the_meter_cannot_determine_never_count_as_a_change() {
        // The distortion of the two unwired phases is NaN in every reading. Comparing an
        // unknown value against an unknown value must stay quiet rather than push a
        // reading every 200 ms.
        let baseline = sample_reading();
        assert!(!baseline.exceeds_threshold(&sample_reading()));

        // The same holds when a value becomes available, or stops being available: neither
        // is a measured movement of the value itself.
        let mut appeared = baseline;
        appeared.thd_current[1] = 42.0;
        assert!(!baseline.exceeds_threshold(&appeared));
        assert!(!appeared.exceeds_threshold(&baseline));
    }

    #[test]
    fn new_baseline_discards_subsequent_noise() {
        let mut baseline = sample_reading();
        let mut new_state = baseline;
        new_state.voltage[0] = 235.0; // significant jump > 1.5V

        assert!(baseline.exceeds_threshold(&new_state));
        baseline = new_state;

        let mut small_fluctuation = new_state;
        small_fluctuation.voltage[0] = 235.4; // delta 0.4 from new baseline 235.0 <= 1.5
        assert!(!baseline.exceeds_threshold(&small_fluctuation));
    }

    /// Compares phases bit for bit, so that a NaN the meter reported counts as the same
    /// value it was given rather than as a difference.
    fn same_phases(left: Phases, right: Phases) -> bool {
        left.into_iter()
            .zip(right)
            .all(|(left, right)| left.to_bits() == right.to_bits())
    }

    #[test]
    fn baseline_reading_takes_every_phase_from_the_snapshot() {
        let snapshot = sample_snapshot();
        let reading = BaselineReading::from_snapshot(&snapshot);

        assert_eq!(reading.frequency, snapshot.frequency);
        assert!(same_phases(reading.voltage, snapshot.voltage));
        assert!(same_phases(reading.current, snapshot.current));
        assert!(same_phases(reading.real_power, snapshot.real_power));
        assert_eq!(reading.real_power_sum3, snapshot.real_power_sum3);
        assert_eq!(reading.apparent_power_sum3, snapshot.apparent_power_sum3);
        assert_eq!(reading.reactive_power_sum3, snapshot.reactive_power_sum3);
        assert!(same_phases(reading.cos_phi, snapshot.cos_phi));
        assert!(same_phases(reading.thd_voltage, snapshot.thd_voltage));
        assert!(same_phases(reading.thd_current, snapshot.thd_current));
    }

    /// A snapshot shaped like the one the meter in the lab returns: L1 loaded, L2 and L3
    /// unconnected, and NaN wherever the meter has nothing to measure.
    fn sample_snapshot() -> Snapshot {
        Snapshot {
            systime: 1_789_068_334,
            frequency: 49.97,
            voltage: [241.66, 0.0, 0.0],
            current: [0.297, 0.0, 0.0],
            real_power: [25.6, 0.0, 0.0],
            real_power_sum3: 25.6,
            apparent_power: [71.7, 0.0, 0.0],
            apparent_power_sum3: 71.7,
            reactive_power: [-36.59, 0.0, 0.0],
            reactive_power_sum3: -36.59,
            cos_phi: [0.57, 1.0, 1.0],
            real_energy_consumed: [439.93, 0.01, 0.01],
            thd_voltage: [1.85, f32::NAN, f32::NAN],
            thd_current: [125.42, f32::NAN, f32::NAN],
        }
    }

    #[test]
    fn measurement_carries_all_three_phases_and_the_measured_sums() {
        let snapshot = sample_snapshot();
        let measurement = measurement_json(&snapshot, snapshot.real_power_sum3, false);

        // The total is the meter's own three-phase sum, not L1 alone.
        assert_eq!(measurement["total_power"], json!(25.6_f32));
        assert_eq!(measurement["systime"], 1_789_068_334);
        assert_eq!(measurement["frequency_hz"], json!(49.97_f32));
        assert_eq!(measurement["totals"]["real_power_w"], json!(25.6_f32));
        assert_eq!(measurement["totals"]["apparent_power_va"], json!(71.7_f32));
        assert_eq!(
            measurement["totals"]["reactive_power_var"],
            json!(-36.59_f32)
        );

        for (phase, block) in ["l1", "l2", "l3"].into_iter().enumerate() {
            for (field, value) in [
                ("voltage_v", snapshot.voltage[phase]),
                ("current_a", snapshot.current[phase]),
                ("real_power_w", snapshot.real_power[phase]),
                ("apparent_power_va", snapshot.apparent_power[phase]),
                ("reactive_power_var", snapshot.reactive_power[phase]),
                ("cos_phi", snapshot.cos_phi[phase]),
                (
                    "real_energy_consumed_wh",
                    snapshot.real_energy_consumed[phase],
                ),
            ] {
                assert_eq!(measurement[block][field], json!(value), "{block}.{field}");
            }
        }
    }

    #[test]
    fn unavailable_values_are_sent_as_null_rather_than_dropped_or_zeroed() {
        let measurement = measurement_json(&sample_snapshot(), 25.6, false);

        // A zero here would claim a distortion-free phase, and a missing field would look
        // like an older client. Null says what is true: the meter could not measure it.
        for block in ["l2", "l3"] {
            for field in ["thd_voltage_pct", "thd_current_pct"] {
                assert_eq!(
                    measurement[block][field],
                    serde_json::Value::Null,
                    "{block}.{field}"
                );
            }
        }
        assert_eq!(measurement["l1"]["thd_voltage_pct"], json!(1.85_f32));
        assert_eq!(measurement["l1"]["thd_current_pct"], json!(125.42_f32));
    }

    #[test]
    fn a_reading_says_whether_it_is_a_settled_change_or_a_keepalive() {
        // The receiver keeps a keepalive as a measurement but leaves it out of device
        // inference, so the two have to be distinguishable on the wire.
        let snapshot = sample_snapshot();
        assert_eq!(
            measurement_json(&snapshot, 25.6, false)["heartbeat"],
            json!(false)
        );
        assert_eq!(
            measurement_json(&snapshot, 25.6, true)["heartbeat"],
            json!(true)
        );
    }

    #[test]
    fn exported_power_is_reported_as_a_negative_total() {
        // A site that feeds more into the grid than it draws is normal in a decentralized
        // grid, and the sign has to survive the trip to the server.
        let mut snapshot = sample_snapshot();
        snapshot.real_power = [-1200.0, 0.0, 0.0];
        snapshot.real_power_sum3 = -1200.0;

        let measurement = measurement_json(&snapshot, snapshot.real_power_sum3, false);
        assert_eq!(measurement["total_power"], json!(-1200.0_f32));
        assert_eq!(measurement["l1"]["real_power_w"], json!(-1200.0_f32));
    }

    #[test]
    fn summary_line_names_every_phase_and_marks_unavailable_values() {
        let line = summary(&sample_snapshot());

        assert!(line.contains("L1 ["), "{line}");
        assert!(line.contains("L2 ["), "{line}");
        assert!(line.contains("L3 ["), "{line}");
        assert!(
            line.contains("Sum3 [P: 25.60W, S: 71.70VA, Q: -36.59var]"),
            "{line}"
        );
        assert!(line.contains("THD_I: n/a%"), "{line}");
        assert!(line.contains("THD_I: 125.42%"), "{line}");
    }

    #[test]
    fn a_settled_change_is_always_recorded() {
        let now = Instant::now();
        // Even immediately after another reading, and even with no heartbeat configured.
        assert!(should_record(true, None, Some(now), now));
        assert!(should_record(
            true,
            Some(Duration::from_secs(2)),
            Some(now),
            now
        ));
    }

    #[test]
    fn an_unchanged_reading_is_recorded_once_the_link_has_been_quiet_too_long() {
        let t0 = Instant::now();
        let heartbeat = Some(Duration::from_secs(2));

        // The very first reading has nothing to be quiet since.
        assert!(should_record(false, heartbeat, None, t0));

        // Inside the interval the noise filter still holds the reading back.
        assert!(!should_record(
            false,
            heartbeat,
            Some(t0),
            t0 + Duration::from_millis(1999)
        ));

        // At the interval it goes out, so the trend and the link stay alive.
        assert!(should_record(
            false,
            heartbeat,
            Some(t0),
            t0 + Duration::from_secs(2)
        ));
    }

    #[test]
    fn without_a_heartbeat_only_changes_are_recorded() {
        // `--heartbeat-ms 0` restores the pure change-triggered behaviour.
        let t0 = Instant::now();
        assert!(!should_record(
            false,
            None,
            Some(t0),
            t0 + Duration::from_secs(3600)
        ));
        assert!(!should_record(false, None, None, t0));
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

        let spike = with_real_power(baseline, baseline.real_power[0] + 40.0);

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

        let higher = with_real_power(baseline, baseline.real_power[0] + 40.0);

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
        let r1 = with_real_power(baseline, 30.0);
        let r2 = with_real_power(baseline, 45.0);
        let r3 = with_real_power(baseline, 28.0);
        let r4 = with_real_power(baseline, 65.0);

        assert!(!monitor.process_reading(r1, t0 + Duration::from_millis(200)));
        assert!(!monitor.process_reading(r2, t0 + Duration::from_millis(500)));
        assert!(!monitor.process_reading(r3, t0 + Duration::from_millis(800)));
        assert!(!monitor.process_reading(r4, t0 + Duration::from_millis(1000)));

        // Stable at 65W until 2 seconds have passed since t0 + 1000ms (i.e. t0 + 3000ms)
        assert!(!monitor.process_reading(r4, t0 + Duration::from_millis(2500)));
        assert!(monitor.process_reading(r4, t0 + Duration::from_millis(3000)));
    }
}
