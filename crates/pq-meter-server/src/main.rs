//! Server side of the Energy Data Hackdays gateway challenge.
//!
//! This binary does two things:
//!
//! 1. It starts a simulated SCION network (PocketSCION) with two ASes, see [`network`].
//! 2. It runs an HTTP/3 server inside one of those ASes, see [`api`].
//!
//! Run it on the laptop; run `pq-meter-client` on the gateway.

mod api;
mod network;

use std::{net::IpAddr, sync::Arc};

use anyhow::Context;
use clap::Parser;
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();

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

    // The catalog and decision method are intentionally supplied independently. Edit the
    // table in `api.rs`, or replace `ClosestPowerMatch` with another `DecisionMethod`.
    let meter = Arc::new(std::sync::Mutex::new(api::MeterState::new(
        api::DUMMY_DEVICE_CATALOG.to_vec(),
    )));
    let decision_method: api::SharedDecisionMethod = Arc::new(api::ClosestPowerMatch::new(40.0));

    println!("SCION network is up");
    println!("  gateway endhost API: {}", network.gateway_endhost_api);
    println!("  HTTP/3 server:       {server_address}");
    println!("  accepting POST on:   {}", args.path);
    println!(r#"  expected JSON:       {{"total_power": 860.0}}"#);
    println!();
    println!("Start the client with:");
    println!("  pq-meter-client --server {}", args.bind_ip);
    println!();

    api::serve(
        Arc::new(socket) as Arc<dyn GenericScionUdpSocket>,
        &args.path,
        meter,
        decision_method,
    )
    .await
}
