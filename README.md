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
  pq-meter-client/         Runs on the gateway
    src/main.rs            Sends one message and prints the answer
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

The client prints `server answered 200 OK: ok`, and the server prints what it received:

```text
received: {"message":"Hello from Energy Data Hackdays 2026"}
```

The server keeps running; the client sends one message and exits. Use `--message` to send
something else.

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
  --message '{"my":"first measurement"}'
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

The meter has to be reachable from the machine you run this on, which on the day means the Pi.

In your own code the entry point is `Umg605ProClient`: `connect_tcp` opens the connection,
and `voltage_l1`, `current_l1` and `power_l1_n` each read one measured value.  

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
* **Send your own data.** The client sends a JSON object with one field. Build whatever
  structure your measurements need in `crates/pq-meter-client/src/main.rs`, and send in a
  loop instead of once. Keep the one `scion_http3::Client`: it holds a pool of connections,
  so every request after the first one reuses the connection that is already up.
* **Receive your own data.** The server prints the request body as text
  (`crates/pq-meter-server/src/api.rs`). It is a normal [axum](https://docs.rs/axum)
  application, so you can add routes, and let axum parse your JSON into a type by taking
  `axum::Json<YourType>` as the handler argument.
* **Look at paths.** SCION lets an application see and choose the paths to a destination. The
  [academy](https://learn.anapaya.net/docs/academy/scion-sdk/) explains how paths are built,
  and `crates/pq-meter-server/src/network.rs` is where you would add more autonomous systems
  and links to have more than one path to play with.

Two shortcuts in this template are fine for a hackathon but not for a product: the server
generates a self-signed certificate on every start and the client does not verify it, and
both sides use a development token to attach to the SNAP.
