//! Server side of the Energy Data Hackdays gateway challenge.
//!
//! This binary runs three cooperating services:
//!
//! 1. It starts a simulated SCION network (PocketSCION) with two ASes, see [`network`].
//! 2. It runs an HTTP/3 server inside one of those ASes, see [`api`].
//! 3. It serves a local, read-only web dashboard, see [`dashboard`].
//!
//! Run it on the laptop; run `pq-meter-client` on the gateway.

mod api;
mod dashboard;
mod decision;
pub mod history;
mod input;
mod meter;
mod network;
mod quality;
mod transport;

#[cfg(test)]
mod e2e_pipeline_test;

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
};

use anyhow::Context;
use clap::{Parser, ValueEnum};
use pocketscion::util::dev_auth_token;
use scion_quic::socket::GenericScionUdpSocket;
use scion_stack::stack::ScionStackBuilder;

/// Command line arguments.
#[derive(Debug, Parser)]
#[command(
    version,
    about = "Simulated SCION network with an HTTP/3 server for meter data"
)]
struct Args {
    /// IP address the simulated SCION network exposes its interfaces on.
    ///
    /// The default is only reachable on this machine. To let the gateway connect, pass the
    /// address of the interface it can reach, for example the WLAN address of this laptop.
    #[arg(long, default_value = "127.0.0.1")]
    bind_ip: IpAddr,

    /// Path the HTTP/3 server accepts POST requests on.
    #[arg(long, default_value = api::DEFAULT_PATH)]
    path: String,

    /// Algorithm used to infer device changes from total-power readings.
    #[arg(long, value_enum, default_value_t = DecisionMethodArg::Adaptive)]
    decision_method: DecisionMethodArg,

    /// Local dashboard port (0 selects a free port). Always binds to 127.0.0.1.
    #[arg(long, default_value_t = 8080)]
    dashboard_port: u16,

    /// Run the receiver without the local web dashboard.
    #[arg(long)]
    no_dashboard: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum DecisionMethodArg {
    /// Training-free online NILM: learn the idle background, then fingerprint each
    /// settled step change in P-Q-distortion space and match or add a device.
    Adaptive,
    /// Wait for three readings within +/- 3 W before deciding (static catalog).
    Settled,
    /// Decide immediately from each change relative to the previous reading.
    Immediate,
    /// Multi-feature P-Q-THD fingerprint matching with settled readings.
    MultiFeature,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();

    // Reserve the dashboard port first so a conflict fails before starting SCION.
    let dashboard_listener = if args.no_dashboard {
        None
    } else {
        Some(
            tokio::net::TcpListener::bind(SocketAddr::from((
                Ipv4Addr::LOCALHOST,
                args.dashboard_port,
            )))
            .await
            .context("binding the local dashboard; use --dashboard-port to select another port")?,
        )
    };

    // The SDK uses rustls for its control plane; pick a crypto backend.
    scion_sdk_utils::rustls::select_ring_crypto_provider();

    let network = network::start(args.bind_ip).await?;

    // Attach a SCION stack to the server's AS and open a socket on it. The SNAP assigns the
    // address, so we can only print it once the socket exists.
    let stack = ScionStackBuilder::new()
        .with_endhost_api(network.server_endhost_api.clone())
        .with_auth_token(dev_auth_token())
        .build()
        .await
        .context("building the SCION stack of the server")?;
    let addr_str = format!("[{},{}]:60000", network::SERVER_AS, args.bind_ip);
    let bind_addr: sciparse::address::ip_socket_addr::ScionSocketIpAddr =
        addr_str.parse().context("parsing bind address")?;

    let socket = stack
        .bind(Some(bind_addr))
        .await
        .context("opening a SCION socket for the server")?;
    let server_address = socket.local_addr();

    // The table, decision method, and input decoder are supplied independently so each can
    // be replaced without changing the HTTP server.
    // The adaptive method learns every device at runtime, so it starts from an empty
    // catalog; the static methods keep the predefined table.
    let seed_catalog = match args.decision_method {
        DecisionMethodArg::Adaptive => Vec::new(),
        _ => decision::DUMMY_DEVICE_CATALOG.to_vec(),
    };
    let meter = Arc::new(Mutex::new(meter::MeterState::new(seed_catalog)));
    let decision_method: Box<dyn decision::DecisionMethod> = match args.decision_method {
        DecisionMethodArg::Adaptive => Box::new(decision::AdaptiveNilm::new()),
        DecisionMethodArg::Settled => Box::new(decision::SettledPowerMatch::new(
            5.0, // Minimum change that can trigger a device-state update.
            3.0, // Consecutive readings must remain within +/- 3 W.
            1,   // 1 reading because client already filters noise and settles over a 2s window.
            8.0, // Maximum difference between the settled delta and table value.
        )),
        DecisionMethodArg::Immediate => Box::new(decision::ClosestPowerMatch::new(8.0)),
        DecisionMethodArg::MultiFeature => Box::new(
            decision::SettledPowerMatch::with_pq_tolerances(5.0, 3.0, 1, 8.0, 15.0, 30.0),
        ),
    };
    let decision_method: api::SharedDecisionMethod = Arc::new(Mutex::new(decision_method));
    let reading_decoder: input::SharedReadingDecoder = Arc::new(input::JsonReadingDecoder);

    println!("SCION network is up");
    println!("  gateway endhost API: {}", network.gateway_endhost_api);
    println!("  HTTP/3 server:       {server_address}");
    println!("  accepting POST on:   {}", args.path);
    println!(
        r#"  expected JSON:       [{{"total_power": 860.0}}] (or one object; context optional)"#
    );
    println!();
    println!("Start the client with:");
    println!("  pq-meter-client --server {}", args.bind_ip);
    println!();

    let dashboard_source = Arc::new(dashboard::model::LiveMeterSource {
        meter: meter.clone(),
        decision_method: match args.decision_method {
            DecisionMethodArg::Adaptive => "adaptive",
            DecisionMethodArg::Settled => "settled",
            DecisionMethodArg::Immediate => "immediate",
            DecisionMethodArg::MultiFeature => "multi-feature",
        },
    });
    let receiver = api::serve(
        Arc::new(socket) as Arc<dyn GenericScionUdpSocket>,
        &args.path,
        meter,
        decision_method,
        reading_decoder,
    );
    if let Some(listener) = dashboard_listener {
        println!("  local dashboard:    http://{}/", listener.local_addr()?);
        // Both services share a lifetime; propagate errors instead of losing a background task.
        tokio::select! {
            result = receiver => result,
            result = dashboard::serve(listener, dashboard_source) => result,
        }
    } else {
        receiver.await
    }
}
