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
use umg605_modbus_client::{DEFAULT_MODBUS_PORT, Umg605ProClient};
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

/// How often meter values are read.
const METER_INTERVAL: Duration = Duration::from_millis(200);

/// TLS name the server's certificate is issued for.
const SERVER_NAME: &str = "pq-meter-server";

/// Largest response body we read. The answer of the server is a few bytes.
const MAX_BODY_SIZE: usize = 1024;

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

    /// Message to send.
    #[arg(long, default_value = "Hello from Energy Data Hackdays 2026")]
    message: String,

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

    let (server_scion, server_ip): (ScionSocketIpAddr, IpAddr) =
        if let Ok(ip) = server_raw.parse::<IpAddr>() {
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

    let body = serde_json::to_vec(&serde_json::json!({ "message": args.message }))
        .context("encoding the message")?;

    // The URL holds the server name and the port. `target` gives the SCION address the
    // packets go to, so the simulated network needs no DNS.
    let url = format!("https://{SERVER_NAME}:{}{}", server_scion.port(), args.path);
    let request = Request::post(&url)
        .header("content-type", "application/json")
        .target(server_scion.host())
        .body(body)
        .build()
        .context("building the request")?;

    println!("sending to {server_scion}{} ...", args.path);

    let response = client
        .request(request)
        .await
        .context("sending the request")?;
    let status = response.status();
    let (body, _trailers) = response
        .text(Some(MAX_BODY_SIZE))
        .await
        .context("reading the response")?;

    println!("server answered {status}: {body}");

    client.close().await;

    anyhow::ensure!(status.is_success(), "server answered with {status}");

    let meter_socket_addr = SocketAddr::new(args.meter_ip, args.meter_port);
    let meter_timeout = Duration::from_secs(args.meter_timeout);
    let mut meter_client =
        Umg605ProClient::connect_tcp(meter_socket_addr, Slave(args.meter_unit), meter_timeout)
            .await?;

    monitor(&mut meter_client, METER_INTERVAL).await?;

    Ok(())
}

async fn monitor(client: &mut Umg605ProClient, period: Duration) -> anyhow::Result<()> {
    let mut interval = tokio::time::interval(period);
    loop {
        interval.tick().await;
        let start = Instant::now();
        
        let voltage_l1 = client.voltage_l1().await?;
        let current_l1 = client.current_l1().await?;
        let power_l1_n = client.power_l1_n().await?;
        let apparent_power_l1 = client.apparent_power_l1().await?;
        let reactive_power_l1 = client.reactive_power_l1().await?;
        let cosphi_l1 = client.cosphi_l1().await?;
        let thd_voltage_l1 = client.thd_voltage_l1().await?;
        let thd_current_l1 = client.thd_current_l1().await?;
        
        let frequency = client.frequency().await?;
        let current_sum = client.current_sum().await?;
        let real_power_sum = client.real_power_sum().await?;
        let apparent_power_sum = client.apparent_power_sum().await?;
        let reactive_power_sum = client.reactive_power_sum().await?;
        
        let total_elapsed = start.elapsed();

        if total_elapsed > period {
            eprintln!(
                "Warning: Reading values took longer than the {:.2?} interval: {:.2?}",
                period, total_elapsed
            );
        }

        println!(
            "L1 [U: {:.2}V, I: {:.2}A, P: {:.2}W, S: {:.2}VA, Q: {:.2}var, PF: {:.2}, THD_U: {:.2}%, THD_I: {:.2}%] | Global [f: {:.2}Hz, I_sum: {:.2}A, P_sum: {:.2}W, S_sum: {:.2}VA, Q_sum: {:.2}var]",
            voltage_l1, current_l1, power_l1_n, apparent_power_l1, reactive_power_l1, cosphi_l1, thd_voltage_l1, thd_current_l1,
            frequency, current_sum, real_power_sum, apparent_power_sum, reactive_power_sum
        );
    }
}
