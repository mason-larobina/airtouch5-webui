# airtouch5-webui -- AirTouch 5 web UI

`airtouch5-webui` is a small web server that wraps the
[`airtouch5`](https://github.com/mason-larobina/airtouch5) crate. It discovers an
AirTouch 5 console on your local network, shows its state, and lets you control
AC units and zones from a browser. The UI is server-rendered HTML updated live
over Server-Sent Events (SSE) using [htmx](https://htmx.org) -- there is no
client-side JavaScript framework, no build step, and no app to install.

You point a browser at it; it finds the console; you control your air
conditioning.

![webui-screenshot](webui-screenshot.png)

## Features

- **Automatic discovery.** The server finds the AirTouch 5 console on the LAN
  via UDP auto-discovery and reconnects on its own if the connection drops.
- **Live updates.** Every connected browser is pushed the latest state over
  SSE the moment the console reports a change -- no polling, no refresh
  button needed.
- **System status.** A console card shows the console name, network address,
  AirTouch ID, console ID, firmware version, update availability, and the
  number of AC units and zones.
- **AC control.** Per AC unit: a large ON/OFF power toggle in the card
  header, mode (Auto / Heat / Dry / Fan / Cool -- only the modes the unit
  actually supports are shown), fan speed (with a separate IntelligentAuto
  toggle), and a setpoint stepper.
- **Zone control.** Per zone: power toggle (Off / On / Turbo), a control-mode
  switch between **% airflow** and **temperature setpoint** (temperature only
  for zones with a sensor), and a `+` / `-` stepper that steps the current
  value (5% airflow or 1.0 C setpoint).
- **Bulk zone control.** An "All zones" bar switches every zone's control mode
  at once and applies preset values across the board (airflow 25/50/75/100% or
  temperature 20/21/22/23 C).
- **Automation programs.** Two hard-coded programs you can enable, disable, and
  configure in the UI (below the zones list): **Setpoint auto-off** turns the
  AC(s) off once every on-zone is in temperature mode and has reached its
  setpoint (held for 15/30/60/120 minutes first), and **Idle auto-off** turns the
  AC(s) off after 15/30/60/120 minutes with no control changes. Settings are
  persisted to a JSON file and survive restarts.
- **Two binaries.** `airtouch5-webui` talks to a real console; `airtouch5-webui-mock` serves the
  exact same UI against an in-memory mock, handy for trying the interface
  without hardware.

## Requirements

- An AirTouch 5 console reachable on your LAN (for `airtouch5-webui`).
- A recent Rust toolchain (edition 2024).

`airtouch5-webui` listens on `0.0.0.0:3000` by default, so it is reachable from other
devices on the network. Bind to `127.0.0.1:3000` if you only want local access.

## Building

```sh
cargo build --release
```

The release binaries land at `target/release/airtouch5-webui` and
`target/release/airtouch5-webui-mock`.

## Running

### The real server

```sh
./target/release/airtouch5-webui
```

Then open `http://localhost:3000` (or `http://<this-machine>:3000` from another
device on the same network).

#### Command-line options

| Option                        | Default        | Meaning                                                                                                                                                                                             |
| ----------------------------- | -------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--bind <addr:port>`          | `0.0.0.0:3000` | Address and port the HTTP server listens on.                                                                                                                                                        |
| `--discovery-timeout-ms <ms>` | `3000`         | How long UDP discovery waits for a console response.                                                                                                                                                |
| `--timeout <seconds>`         | off            | Shut down after N seconds (mainly for tests).                                                                                                                                                       |
| `--automation-tick-secs <s>`  | `60`           | How often the automation engine evaluates its programs. `0` disables it.                                                                                                                            |
| `--automation-config <path>`  | XDG config dir | File the automation enable/parameter settings are saved to and loaded from. Defaults to `$XDG_CONFIG_HOME/airtouch5-webui/automation.json` (typically `~/.config/airtouch5-webui/automation.json`). |

Logging is the one env-driven option. Set the tracing filter with `RUST_LOG`;
the default is `airtouch5_webui=info,tower_http=info`. Control actions (every
`POST`) are logged at `info` with the client IP, the action, the response
status, and elapsed time; page, partial, SSE, and asset requests are logged at
`debug`.

### The mock server

```sh
./target/release/airtouch5-webui-mock
```

`airtouch5-webui-mock` serves the same UI against a built-in mock controller that starts
with a representative one-AC / six-zone setup (mirroring the static mockup). It
shares `--bind` and `--timeout` with `airtouch5-webui` but has no
discovery timeout (there is no console to discover). Use it to try the
interface, demo it, or develop UI changes without hardware.

## The web UI

The page is laid out top to bottom:

- **Controller card.** At the bottom: the console name with a `[refresh]`
  button, the console metadata fields (address, AirTouch ID, console ID,
  firmware, update, AC units, zones), and a disconnected status line. The
  line is hidden while the console is reachable and turns into a red
  "Disconnected -- reconnecting..." alarm when it is lost; the server keeps
  the last-known state visible underneath while it reconnects.
- **AC unit cards.** One per AC. The card header carries the unit name and
  a large ON/OFF power toggle; the body has controls for mode, fan speed, and
  setpoint, plus the current ("now") temperature and the setpoint value.
  Unsupported modes and fan speeds are hidden.
- **Zones.** An "All zones" bulk bar sits on top of the zone list, followed by
  one row per zone. Each row shows the zone name, its sensor reading (or "no
  sensor" / "sensor n/a"), a circular power toggle, a `%` / `Temp` mode switch,
  and a `+` / `-` stepper with the current value.
- **Automation.** Below the zones list, a card per program with the parameter
  presets (hold time / idle timeout) and an On/Off enable toggle together in
  the card header. Both programs turn the **AC units** off when they fire.
- **Footer.** A theme selector (three colour palettes: Daylight, Ember, and
  Contrast) and a repository link. The theme is persisted in a cookie and
  applied instantly by setting `data-theme` on `<html>`.

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
  an already-on AC off is always allowed.)

Out-of-range or invalid values come back as a short error line in place of the
control (HTTP 422), so you see the rejection inline.

### Automation programs

Two hard-coded programs run as a background task (one tick per minute by
default) and turn the **AC units** off when their condition is met. Zones are
left untouched.

- **Setpoint auto-off** -- armed only when every on-zone is in temperature
  control mode. It fires when every on-zone's sensor reading has reached its
  setpoint (cooling satisfied / heating satisfied, decided by the owning AC's
  mode) and _stays_ that way for the configured hold time (15/30/60/120
  minutes).
  A brief dip past the setpoint does not trip it.
- **Idle auto-off** -- fires after the configured timeout (15/30/60/120
  minutes) with no control changes. "Control changes" are power, mode, fan
  speed, setpoint, or airflow changes; the live sensor/temperature readings
  drift continuously and are deliberately ignored so a steady room does not
  keep the idle timer alive.

Both programs are disabled by default. Enable them and pick presets in the UI;
settings are saved to the `--automation-config` file (defaulting to the XDG
config dir, e.g. `~/.config/airtouch5-webui/automation.json`)
and reloaded on startup. Away/Sleep AC states are never touched -- only ACs
that are `On` get turned off.

## Troubleshooting

- **"Disconnected -- reconnecting..."** The server could not reach the console.
  Discovery retries with an exponential backoff and reconnects automatically
  once it is back; the cards keep showing the last-known state.
- **A control silently does nothing for a while then recovers.** The console
  can occasionally hang on a single request. `airtouch5-webui` aborts any console call
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
[`airtouch5`](https://github.com/mason-larobina/airtouch5).
