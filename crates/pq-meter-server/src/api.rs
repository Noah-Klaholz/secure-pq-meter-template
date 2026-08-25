//! The HTTP/3 endpoint that receives data from the gateway.
//!
//! The application is a plain [`axum::Router`]. The SDK's [`ScionH3AxumServer`] serves it
//! over HTTP/3 on a SCION socket, so everything you know about axum applies — add routes,
//! extractors and state as you like.

use std::sync::Arc;

use anyhow::Context;
use axum::{Router, http::StatusCode, routing::post};
use scion_h3_axum::ScionH3AxumServer;
use scion_quic::{quic::config::QuicConfig, reexport::squiche, socket::GenericScionUdpSocket};

/// Path the server accepts POST requests on.
pub const DEFAULT_PATH: &str = "/edh/v1/hello";

/// TLS name of the server. HTTP/3 always runs over TLS, and the certificate below is
/// issued for this name.
pub const SERVER_NAME: &str = "pq-meter-server";

/// Serves the HTTP/3 application on `socket` until the process is stopped.
pub async fn serve(socket: Arc<dyn GenericScionUdpSocket>, path: &str) -> anyhow::Result<()> {
    let app = Router::new().route(path, post(receive));
    let config = quic_config().context("building the QUIC server configuration")?;

    ScionH3AxumServer::serve(socket, app, config)
        .await
        .map_err(|error| anyhow::anyhow!("HTTP/3 server stopped: {error}"))
}

/// Prints the body of an incoming POST request.
///
/// The body is taken as a `String` so that anything the gateway sends shows up on stdout,
/// even while you are still shaping your message format. Once the format is fixed, replace
/// the argument with `axum::Json<YourType>` and let axum parse it for you.
async fn receive(body: String) -> (StatusCode, &'static str) {
    println!("received: {body}");
    (StatusCode::OK, "ok\n")
}

/// Builds the QUIC configuration of the server, with a self-signed certificate that is
/// generated on every start.
///
/// The client does not verify this certificate. That keeps the setup short, but it means
/// the connection is encrypted without the client knowing who it talks to. Use a real
/// certificate before taking anything like this outside of a hackathon.
fn quic_config() -> anyhow::Result<squiche::Config> {
    let mut config = QuicConfig::builder()
        .verify_peer(false)
        .build()
        .to_quiche_config()
        .context("creating the QUIC configuration")?;

    let certificate = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_string()])
        .context("generating a self-signed certificate")?;

    // squiche reads the certificate and the key from files, so write them to temporary
    // files that are removed again when this function returns.
    let certificate_file = write_temporary_file(certificate.cert.pem().as_bytes(), "certificate")?;
    let key_file = write_temporary_file(certificate.signing_key.serialize_pem().as_bytes(), "key")?;

    config
        .load_cert_chain_from_pem_file(path_of(&certificate_file)?)
        .context("loading the certificate")?;
    config
        .load_priv_key_from_pem_file(path_of(&key_file)?)
        .context("loading the private key")?;

    Ok(config)
}

/// Writes `contents` to a temporary file.
fn write_temporary_file(contents: &[u8], what: &str) -> anyhow::Result<tempfile::NamedTempFile> {
    use std::io::Write;

    let mut file = tempfile::NamedTempFile::new()
        .with_context(|| format!("creating a temporary file for the {what}"))?;
    file.write_all(contents)
        .with_context(|| format!("writing the {what}"))?;
    Ok(file)
}

/// Returns the path of a temporary file as a string.
fn path_of(file: &tempfile::NamedTempFile) -> anyhow::Result<&str> {
    file.path()
        .to_str()
        .context("temporary file path is not valid UTF-8")
}
