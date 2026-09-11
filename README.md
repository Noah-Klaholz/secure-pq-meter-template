# Secure power quality metering via SCION

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

## Security status: this is a demonstrator, not a deployment

The name says "secure", and the security this prototype demonstrates is SCION's: readings
travel over a path-aware network that can move around a failure, and a real deployment can
refuse an unauthorized sender at the network layer instead of at the application. **The
authentication around that is stubbed out**, so the prototype should not be pointed at a
real installation as it stands.

Every shortcut is marked in the code. To see the current list:

```bash
grep -rn "TODO(security)" crates/
```

What is stubbed, and what a deployment would need instead:

| Shortcut | Where | What it should be |
| --- | --- | --- |
| Dummy SNAP token on the gateway | `pq-meter-client/src/uplink.rs`, `link.rs` | A token issued to *this* gateway by the AA (authentication and authorization service), so the network refuses an unknown device before its packets reach the application |
| PocketSCION development token on the receiver | `pq-meter-server/src/main.rs` | A credential issued to the receiver |
| Gateway does not verify the receiver (`verify_peer(false)`) | `pq-meter-client/src/uplink.rs` | Pin the backend certificate, or verify against a CA the gateway is provisioned with. The connection is encrypted, but the gateway does not know who is on the other end |
| Self-signed certificate regenerated on every start | `pq-meter-server/src/api.rs` | A stable certificate the gateway can pin — regenerating it is what forces the gateway to skip verification in the first place |
| Ingest endpoint is unauthenticated | `pq-meter-server/src/api.rs` | Anything that can reach the SNAP can post readings, and nothing ties a batch to the meter it claims to come from. Reject unauthorized gateways at the network layer, and give the reading an identity the receiver checks |
| Dashboard has no authentication | `pq-meter-server/src/dashboard/mod.rs` | Bound to `127.0.0.1`, so being on the machine is the only thing protecting it. Exposing it needs authentication and TLS |
| Modbus TCP to the meter is unauthenticated and unencrypted | `umg605-modbus-client/src/lib.rs` | The protocol offers nothing here, so the meter belongs on an isolated link to the gateway. This segment is the one part of the path SCION does not cover |

Two further properties are by design rather than shortcuts, but are worth knowing:

- The **transport figures the dashboard shows** — path in use, queued readings, latency,
  failover count, meter reconnects, readings lost — are *reported by the gateway about
  itself*, because the receiver cannot observe them. They are not evidence. The one thing the
  receiver does not take on trust is the link state, which it judges from when a batch
  actually arrived.
- The **device catalog is a demonstration**, and device inference is a guess from changes in
  power. It is not metering-grade, and nothing billable should be derived from it.

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
    src/main.rs            Command line interface; the acquisition and upload tasks
    src/meter.rs           The Modbus connection, and how it re-establishes itself
    src/spool.rs           The bounded queue of readings awaiting acknowledgement
    src/uplink.rs          The HTTP/3 connection, and when to rebuild it
    src/retry.rs           Jittered exponential backoff, shared by both of the above
  umg605-modbus-client/    Reads data from a UMG 605-PRO power quality meter over Modbus TCP
    src/lib.rs             The Modbus TCP client and the registers it reads
    bin/pinger.rs          Example binary that reads values from the meter
Cargo.toml                 Workspace, pins the SCION SDK version
rust-toolchain.toml        Rust version used to build this repository
.pre-commit-config.yaml    Git hooks: formatting, lints, tests and file hygiene
.github/workflows/ci.yml   The same checks, run on every push and pull request
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

