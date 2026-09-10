# Secure power quality metering via SCION — starter template

This repository is the starting point for the *Secure Power Quality Metering via SCION*
challenge at the [Energy Data Hackdays](https://www.energydatahackdays.ch/). It contains
three small programs:

* a **server** that runs on your laptop and receives data,
* a **client** that runs on the Raspberry Pi 5 gateway and sends data to it over SCION,
* a **Modbus client** that reads data from a UMG 605-PRO power quality meter over Modbus TCP.

Get these three running first. Once a message from the Pi shows up on your laptop, the
networking part of the challenge is done and you can put your time into the gateway itself:
reading the meter, deciding what to send, and how often.

The challenge itself is described in the
[challenge description](https://www.energydatahackdays.ch/uploads/secure-power-quality-metering-via-scion/Secure-PQ-Metering-via-SCION.pdf).

## What is in this repository

```
crates/
  pq-meter-server/         Runs on the laptop
    src/main.rs            Command line interface, starts everything
    src/network.rs         The simulated SCION network (which ASes, which addresses)
    src/api.rs             The HTTP/3 endpoint that receives the data
    src/input.rs           Typed measurement decoding and batch validation
    src/meter.rs           Shared current state, independent of either HTTP transport
    src/dashboard/         Local dashboard, versioned read API, and embedded UI assets
  pq-meter-client/         Runs on the gateway
    src/main.rs            Reads the meter and sends batches of measurements
  umg605-modbus-client/    Reads data from a UMG 605-PRO power quality meter over Modbus TCP
    src/lib.rs             The Modbus TCP client and the registers it reads
    bin/pinger.rs          Example binary that reads values from the meter
Cargo.toml                 Workspace, pins the SCION SDK version
rust-toolchain.toml        Rust version used to build this repository
```

The server and client are built on the [SCION endhost SDK](https://github.com/Anapaya/scion-sdk),
pinned to one release in the workspace `Cargo.toml`. The
[SCION SDK academy](https://learn.anapaya.net/docs/academy/scion-sdk/) explains the concepts
behind it — autonomous systems, addresses, paths and segments — and is the place to read up
when a term in this README is new to you. The API reference is on
[docs.rs/scion-http3](https://docs.rs/scion-http3) and [docs.rs/scion-stack](https://docs.rs/scion-stack).

## How the pieces fit together

```
  Raspberry Pi 5 (gateway)                  Laptop
 ┌──────────────────────────┐              ┌───────────────────────────────────────┐
 │ pq-meter-client          │              │ pq-meter-server                       │
 │                          │  your WLAN   │  ┌─────────────────────────────────┐  │
 │  SCION stack ────────────┼─────────────►│  │ PocketSCION                     │  │
 │   HTTP/3 POST            │              │  │  1-ff00:0:132 ─── 2-ff00:0:212  │  │
 │                          │              │  └─────────────────────────────────┘  │
 │                          │              │  HTTP/3 server in 2-ff00:0:212        │
 └──────────────────────────┘              └───────────────────────────────────────┘
```

Three terms are enough to follow what happens:

* **SCION** is an internet architecture in which the application, not the network, chooses
  the path its packets take. A SCION address looks like `[2-ff00:0:212,10.0.0.1]:31337`: an
  ISD-AS number (the autonomous system) plus a normal IP address and port inside it.
* **PocketSCION** is a SCION network simulator that ships with the SDK. The server binary
  starts it, so you need no SCION installation and no access to a real SCION network. It
  simulates two autonomous systems: one for the gateway, one for the server.
* A **SNAP** (SCION Network Access Point) is how a program on an ordinary operating system
  reaches a SCION network: it tunnels its packets to the SNAP, which forwards them into
  SCION. The client does this for you; it only needs to know where the SNAP is.

The client learns everything it needs from one URL, the *endhost API* of its autonomous
system. That is the service a SCION stack asks for paths and for the address of its SNAP.
The server prints this URL when it starts.

## Try it on one machine

You need the [build tools](#installing-the-build-tools): Rust, cmake and a C/C++ compiler.
In the first terminal:

```bash
cargo run -p pq-meter-server
```

The server also starts a local dashboard at **http://127.0.0.1:8080/**. See
[Local dashboard](#local-dashboard) for configuration and the extension points.

It prints, among the log lines:

```text
SCION network is up
  gateway endhost API: http://127.0.0.1:31000/
  HTTP/3 server:       [2-ff00:0:212,127.0.0.1]:59218
  accepting POST on:   /edh/v1/hello

Start the client with:
  pq-meter-client --endhost-api http://127.0.0.1:31000/ --server '[2-ff00:0:212,127.0.0.1]:59218'
```

Copy that command into a second terminal and run it through cargo:

```bash
cargo run -p pq-meter-client -- \
  --endhost-api http://127.0.0.1:31000/ \
  --server '[2-ff00:0:212,127.0.0.1]:59218'
```

The client needs a reachable Modbus meter (default `10.10.0.2:502`; override with
`--meter-ip` and `--meter-port`). It continuously reads measurements every 200 ms and
sends a JSON array when either `--batch-size` (default 10) or `--batch-timeout-ms`
(default 1000 ms) is reached. The server acknowledges accepted batches and updates its
meter and device state. See [Measurement ingestion](#measurement-ingestion) for the payload.

Note that the port of the server address (`59218` above) is assigned by the SNAP and is
different on every start, so take the address from the output rather than from this README.

## Run it between the Pi and the laptop

By default the simulated network is only reachable on the laptop itself. Give the server the
address of the interface the Pi can reach, for example the WLAN address of the laptop:

```bash
cargo run -p pq-meter-server -- --bind-ip 192.168.1.42
```

The printed URLs and addresses now use that IP address. Run the client on the Pi with them
(the binary gets there by [cross compiling](#cross-compiling-for-the-raspberry-pi-5)):

```bash
./pq-meter-client \
  --endhost-api http://192.168.1.42:31000/ \
  --server '[2-ff00:0:212,192.168.1.42]:59218' \
  --meter-ip 10.10.0.2
```

The server binds these ports on the address you pass, and all of them have to be reachable
from the Pi:

| Port  | Protocol | What it is                                           |
| ----- | -------- | ---------------------------------------------------- |
| 31000 | TCP      | endhost API of the gateway AS — the client uses this  |
| 31001 | TCP      | endhost API of the server AS — used inside the laptop |
| 31010 | TCP      | SNAP control plane, gateway AS                       |
| 31011 | UDP      | SNAP data plane, gateway AS                          |
| 31020 | TCP      | SNAP control plane, server AS                        |
| 31021 | UDP      | SNAP data plane, server AS                           |

If the client hangs or reports a connection error, the usual cause is a firewall on the
laptop that blocks these ports:

* **macOS** asks once, in a dialog that is easy to miss. Allow incoming connections for the
  binary, or check *System Settings → Network → Firewall*.
* **Windows** shows a similar dialog on the first start. Allow the binary for private
  networks; if the dialog was dismissed, add the rule in *Windows Defender Firewall*.
* **Linux** does not ask. If a firewall is running (`sudo ufw status`,
  `sudo firewall-cmd --state`), open the ports above, or stop the firewall while you work.

Two more things to check when the ports look fine:

* A **VPN** on the laptop can capture the route to the network of the Pi. The packets of the
  Pi still arrive, but the answers of the laptop leave through the VPN and never come back.
  Check with `ip route get <pi-ip>` on Linux or `route -n get <pi-ip>` on macOS that the
  answer leaves through your WLAN interface, and disconnect the VPN while you work.
* The Pi and the laptop have to be on the **same network**, and it must not be a guest WLAN —
  those often block traffic between devices.

## Measurement ingestion

`POST /edh/v1/hello` (or the configured `--path`) accepts a single measurement object
or a non-empty JSON array in the client's format:

```json
[
  {
    "total_power": 100.0,
    "systime": 123456,
    "frequency_hz": 50.0,
    "l1": {
      "voltage_v": 230.0,
      "current_a": 0.5,
      "real_power_w": 100.0,
      "apparent_power_va": 115.0,
      "reactive_power_var": -10.0,
      "cos_phi": 0.9,
      "real_energy_consumed_wh": 1234.0,
      "thd_voltage_pct": 1.85,
      "thd_current_pct": 2.0
    },
    "l2": { "…": "same nine fields" },
    "l3": { "…": "same nine fields" },
    "totals": {
      "real_power_w": 100.0,
      "apparent_power_va": 115.0,
      "reactive_power_var": -10.0
    }
  }
]
```

`total_power` must be a finite number in watts and is the authoritative input for device
detection. The client fills it from the meter's own three-phase real-power sum (register
19026), not from L1 alone. It is **signed**: a site that exports more than it draws reports
a negative total, which is a normal reading in a decentralized grid, and detection works on
the change between readings, so it keeps working below zero. The server does not derive the
total from the phases.

For compatibility, every field except `total_power` may be omitted. When supplied,
`systime` must be a signed 32-bit integer, matching the client's raw meter register; `l1`,
`l2`, `l3` must each contain all nine numeric fields shown above, and `totals` all three.
Unknown fields are ignored. Legacy aliases `power`, `power_watts`, and `power_l1_n`, and the
legacy object `{"message":"100"}`, remain supported.

A single measured value may be `null`, meaning the meter reported it as unavailable rather
than as a number. This is not a theoretical case: a UMG 605-PRO wired up on one phase
answers the THD registers of the other two with NaN, and JSON has no way to spell that. Null
is kept as "not measured" instead of being flattened to zero, which would claim a
distortion-free phase that was never measured. The distinction only applies to individual
values — `frequency_hz` and the fields inside `l1`, `l2`, `l3` and `totals`. The blocks
themselves, and `systime`, are structure rather than measurement: an explicit `null` there
is rejected, as are strings and non-finite numbers anywhere.

The entire request is validated before any state changes. Empty batches, malformed JSON,
and invalid readings return HTTP 400 without updating either meter or decision state.
Accepted readings are processed in array order, with no timestamp sorting or deduplication.
Every sample advances the reading count and decision method, including settling across
batch boundaries. Concurrent requests cannot interleave samples within a batch.

Successful requests return HTTP 200 with a compact text acknowledgement. A single reading
keeps the baseline/device-change response; multiple readings return, for example,
`accepted 10 readings: 1 added, 0 removed`. This fits the client's 4096-byte response limit.

The latest complete accepted reading is available as `latest_reading` in `GET /api/v1/state`.
Missing context is omitted from that object; a value the meter could not determine stays
`null` there, so the two remain distinguishable. Before any readings it is `null`. A later
power-only reading replaces the previous context rather than retaining stale values.
The server receipt timestamp (`last_received_at`) remains separate from meter `systime`.
Only the latest reading is retained; the dashboard UI and device algorithms still use
`total_power`.

## Local dashboard

Run the server, then open [PQ Monitor](http://127.0.0.1:8080/) in a browser:

```bash
cargo run -p pq-meter-server
# Choose another port (0 selects an available port, printed at startup):
cargo run -p pq-meter-server -- --dashboard-port 8081
# Run only the SCION receiver:
cargo run -p pq-meter-server -- --no-dashboard
```

The dashboard always binds to `127.0.0.1`, independently of `--bind-ip`, so making SCION
reachable from the gateway does not expose the dashboard to the network. HTML, CSS, and
JavaScript are embedded in the Rust binary; there is no frontend build, CDN, or extra
process to start. Recompile the server after editing an asset.

The overview refreshes once per second and shows:

- The latest accepted total-power reading, in watts.
- Inferred active devices and their summed nominal catalog power.
- All catalog entries, the latest device addition/removal, and the selected decision method.
- Accepted reading count and the server receipt time of the last reading.

Before a reading arrives, measurements show as unavailable. After ten seconds without an
accepted reading, the view marks the retained state as stale. If the dashboard API becomes
unreachable, it preserves the last view, marks it disconnected, and retries automatically.
Polling pauses in background tabs and resumes when they become visible.

Device activity is inferred from **changes** in total power. The first reading establishes
a baseline; a device already on at startup is not automatically identified. “Not detected”
is therefore not a confirmed off state, and nominal catalog power is not an individual
measurement. The receiver validates and retains the latest gateway measurement, including
voltage, frequency, and other context, in the read API's `latest_reading` field. These extra
fields are not displayed by the overview UI.

### Dashboard architecture and future history

`meter.rs` owns the current application state. The SCION ingestion handler updates it only
after a reading has been decoded and accepted. It records receipt time, reading count, and
the latest actual device change alongside the existing power and device state.

`dashboard/model.rs` defines the dashboard's serializable read model and `SnapshotSource`
interface. `LiveMeterSource` takes a consistent copy of meter state under a short lock,
then builds the response after releasing it. The dashboard never calls the decision engine
or mutates the meter. Alternative sources can implement the same interface.

`dashboard/mod.rs` serves the embedded assets and read-only `GET /api/v1/state` endpoint.
The response includes `schema_version`, server timestamps, the stale threshold, power,
reading count, device states, the last change, and the latest full measurement. Responses
disable caching. No reading is represented as JSON `null`, not zero. An unavailable store
returns a JSON error with HTTP 503; mutation requests are not supported.

The frontend separates HTTP requests (`assets/api.js`), polling and lifecycle
(`assets/app.js`), and rendering (`assets/overview.js`). Add future views alongside the
overview, with their own API functions and navigation entries.

There is **no historical storage yet**: only one current state and the last device change
are retained, and a server restart resets them. To add history, record timestamped accepted
readings and device changes at the ingestion boundary, use bounded retention or persistent
storage, and expose a separate time-range/paginated endpoint (for example,
`GET /api/v1/history`). Keep historical queries separate from the lightweight live snapshot;
do not grow the shared state or live response into an unbounded event list.

Validation for this module:

```bash
cargo test -p pq-meter-server
cargo clippy -p pq-meter-server --all-targets -- -D warnings
cargo fmt -p pq-meter-server -- --check
```

## Read from the meter

The third program talks to the meter rather than to SCION. `umg605-modbus-client` is a small
Modbus TCP client for the UMG 605-PRO, with a `pinger` binary that reads a few values in a
loop so you can check that the meter answers:

```bash
cargo run -p umg605-modbus-client --bin pinger -- --ip 192.168.1.50 monitor
```

```text
Voltage L1: 230.12 V, Current L1: 1.83 A, Power L1-N: 420.75 W
```

The `snapshot` subcommand prints the complete set the gateway sends instead, which is the
quickest way to see what the meter's wiring actually delivers on each phase:

```bash
cargo run -p umg605-modbus-client --bin pinger -- --ip 192.168.1.50 snapshot
```

```text
systime 1789069140 | 33.24ms | f 50.01 Hz | Sum3 P 47.61 W, S 113.47 VA, Q -44.79 var
  L1: U 239.48 V, I 0.47 A, P 47.61 W, ..., THD-U 1.91 %, THD-I 141.47 %
  L2: U 0.00 V, I 0.00 A, P 0.00 W, ..., THD-U n/a %, THD-I n/a %
  L3: U 0.00 V, I 0.00 A, P 0.00 W, ..., THD-U n/a %, THD-I n/a %
```

`n/a` is a value the meter reports as unavailable: with only L1 connected, the harmonic
distortion of a current that is not flowing has nothing to be measured against.

The meter has to be reachable from the machine you run this on, which on the day means the Pi.

In your own code the entry point is `Umg605ProClient`: `connect_tcp` opens the connection,
and `voltage_l1`, `current_l1` and `power_l1_n` each read one measured value.

`snapshot` is what the gateway uses instead. Every measured value of the meter lies in the
contiguous range 19000–19121, which is 122 registers and therefore fits in a single Modbus
read; a snapshot costs one read for that block and one for the clock, no matter how many
values it carries, and the values in it are consistent with each other. It returns all three
phases — voltage, current, real/apparent/reactive power, cos phi, energy and both THD
figures — together with the three-phase sums the meter measures itself. Values the meter
cannot determine come back as NaN rather than as an error, so check `is_finite` before using
one.  

Which register holds which value is in the [register map of the meter][register-map].

You can look at the example functions provided in the library to see how to read other values.

[register-map]: https://assets.janitza.com/ce18jq9ih0x6/b83ae2356a42a682591109/ef2bc2b24a6b7c77de4dbda20e43cebf/janitza-mal-umg605pro-en.pdf

## Installing the build tools

You need Rust, cmake and a C/C++ compiler. The last two are needed because the TLS library
in the dependency tree is C code that is built from source.

Install Rust with [rustup](https://rustup.rs/). It reads `rust-toolchain.toml` and fetches
the version this repository is built with automatically.

### Linux

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
sudo apt install build-essential cmake        # Debian and Ubuntu
```

### macOS

```bash
brew install rustup-init && rustup-init
xcode-select --install                        # C/C++ compiler
brew install cmake
```

Rust can also be installed with the same `curl` command as on Linux if you do not use
Homebrew.

## Bootstrapping the SD card for the Raspberry Pi 5

Use the *Raspberry Pi Imager*, which writes the operating system to the SD card and can
pre-configure the first boot.

### Linux (Ubuntu)

```bash
sudo apt install rpi-imager
```

### macOS

```bash
brew install --cask raspberry-pi-imager
```

### Writing the card

1. Start the imager and choose device *Raspberry Pi 5*.
2. As the operating system, open *Raspberry Pi OS (other)* and choose *Raspberry Pi OS Lite
   (64-bit)*. Lite leaves out the desktop, which you do not need on a gateway you reach over
   SSH. The 64-bit version matters: the cross compilation below builds for a 64-bit target.
3. Choose your SD card and continue to *Edit settings*. Set a hostname, a user name and
   password, your WLAN network, and enable SSH under *Services*. This is what saves you from
   needing a keyboard and monitor for the Pi.
4. Write the card, put it into the Pi, and power it up. After a minute you can log in:

```bash
ssh <user>@<hostname>.local
```

## Cross compiling for the Raspberry Pi 5

The Pi is slow at compiling, so build on your laptop and copy the binary over. The target is
`aarch64-unknown-linux-gnu`. We use [`cargo-cross`](https://github.com/zijiren233/cargo-cross),
which downloads the needed toolchain itself and needs no container engine.

### Install cargo-cross

Same on Linux and macOS:

```bash
cargo install cargo-cross
```

### Build the client

```bash
cargo cross build --release -p pq-meter-client --target aarch64-unknown-linux-gnu
```

The first build takes a few minutes because the toolchain is downloaded. The binary ends up
in `target/aarch64-unknown-linux-gnu/release/pq-meter-client`.

### Copy it to the Pi

```bash
scp target/aarch64-unknown-linux-gnu/release/pq-meter-client <user>@<hostname>.local:
```

Then run it on the Pi as shown [above](#run-it-between-the-pi-and-the-laptop).

The server can be cross compiled the same way (`-p pq-meter-server`), but you will not
normally need it on the Pi. The `pinger` does belong there, since the meter is on the network
of the Pi:

```bash
cargo cross build --release -p umg605-modbus-client --bin pinger --target aarch64-unknown-linux-gnu
```

## Where to continue

* **Read the meter.** `pq-meter-client` already depends on `umg605-modbus-client`, so
  `use umg605_modbus_client::Umg605ProClient;` in `crates/pq-meter-client/src/main.rs` is
  enough to read a value and send it on. Check the meter with the `pinger` [first](#read-from-the-meter).
* **Send your own data.** The client sends batches of measurements from
  `crates/pq-meter-client/src/main.rs`. Extend its payload and the typed decoder in
  `crates/pq-meter-server/src/input.rs` together. Keep the one `scion_http3::Client` so
  requests reuse its connection pool.
* **Receive your own data.** `crates/pq-meter-server/src/api.rs` validates complete
  batches before feeding each reading to the decision method and shared meter state.
  Replace `ReadingDecoder` to support another wire format, or add routes to the axum app.
* **Look at paths.** SCION lets an application see and choose the paths to a destination. The
  [academy](https://learn.anapaya.net/docs/academy/scion-sdk/) explains how paths are built,
  and `crates/pq-meter-server/src/network.rs` is where you would add more autonomous systems
  and links to have more than one path to play with.

Two shortcuts in this template are fine for a hackathon but not for a product: the server
generates a self-signed certificate on every start and the client does not verify it, and
both sides use a development token to attach to the SNAP.
