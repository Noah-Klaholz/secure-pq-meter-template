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

use anyhow::Context;
use clap::Parser;
use scion_http3::{Client, Config, Request, scion_quic::quic::config::QuicConfig};
use sciparse::address::ip_socket_addr::ScionSocketIpAddr;
use url::Url;

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
    /// URL of the endhost API this client attaches to, for example
    /// `http://192.168.1.42:31000`.
    #[arg(long)]
    endhost_api: Url,

    /// SCION address of the server, for example `[2-ff00:0:212,192.168.1.42]:31337`.
    #[arg(long)]
    server: ScionSocketIpAddr,

    /// Path to POST to on the server.
    #[arg(long, default_value = "/edh/v1/hello")]
    path: String,

    /// Message to send.
    #[arg(long, default_value = "Hello from Energy Data Hackdays 2026")]
    message: String,
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

    // One client per program: it holds the connection pool. Building it does no I/O, the
    // connection is established with the first request.
    let client = Client::new(
        Config::new(args.endhost_api)
            .with_auth_token(snap_tokens::v0::dummy_snap_token())
            // The server uses a self-signed certificate, so its identity is not verified.
            .with_quic_config(QuicConfig::builder().verify_peer(false).build()),
    );

    let body = serde_json::to_vec(&serde_json::json!({ "message": args.message }))
        .context("encoding the message")?;

    // The URL holds the server name and the port. `target` gives the SCION address the
    // packets go to, so the simulated network needs no DNS.
    let url = format!("https://{SERVER_NAME}:{}{}", args.server.port(), args.path);
    let request = Request::post(&url)
        .header("content-type", "application/json")
        .target(args.server.host())
        .body(body)
        .build()
        .context("building the request")?;

    println!("sending to {}{} ...", args.server, args.path);

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
    Ok(())
}
