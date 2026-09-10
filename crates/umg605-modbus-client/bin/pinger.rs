//! CLI for reading measured values from a Janitza UMG 605-PRO via Modbus TCP.

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use clap::Parser;
use tokio_modbus::Slave;
use umg605_modbus_client::{Umg605ProClient, DEFAULT_MODBUS_PORT};

/// Unit id for a directly addressed Modbus TCP device, per the Modbus TCP spec.
const DEFAULT_MODBUS_UNIT: u8 = 1;

const DEFAULT_TIMEOUT_SECS: u64 = 5;

/// How often `monitor` reads a fresh set of values.
const MONITOR_INTERVAL: Duration = Duration::from_secs(1);

#[derive(clap::Parser)]
#[command(version, about = "Read measured values from a Janitza UMG 605-PRO via Modbus TCP")]
struct Cli {
    /// The IP address of the Umg605Pro device.
    #[arg(short, long)]
    ip: IpAddr,

    /// The Modbus TCP port of the Umg605Pro device.
    #[arg(short, long, default_value_t = DEFAULT_MODBUS_PORT)]
    port: u16,

    /// The Modbus unit id, i.e. the device address configured on the meter.
    #[arg(short, long, default_value_t = DEFAULT_MODBUS_UNIT)]
    unit: u8,

    /// Timeout in seconds for connecting and for each register read.
    #[arg(short, long, default_value_t = DEFAULT_TIMEOUT_SECS)]
    timeout: u64,

    #[command(subcommand)]
    subcommand: SubCommand,
}

#[derive(clap::Subcommand)]
enum SubCommand {
    /// Monitor current Voltage and Current values.
    Monitor,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let socket_addr = SocketAddr::new(cli.ip, cli.port);
    let timeout = Duration::from_secs(cli.timeout);

    let mut client = Umg605ProClient::connect_tcp(socket_addr, Slave(cli.unit), timeout).await?;

    match cli.subcommand {
        SubCommand::Monitor => monitor(&mut client, MONITOR_INTERVAL).await?,
    }

    Ok(())
}

async fn monitor(client: &mut Umg605ProClient, period: Duration) -> anyhow::Result<()> {
    let mut interval = tokio::time::interval(period);
    loop {
        interval.tick().await;
        let start = Instant::now();
        let voltage_l1 = client.voltage_l1().await?;
        let elapsed1 = start.elapsed();
        let current_l1 = client.current_l1().await?;
        let elapsed2 = start.elapsed();
        let power_l1_n = client.power_l1_n().await?;
        let elapsed3 = start.elapsed();

        if elapsed3 > period {
            eprintln!(
                "Warning: Reading values took longer than the {:.2?} interval: {:.2?} + {:.2?} + {:.2?} = {:.2?}",
                period, elapsed1, elapsed2 - elapsed1, elapsed3 - elapsed2, elapsed3
            );
        }

        println!(
            "Voltage L1: {:.2} V, Current L1: {:.2} A, Power L1-N: {:.2} W",
            voltage_l1, current_l1, power_l1_n
        );
    }
}
