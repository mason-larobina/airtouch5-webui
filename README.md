# aircon -- AirTouch 5 web UI

`aircon` is a small web server that wraps the
[`airtouch5`](https://codeberg.org/kbriggs/airtouch5) crate. It discovers an
AirTouch 5 console on your local network, shows its state, and lets you control
AC units and zones from a browser. The UI is server-rendered HTML updated live
over Server-Sent Events (SSE) using [htmx](https://htmx.org) -- there is no
client-side JavaScript framework, no build step, and no app to install.

You point a browser at it; it finds the console; you control your air
conditioning.

## Features

- **Automatic discovery.** The server finds the AirTouch 5 console on the LAN
  via UDP auto-discovery and reconnects on its own if the connection drops.
- **Live updates.** Every connected browser is pushed the latest state over
  SSE the moment the console reports a change -- no polling, no refresh
  button needed.
- **System status.** A console card shows the console name, network address,
  AirTouch ID, console ID, firmware version, update availability, and the
  number of AC units and zones.
- **AC control.** Per AC unit: power (On / Off / Away / Sleep), mode
  (Auto / Heat / Dry / Fan / Cool -- only the modes the unit actually supports
  are shown), fan speed (with a separate IntelligentAuto toggle), and a
  setpoint stepper.
- **Zone control.** Per zone: power toggle (Off / On / Turbo), a control-mode
  switch between **% airflow** and **temperature setpoint** (temperature only
  for zones with a sensor), and a `+` / `-` stepper that steps the current
  value (5% airflow or 1.0 C setpoint).
- **Bulk zone control.** An "All zones" bar switches every zone's control mode
  at once and applies preset values across the board (airflow 25/50/75/100% or
  temperature 20/21/22/23 C).
- **Two binaries.** `aircon` talks to a real console; `aircon-mock` serves the
  exact same UI against an in-memory mock, handy for trying the interface
  without hardware.

## Requirements

- An AirTouch 5 console reachable on your LAN (for `aircon`).
- A recent Rust toolchain (edition 2024).

`aircon` listens on `0.0.0.0:3000` by default, so it is reachable from other
devices on the network. Bind to `127.0.0.1:3000` if you only want local access.

## Building

```sh
cargo build --release
```

The release binaries land at `target/release/aircon` and
`target/release/aircon-mock`.

## Running

### The real server

```sh
./target/release/aircon
```

Then open `http://localhost:3000` (or `http://<this-machine>:3000` from another
device on the same network).

#### Command-line options and environment variables

| Option                       | Env var                            | Default       | Meaning                                              |
| ---------------------------- | ---------------------------------- | ------------- | ---------------------------------------------------- |
| `--bind <addr:port>`         | `AIRCON_LISTEN`                    | `0.0.0.0:3000`| Address and port the HTTP server listens on.         |
| `--discovery-timeout-ms <ms>`| `AIRCON_DISCOVERY_TIMEOUT_MS`      | `3000`        | How long UDP discovery waits for a console response.  |
| `--timeout <seconds>`        | (none)                             | off           | Shut down after N seconds (mainly for tests).         |

Logging is environment-driven. Set the tracing filter with `AIRCON_LOG` or
`RUST_LOG`; the default is `aircon=info,tower_http=info`. Control actions (every
`POST`) are logged at `info` with the client IP, the action, the response
status, and elapsed time; page, partial, SSE, and asset requests are logged at
`debug`.

### The mock server

```sh
./target/release/aircon-mock
```

`aircon-mock` serves the same UI against a built-in mock controller that starts
with a representative one-AC / six-zone setup (mirroring the static mockup). It
shares `--bind` / `AIRCON_LISTEN` and `--timeout` with `aircon` but has no
discovery timeout (there is no console to discover). Use it to try the
interface, demo it, or develop UI changes without hardware.

## The web UI

The page is laid out top to bottom:

- **Connection banner.** A green "Connected" banner turns red and reads
  "Disconnected -- reconnecting..." if the console is lost; the server keeps
  the last-known state visible underneath while it reconnects.
- **AC unit cards.** One per AC. Each shows the unit name and controls for
  power, mode, fan speed, and setpoint, plus the current ("now") temperature
  and the setpoint value. Unsupported modes and fan speeds are hidden.
- **Zones.** An "All zones" bulk bar sits on top of the zone list, followed by
  one row per zone. Each row shows the zone name, its sensor reading (or "no
  sensor" / "sensor n/a"), a circular power toggle, a `%` / `Temp` mode switch,
  and a `+` / `-` stepper with the current value.

### How control works

Every control is an htmx `POST` that returns the updated fragment for that one
card or row, which the browser swaps straight in. The console then confirms
the change asynchronously, and the same fragment may be re-pushed over SSE --
this is harmless (an idempotent swap) and is what keeps every browser in sync.

### Operating constraints

These come from the AirTouch 5 protocol and are enforced by the server:

- **Setpoint temperatures** must be 10.0 - 25.0 C.
- **Airflow percentages** must be 0 - 100 inclusive.
- A zone **without a temperature sensor** cannot be temperature-controlled:
  its `Temp` button is disabled, and a bulk "All zones" temperature switch
  skips sensor-less zones.
- An **AC will not start while every one of its zones is off** -- starting it
  would run the unit with no open airflow path. Turn a zone on first. (Turning
  an already-on AC off, or using Away/Sleep, is always allowed.)

Out-of-range or invalid values come back as a short error line in place of the
control (HTTP 422), so you see the rejection inline.

## Troubleshooting

- **"Disconnected -- reconnecting..."** The server could not reach the console.
  Discovery retries with an exponential backoff and reconnects automatically
  once it is back; the cards keep showing the last-known state.
- **A control silently does nothing for a while then recovers.** The console
  can occasionally hang on a single request. `aircon` aborts any console call
  that takes longer than 10 seconds, drops the connection, and reconnects, so
  the UI un-sticks itself instead of wedging forever. Check the logs for the
  last interaction before the stall.
- **Nothing happens when I click.** Open the browser console; htmx prints
  swap errors there. Invalid input is reported as a 422 with an error fragment.

## Project layout and contributing

The code is a library plus two thin binaries. Developers should read
**ARCHITECTURE.md** for the connection-actor model, the rendering snapshot,
the htmx/SSE contract, the mock controller, and the test harness.

The upstream AirTouch 5 protocol library is
[`airtouch5`](https://codeberg.org/kbriggs/airtouch5).
