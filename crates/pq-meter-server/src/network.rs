//! The simulated SCION network that the server and the gateway share.
//!
//! [PocketSCION] simulates a complete SCION network inside this process. Here it runs
//! two autonomous systems (ASes):
//!
//! * [`GATEWAY_AS`] — the AS the gateway (Raspberry Pi) attaches to.
//! * [`SERVER_AS`] — the AS the HTTP/3 server in this binary runs in.
//!
//! Each AS gets two things the outside world talks to:
//!
//! * an *endhost API*: the HTTP service a SCION stack asks for paths and for the address
//!   of its SNAP.
//! * a *SNAP* (SCION Network Access Point): the gateway into the SCION network. A program
//!   without a SCION-capable operating system sends its packets through a SNAP tunnel. It
//!   has a control plane (setting the tunnel up) and a data plane (carrying the packets).
//!
//! By default PocketSCION binds all of these to `127.0.0.1`, which is unreachable from
//! another machine. We therefore set an explicit address for every interface the gateway
//! needs, using the IP address passed on the command line and the fixed ports below.
//!
//! [PocketSCION]: pocketscion

use std::net::{IpAddr, SocketAddr};

use anyhow::Context;
use pocketscion::{
    io_config::IoConfig,
    network::scion::topology::{
        ScionAs, ScionLink, ScionLinkType, ScionTopology, ScionTopologyBuilder,
    },
    runtime::{PocketScionRuntime, builder::PocketScionRuntimeBuilder},
    state::PocketScionState,
    util::addr_to_http_url,
};
use url::Url;

/// The AS the gateway (Raspberry Pi) attaches to (`1-ff00:0:132`), and the AS the HTTP/3
/// server runs in (`2-ff00:0:212`).
pub use pocketscion::util::topologies::{IA132 as GATEWAY_AS, IA212 as SERVER_AS};

/// Port of the endhost API of [`GATEWAY_AS`]. The client is pointed at this port.
pub const GATEWAY_ENDHOST_API_PORT: u16 = 31000;
/// Port of the endhost API of [`SERVER_AS`], used by the server in this binary.
const SERVER_ENDHOST_API_PORT: u16 = 31001;
/// Port of the SNAP control plane in [`GATEWAY_AS`].
const GATEWAY_SNAP_CONTROL_PORT: u16 = 31010;
/// Port of the SNAP data plane in [`GATEWAY_AS`].
const GATEWAY_SNAP_DATA_PLANE_PORT: u16 = 31011;
/// Port of the SNAP control plane in [`SERVER_AS`].
const SERVER_SNAP_CONTROL_PORT: u16 = 31020;
/// Port of the SNAP data plane in [`SERVER_AS`].
const SERVER_SNAP_DATA_PLANE_PORT: u16 = 31021;

/// A running simulated SCION network.
pub struct Network {
    /// Owns all simulation tasks. Dropping this stops the network.
    _runtime: PocketScionRuntime,
    /// Endhost API of [`GATEWAY_AS`]; this is the URL the client needs.
    pub gateway_endhost_api: Url,
    /// Endhost API of [`SERVER_AS`]; used by the HTTP/3 server in this binary.
    pub server_endhost_api: Url,
}

/// Starts the simulated SCION network and exposes its interfaces on `bind_ip`.
pub async fn start(bind_ip: IpAddr) -> anyhow::Result<Network> {
    let mut state = PocketScionState::new(chrono::Utc::now());
    state.set_topology(topology().context("building the SCION topology")?);

    // One endhost API and one SNAP per AS.
    let gateway_api = state.add_endhost_api([GATEWAY_AS]);
    let server_api = state.add_endhost_api([SERVER_AS]);
    let gateway_snap = state
        .add_snap(GATEWAY_AS)
        .context("adding the gateway SNAP")?;
    let server_snap = state
        .add_snap(SERVER_AS)
        .context("adding the server SNAP")?;

    // Bind every interface a client may talk to on the given IP address.
    let io_config = IoConfig::new();
    io_config.set_endhost_api_addr(
        gateway_api,
        SocketAddr::new(bind_ip, GATEWAY_ENDHOST_API_PORT),
    );
    io_config.set_endhost_api_addr(
        server_api,
        SocketAddr::new(bind_ip, SERVER_ENDHOST_API_PORT),
    );
    io_config.set_snap_control_addr(
        gateway_snap,
        SocketAddr::new(bind_ip, GATEWAY_SNAP_CONTROL_PORT),
    );
    io_config.set_snap_data_plane_addr(
        gateway_snap,
        SocketAddr::new(bind_ip, GATEWAY_SNAP_DATA_PLANE_PORT),
    );
    io_config.set_snap_control_addr(
        server_snap,
        SocketAddr::new(bind_ip, SERVER_SNAP_CONTROL_PORT),
    );
    io_config.set_snap_data_plane_addr(
        server_snap,
        SocketAddr::new(bind_ip, SERVER_SNAP_DATA_PLANE_PORT),
    );

    let runtime = PocketScionRuntimeBuilder::new()
        .with_system_state(state)
        .with_io_config(io_config)
        .start()
        .await
        .context("starting PocketSCION")?;

    let gateway_endhost_api = runtime
        .endhost_api_addr(gateway_api)
        .map(addr_to_http_url)
        .context("PocketSCION did not report the gateway endhost API address")?;
    let server_endhost_api = runtime
        .endhost_api_addr(server_api)
        .map(addr_to_http_url)
        .context("PocketSCION did not report the server endhost API address")?;

    Ok(Network {
        _runtime: runtime,
        gateway_endhost_api,
        server_endhost_api,
    })
}

/// Two core ASes joined by a single link.
///
/// This is the smallest network with more than one AS, so packets between the gateway and
/// the server really do cross an inter-AS SCION path. Add more ASes and links here if you
/// want to experiment with path selection.
fn topology() -> anyhow::Result<ScionTopology> {
    let mut topology = ScionTopologyBuilder::new();
    topology
        .add_as(ScionAs::new_core(GATEWAY_AS))?
        .add_as(ScionAs::new_core(SERVER_AS))?
        .add_link(ScionLink::new(
            GATEWAY_AS,
            1,
            ScionLinkType::Core,
            SERVER_AS,
            3,
        )?)?;
    topology.build()
}
