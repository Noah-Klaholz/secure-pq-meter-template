//! CLI for reading measured values from a Janitza UMG 605-PRO via Modbus TCP.

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use clap::Parser;
use tokio_modbus::Slave;
use umg605_modbus_client::{DEFAULT_MODBUS_PORT, PHASE_COUNT, Snapshot, Umg605ProClient};

/// Unit id for a directly addressed Modbus TCP device, per the Modbus TCP spec.
const DEFAULT_MODBUS_UNIT: u8 = 1;

const DEFAULT_TIMEOUT_SECS: u64 = 5;

/// How often `monitor` reads a fresh set of values.
const MONITOR_INTERVAL: Duration = Duration::from_secs(1);

#[derive(clap::Parser)]
#[command(
    version,
    about = "Read measured values from a Janitza UMG 605-PRO via Modbus TCP"
)]
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
    /// Monitor the complete three-phase snapshot the gateway sends.
    Snapshot,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let socket_addr = SocketAddr::new(cli.ip, cli.port);
    let timeout = Duration::from_secs(cli.timeout);

    let mut client = Umg605ProClient::connect_tcp(socket_addr, Slave(cli.unit), timeout).await?;

    match cli.subcommand {
        SubCommand::Monitor => monitor(&mut client, MONITOR_INTERVAL).await?,
        SubCommand::Snapshot => snapshot(&mut client, MONITOR_INTERVAL).await?,
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
                period,
                elapsed1,
                elapsed2 - elapsed1,
                elapsed3 - elapsed2,
                elapsed3
            );
        }

        println!(
            "Voltage L1: {:.2} V, Current L1: {:.2} A, Power L1-N: {:.2} W",
            voltage_l1, current_l1, power_l1_n
        );
    }
}

/// Prints the same set of values the gateway reads, so the meter can be checked without
/// running the SCION side.
async fn snapshot(client: &mut Umg605ProClient, period: Duration) -> anyhow::Result<()> {
    let mut interval = tokio::time::interval(period);
    loop {
        interval.tick().await;
        let start = Instant::now();
        let snapshot = client.snapshot().await?;
        let elapsed = start.elapsed();

        println!(
            "systime {} | {:.2?} | f {} Hz | Sum3 P {} W, S {} VA, Q {} var",
            snapshot.systime,
            elapsed,
            show(snapshot.frequency),
            show(snapshot.real_power_sum3),
            show(snapshot.apparent_power_sum3),
            show(snapshot.reactive_power_sum3),
        );
        for phase in 0..PHASE_COUNT {
            println!("  {}", phase_line(&snapshot, phase));
        }
    }
}

fn phase_line(snapshot: &Snapshot, phase: usize) -> String {
    format!(
        "L{}: U {} V, I {} A, P {} W, S {} VA, Q {} var, cos phi {}, E {} Wh, THD-U {} %, THD-I {} %",
        phase + 1,
        show(snapshot.voltage[phase]),
        show(snapshot.current[phase]),
        show(snapshot.real_power[phase]),
        show(snapshot.apparent_power[phase]),
        show(snapshot.reactive_power[phase]),
        show(snapshot.cos_phi[phase]),
        show(snapshot.real_energy_consumed[phase]),
        show(snapshot.thd_voltage[phase]),
        show(snapshot.thd_current[phase]),
    )
}

/// The meter answers with NaN for quantities its wiring gives it no way to measure.
fn show(value: f32) -> String {
    if value.is_finite() {
        format!("{value:.2}")
    } else {
        "n/a".to_owned()
    }
}