The client reads a Modbus meter (default `10.10.0.2:502`; override with `--meter-ip` and
`--meter-port`) every 200 ms and sends a JSON array when either `--batch-size` (default 10)
or `--batch-timeout-ms` (default 1000 ms) is reached. The server acknowledges accepted
batches and updates its meter and device state. See
[Measurement ingestion](#measurement-ingestion) for the payload.

Neither the meter nor the receiver has to be up when the gateway starts, and neither taking
it down stops it — see [Surviving a failure](#surviving-a-failure).

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
The optional boolean `heartbeat` marks a keepalive reading, which is recorded and plotted
but left out of device inference (see [Reading history](#reading-history)); omitted means
a settled change.
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
Only the latest reading is retained. `total_power` is the authoritative signal for every
decision method; the `adaptive` method additionally uses the L1 reactive power and THD_I
from `l1` when present, and falls back to power-only behaviour when it is not.

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
process to start. Recompile the server after editing an asset. The charts are inline SVG
drawn by `assets/charts.js`: the content security policy allows scripts from this origin
only, so there is no charting library to load.

The dashboard has three views, reachable through the sidebar (a horizontal navigation
on small screens): **Power Quality** for live measurements, quality events and transport;
**Connected Devices** for the device catalog, labels and latest device change; and
**History** for the rolling charts and recent measurements. The selected view is retained
in the URL, and all views refresh automatically.

In **Connected Devices**, select **Rename**, enter a name (1–80 characters), then select
**Save name**. Names are stored on the receiver in `device-labels.json`, so every browser
sees the same names. Use `--device-labels-file /path/to/devices.json` to choose a persistent
location; relative paths resolve from the server's working directory. Each successful save
atomically retains the labels and all currently learned appliance signatures, including
their IDs and distortion current. Restarting restores those identities and relearns the
idle background from the first reading. As before, appliances already on at startup are
part of that background until they can be inferred from later changes. Unsaved device discoveries remain session data. Measurement history is archived separately. Save errors leave the previous label intact.

The live state refreshes once per second. Across the three views, the dashboard shows:

- **Grid frequency**, against the EN 50160 band it is judged in.
- **Net real power**, signed, with an import/export badge. This is the meter's own
  three-phase sum, so an exporting site reads negative.
- **Supply status**, summarising the power-quality events below.
- **SCION link** state, judged from when a reading last arrived.
- **Per phase**: voltage, current, real power, cos φ, and both harmonic distortion
  figures, with any value outside its limit coloured.
- **Rolling 60-second charts** of frequency, the three phase voltages, and real power,
  with the allowed band shaded behind the trace.
- **Power quality events**: every measurement outside its limit, in words.
- **SCION transport**: the path in use, last acknowledgement latency, queued readings,
  path failover count, meter reconnects, and readings the gateway admits it lost.
- Device inference, kept as a secondary panel.

### Power-quality limits

The limits live in `quality.rs` rather than in the dashboard's JavaScript, so there is one
definition, covered by tests, that the UI only colours in. They follow EN 50160: voltage
within 10% of 230 V, frequency within 49.5–50.5 Hz, and voltage distortion at most 8%.

Two judgements are deliberately narrower than "compare against the limit":

- A phase with **nothing wired to it** reads 0 V. That is an absence of supply on an unused
  terminal, not an undervoltage event, so a phase drawing no current is not judged against
  the voltage band. Otherwise a bench meter connected on L1 would report two permanent
  faults that bury the real measurement.
- **Current distortion** is only judged above 1 A. THD_I is a ratio against a fundamental
  that approaches zero when a phase is idle, so the meter on the bench reports over 100%
  for the few hundred milliamps it draws at rest. It is shown as context at any current and
  raised as a warning, never a violation, above that threshold — EN 50160 sets no
  current-distortion limit.

A value the meter reported as unavailable is never a violation: an unknown value is not a
measured excursion.

### Device fingerprinting (`--decision-method`)

Device activity is inferred from **changes** in electrical signature, not from absolute
power, and is kept as a secondary panel. The first accepted reading establishes a baseline;
a device already on at startup is folded into the background and not identified
individually, so “not detected” is not a confirmed off state. A reading marked
`"heartbeat": true` is recorded but skipped for inference.

`decision.rs` provides interchangeable inference methods. The default is `adaptive`:

* **`adaptive` — training-free online NILM.** No predefined catalog. The first accepted
  reading is taken as the always-on **background** (here, the two Raspberry Pis, ~23 W)
  and never reported as an event. Every later *settled* step change is an **edge** in
  `(ΔP, ΔQ, ΔI_dist)` space, where `I_dist = I_rms · THD_I / 100` is the harmonic
  (distortion) current — the three quantities that stay roughly additive across parallel
  loads. An edge is matched by normalised nearest-neighbour distance to a device learned
  earlier; an unmatched turn-on adds a new one (`Device N (~W)`). Turn-offs match the
  negated edge against active devices. This is Hart's P–Q signature approach with a
  harmonic axis. It trusts the gateway's own settling window, so it decides on a
  single sample per edge (`required_samples = 1`); the learned set is bounded (32).
* **`settled` / `immediate` / `multi-feature`** keep the static `DUMMY_DEVICE_CATALOG` and
  match power (and, for `multi-feature`, Q and THD) deltas against fixed nominal profiles.
  These are only useful when the catalog has been hand-calibrated for the devices present.

The inferred-power sum on the dashboard adds the background plus each active device's ΔP,
so under `adaptive` it approximates a real disaggregation of the measured total.

### Transport telemetry

The receiver cannot see most of the link for itself. Which path the packets took, how many
readings are waiting on the gateway, and how often it has failed over are facts about the
*sending* end. The gateway reports them as headers on each batch, which leaves the
measurement body exactly as documented above:

| Header | Meaning |
| --- | --- |
| `x-pq-scion-path` | The SCION path in use, e.g. `1-ff00:0:132 1>3 2-ff00:0:212` |
| `x-pq-queued-readings` | Readings buffered on the gateway and not yet acknowledged |
| `x-pq-ack-latency-ms` | Round trip of the gateway's *previous* batch |
| `x-pq-failover-count` | How often the gateway has changed path since it started |
| `x-pq-dropped-readings` | Readings the gateway gave up on: shed from a full queue, or refused here |
| `x-pq-modbus-reconnects` | How often the gateway had to re-establish its connection to the meter |

`x-pq-dropped-readings` is the one that matters when reading the archive: it is the gateway
admitting there is a hole in it. Without it a gap is indistinguishable from an installation
that had nothing to report, so the dashboard flags any non-zero value rather than showing it
as just another count.

Every header is optional and independently parsed. A malformed value is dropped rather than
failing the batch — measurements must not be rejected over their metadata — and a gateway
that sends none of them leaves the panel empty rather than showing zeros that look measured.
The path string is bounded and stripped of control characters before being stored, and the
dashboard renders it as text: it arrives from the network.

The **link state** is the one part the receiver judges for itself, from when a batch last
arrived. A gateway that has stopped sending cannot claim to be connected.

### Surviving a failure

A gateway is only useful if it outlives the things around it. The meter and the receiver are
separate machines on links the gateway does not control, and both will go away at some point.
Neither takes the gateway with it.

**Acquisition and upload run as separate tasks.** They have to: a read happens on the cadence
the meter dictates, a send takes as long as the network takes, and running both in one loop
means a slow send silently skips readings. They share one queue.

**Readings are removed only once the receiver acknowledges them.** A batch handed to the
network stays queued until the server answers for it. What the answer was decides what
happens next:

| Answer | What the gateway does |
| --- | --- |
| `2xx` | The receiver has them; they leave the queue |
| `4xx` | Malformed, and resending changes nothing — discarded, counted, and logged loudly |
| `5xx`, timeout, no answer | Kept, and sent again after a backoff |

The distinction matters in both directions. Retrying a `4xx` forever would block every
reading behind a batch that can never be accepted; discarding a `503` would throw away good
readings because the *receiver* failed to write them to disk.

**The queue is bounded** by `--queue-capacity` (default 5000 readings, roughly an hour of an
idle installation). When it is full the **oldest** reading is dropped, so the dashboard keeps
showing the present and the gap lands in the archive instead — and `x-pq-dropped-readings`
says so.

**The meter reconnects on its own.** Connecting is deferred to the first read, so the gateway
can start before the meter does; a read that fails drops the connection and retries with a
backoff from 100 ms to 5 s. A meter that blinks is a gap in the data, not the end of the
program.

**So does the link.** `scion_http3::Client` pools connections, and a pooled connection
outlives the receiver it points at: once the receiver has restarted, every request on that
connection times out, forever. Retrying alone would never recover — so after three batches in
a row fail to reach the receiver, the gateway rebuilds the connection (`uplink.rs`). This was
invisible while a failed batch was discarded on the spot, because nothing accumulated to show
it; it is the difference between a queue that drains when the receiver comes back and one
that does not.

**Retries are jittered.** Delivery backs off from 500 ms to 30 s, ±25 %, so a fleet of
gateways coming back after a shared outage does not retry in lockstep.

What this does *not* survive is the gateway itself dying: the queue is in memory, so a crash
or a power cut loses whatever had not been acknowledged yet. Persisting it is the next step,
and the queue is deliberately behind a narrow interface (`spool.rs`) so that a disk tier can
be added without either task changing.

To watch it work, cut the link while the client is running:

```bash
make run-server          # laptop
make client              # Pi
make stop-server         # queue depth climbs on the dashboard, nothing is lost
make run-server          # the backlog drains
```

### Reading history

`GET /api/v1/history` serves the latest recorded 60-second window, oldest first, as
the narrow per-sample shape the charts plot. It is kept apart from `GET /api/v1/state`
because the live view is polled every second and has to stay small while the series grows
with the window; the dashboard fetches it half as often. The chart cache is bounded in memory
(`History::bounded`); startup restores that cache from the persistent measurement archive.

Because the gateway only sends readings that cross its noise threshold, an idle
installation would otherwise produce an empty chart and a link that reports itself stale
while it is perfectly healthy. The gateway therefore also sends a reading when nothing has
changed for `--heartbeat-ms` (2 s by default; `0` restores pure change-triggered sending).

Such a reading carries `"heartbeat": true`, and **device inference skips it**. The gateway
waits for a level to settle before calling it a change, so a reading it sends in the
meantime can sit anywhere between the old level and the new one. That value is a valid
measurement — it is plotted and judged against the limits like any other — but it is not
evidence of a device. Matching it would name the wrong device *and* leave the real change
to be measured from the intermediate value, so the settled change that follows would be
mis-matched too. A reading without the field is a settled change, which is what older
gateways send.

### Dashboard architecture

`meter.rs` owns the current application state. The SCION ingestion handler updates it only
after a reading has been decoded and accepted. It records receipt time, reading count, and
the latest actual device change alongside the existing power and device state.

`dashboard/model.rs` defines the dashboard's serializable read model and `SnapshotSource`
interface. `LiveMeterSource` takes a consistent copy of meter state under a short lock,
then builds the response after releasing it. The dashboard never calls the decision engine. Its rename operation updates only user
labels and persists them with the learned signatures. Alternative sources can implement
the same interface; sources without rename support return HTTP 503.

`quality.rs` holds the limits and decides which measurements breach them. `transport.rs`
parses what the gateway reports about the link. Neither knows about HTTP or the dashboard.

`dashboard/mod.rs` serves the embedded assets and the two read-only endpoints,
`GET /api/v1/state` and `GET /api/v1/history`. The state response includes
`schema_version`, server timestamps, the stale threshold, the power-quality block and the
limits it was judged against, the transport, reading count, device states, the last change,
and the latest full measurement. Responses disable caching. No reading is represented as
JSON `null`, not zero. An unavailable store returns a JSON error with HTTP 503.
`PUT /api/v1/devices/{id}/label` accepts a JSON object with a `name` string and returns the
saved, trimmed name. Invalid names return HTTP 400 and unknown IDs return HTTP 404.
The live state and history endpoints remain read-only.

The frontend separates HTTP requests (`assets/api.js`), polling and lifecycle
(`assets/app.js`), rendering (`assets/overview.js`), and chart geometry
(`assets/charts.js`). Hash navigation in `app.js` switches the three view sections, and
the rename dialog stays independent of the polling-rendered device rows.

History is **persistent by default** in the local SQLite database
`data/history.db`. Use `--history-file /path/to/history.db` to select another database
location. Every accepted measurement is committed directly to SQLite before live state,
device inference, or the HTTP acknowledgement is updated. The complete measurement object
is retained, including phases, nullable values, meter timestamps, and heartbeat flags.

On startup, an existing `measurement-history.jsonl` file is automatically imported into
SQLite in one transaction. A migration metadata record and source hash make the import
idempotent across restarts; if the source changes after migration, startup refuses to
duplicate-import it. The original JSONL file is kept and is never deleted automatically.
Migration failures leave that file untouched. No SQLite server or external service is
needed; the Rust build uses SQLite bundled with `rusqlite`.

The database keeps the full archive and indexes receipt timestamps. The History tab queries
only the latest 60-second window, while the in-memory chart cache retains at most 600
readings. Saved measurements are not replayed into live device inference or transport state:
each receiver session establishes a fresh baseline from new readings. The API adds
`persistent` and `stored_readings` to the history response.

To inspect the database locally:

```bash
sqlite3 data/history.db
```

Useful queries include:

```sql
SELECT COUNT(*) FROM measurements;
SELECT * FROM measurements ORDER BY received_at DESC LIMIT 10;
```

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

## Git hooks and CI

Every push and pull request runs `.github/workflows/ci.yml`, which checks four things:

| Check | Command |
| --- | --- |
| Formatting | `cargo fmt --all --check` |
| Lints | `cargo clippy --workspace --all-targets --all-features -- -D warnings` |
| Tests | `cargo test --workspace` |
| File hygiene | `pre-commit run --all-files` — no trailing whitespace, LF line endings only, a newline at the end of every file |

The same checks are available as git hooks, so a commit that would fail CI fails on your
machine first. They are managed by [pre-commit](https://pre-commit.com/), a Python tool
that reads `.pre-commit-config.yaml`.

### Installing the hooks

Install `pre-commit` once per machine:

```bash
pipx install pre-commit          # or: pip install --user pre-commit
sudo apt install pre-commit      # Debian and Ubuntu
brew install pre-commit          # macOS
```

Then install the hooks into your clone:

```bash
make hooks
```

That runs `pre-commit install`, which writes `.git/hooks/pre-commit`. Hooks live in
`.git/`, so this is per clone: everyone who clones the repository runs it once.

### What runs when

Everything runs on `git commit`: the file hygiene hooks over the staged files, then `cargo
fmt --check`, `cargo clippy` and `cargo test` over the workspace whenever a `.rs` file is
part of the commit. The suite finishes in well under a second on an already-built
workspace, so it is worth having in front of every commit rather than only the push.

The whitespace and line-ending hooks *fix* what they find and fail the commit; re-stage
the corrected files with `git add` and commit again.

Useful commands:

```bash
pre-commit run --all-files       # check the whole tree, not just staged files
make lint                        # cargo fmt --check and clippy, without the hooks
git commit --no-verify           # skip the hooks for one commit
pre-commit autoupdate            # bump the pinned hook versions in the config
```

Line endings have a second guard: `.gitattributes` sets `* text=auto eol=lf`, so git
stores LF whatever your working tree checks out as. The `mixed-line-ending` hook catches
files that reach git with CRLF in them anyway.

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

The `.local` name is mDNS, which only reaches as far as the local link. It works when the Pi
and the laptop share a network, and fails on a campus or guest network that puts clients in
different routed subnets — which is what `fhnw-public` does. Check whether the network's own
DNS knows the Pi instead; a DHCP server that registers client hostnames gives you a name that
works from anywhere on the network:

```bash
getent hosts <hostname>          # e.g. -> 10.0.5.23 <hostname>.example.ac.uk
```

Prefer that name over the address wherever you can. A short DHCP lease means the Pi comes
back on a different address after any gap longer than the lease, and on a large network that
can be a different subnet as well; a name follows it, an address does not. Set `PI_HOST` in
the `Makefile` to whichever of the two works for you.

## Cross compiling for the Raspberry Pi 5

The Pi is slow at compiling, so build on your laptop and copy the binary over. The target is
`aarch64-unknown-linux-gnu`. We use [`cargo-cross`](https://github.com/zijiren233/cargo-cross),
which downloads the needed toolchain itself and needs no container engine.

The gateway and the receiver share the measurement schema, so **deploy them together**. A
gateway one version behind is rejected with a message naming the field it is missing, for
example `reading 1: missing field \`thd_voltage_pct\``; rebuild and redeploy the client
when that appears.

### Install cargo-cross

Same on Linux and macOS:

```bash
cargo install cargo-cross
```

`cross` runs the build in a container and needs Docker or Podman. Without it, `make
build-client` falls back to the host toolchain, which needs an aarch64 GCC and the Rust
std for the target (Debian/Ubuntu: `gcc-aarch64-linux-gnu`, Arch: `aarch64-linux-gnu-gcc`).

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

`make deploy-client` does the same thing and uses `PI_HOST`, so it follows whichever name you
settled on [above](#writing-the-card).

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
