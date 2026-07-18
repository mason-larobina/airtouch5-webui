# Architecture -- `aircon`

This document is for contributors. For user-facing usage see **README.md**.

`aircon` is a library crate plus two thin binaries (`aircon` for a real
AirTouch 5 console, `aircon-mock` for an in-memory controller) that serve a
server-rendered web UI over [htmx](https://htmx.org) with live updates pushed
over Server-Sent Events (SSE). It wraps the
[`airtouch5`](https://codeberg.org/kbriggs/airtouch5) crate.

The central design choice: **one long-lived task owns the non-`Clone`
`AirTouch5` handle, and the web layer talks to it through cheap cloneable
handles.** Everything the browser renders comes from a single render-ready
`Snapshot` published on a `tokio::sync::watch` channel.

## 1. Stack

```toml
airtouch5 = { version = "0.2", features = ["control"] }   # control enables control_zone/control_ac
axum = "0.8"
tokio = { version = "1", features = ["rt-multi-thread","macros","signal","time","sync"] }
tower = "0.5"
tower-http = { version = "0.6", features = ["fs","trace","set-header"] }   # ServeDir + tracing + cache header
askama = "0.12"            # compile-time Jinja-like templates
askama_axum = "0.4"        # IntoResponse for askama
tracing, tracing-subscriber (env-filter)
futures-util = "0.3"       # SSE stream combinators
clap = { version = "4", features = ["derive","env"] }
```

Vendored, un-minified, version-pinned static assets under `static/vendor/`:

- `htmx-2.0.4.js`
- `htmx-ext-sse-2.2.4.js`

They are served from `/vendor` with `Cache-Control: public, max-age=31536000,
immutable`. The versioned filenames make the immutable cache safe: a version
bump is itself a cache-bust, and `base.html` references the exact versioned
path in its `<script src=...>`. The htmx `sse` extension must load after core
htmx; fragment swapping over SSE needs `hx-ext="sse"`, `sse-connect`, and
`sse-swap`.

The site stylesheet `static/css/app.css` is served from `/css` (a plain
`ServeDir`, no immutable cache) and linked from `base.html`.

## 2. Module layout

The crate (`src/lib.rs`) exposes:

- `manager/` -- the connection actor.
  - `mod.rs` -- `ManagerHandle`, `spawn_manager()`, the supervisor + connected
    session loops, command application with a command timeout.
  - `command.rs` -- `Command` and the `ZoneControlReq` / `AcControlReq` request
    enums that translate to the crate's `ZoneControl` / `AcControl`.
  - `snapshot.rs` -- `Snapshot` and all view types, the crate-to-view mapping,
    `StaticInfo` (capabilities + names retained across reconnects), and the
    setpoint/airflow parsers.
- `airtouch/mod.rs` -- thin helpers: `discover_with_retry()`,
  `connect_and_prefill()`.
- `automation/mod.rs` -- the automation engine + shared config store. Two
  hard-coded programs (setpoint auto-off, idle auto-off) evaluated on a
  background tick; turns ACs off via `Command::ControlAc`. Config is shared
  with the web layer and optionally persisted to JSON.
- `web/` -- the axum layer.
  - `mod.rs` -- `build_router()`, the route table, the vendor `ServeDir` with
    its immutable cache layer, the trace + request-log middleware.
  - `state.rs` -- `AppState { manager, automation }`.
  - `error.rs` -- `AppError` -> 422 HTML fragment response.
  - `log.rs` -- request-log middleware (control actions at `info`, the rest
    at `debug`).
  - `sse.rs` -- `/events`: the SSE stream with per-id dirty diffing.
  - `handlers/` -- `pages.rs` (`GET /`, `GET /partials/*`, `POST /refresh`),
    `zone.rs` (`POST /zone/*` and bulk `/zones/*`), `ac.rs` (`POST /ac/*`),
    `automation.rs` (`POST /automation/*`).
- `mock.rs` -- an in-memory controller implementing the same `ManagerHandle`
  contract, used by `aircon-mock` and the e2e tests.
- `templates.rs` -- the askama `Template` structs (one per template file) and
  the `render_*` helpers the handlers and SSE stream call.
- `config.rs` -- `Config { listen, discovery_timeout, log_level }`.

Binaries:

- `src/main.rs` -- `aircon`: clap CLI, tracing init, `spawn_manager`, load the
  automation store, `spawn_automation`, `serve`.
- `src/bin/aircon-mock.rs` -- `aircon-mock`: clap CLI, tracing init,
  `spawn_mock_controller(sample_snapshot())`, load the automation store,
  `spawn_automation`, `serve`.

Templates (`templates/`, askama, configured via `askama.toml`):

- `base.html` -- `<head>`, a `<link>` to `/css/app.css`, htmx + sse script tags.
- `index.html` -- page shell + SSE bootstrap, includes the partials inline.
- `partials/connection_state.html` -- `#connection-state` (connected banner,
  included at the bottom of `#system`).
- `partials/system.html` -- `#system` (controller card + `[refresh]`, includes
  the connected banner and the console metadata fields).
- `partials/acs.html` -- `#acs` wrapper, includes one `ac.html` per AC.
- `partials/ac.html` -- `#ac-<id>` (one AC card).
- `partials/zones.html` -- `#zones` wrapper + the bulk "All zones" bar,
  includes one `zone.html` per zone.
- `partials/zone.html` -- `#zone-<id>` (one zone row).
- `partials/automation.html` -- `#automation` (two program cards: enable
  toggles + parameter presets).

There is no `macros.html`; shared rendering helpers live as methods on the view
types in `snapshot.rs`.

## 3. The connection actor

### 3.1 Why an actor

`AirTouch5` owns a spawned I/O task (`JoinHandle`) and a `oneshot::Sender`, so
it is **not `Clone`** and must not be shared across request handlers. One
long-lived task owns it; the web layer talks to it through a cheap handle:

```rust
#[derive(Clone)]
pub struct ManagerHandle {
    pub snapshot_rx: watch::Receiver<Snapshot>,   // read-only, cloneable; fan-out to many SSE clients
    pub cmd_tx: mpsc::Sender<Command>,            // request a control; reply on embedded oneshot
}
```

Stored in axum state as `AppState { manager: ManagerHandle }` (the handle is
already `Clone`; no `Arc` needed).

### 3.2 The supervisor loop

`spawn_manager()` creates the channels, spawns `manager_loop`, and returns the
handle. `manager_loop` runs forever:

1. **Discover** via `airtouch::discover_with_retry(timeout)` -- exponential
   backoff (500ms -> 30s cap) until a console is found.
2. **Connect and prefill** via `airtouch::connect_and_prefill(console)`:
   `AirTouch5::with_ipaddr`, then `try_join!` of `ac_capabilities()`,
   `zone_names()`, `console_version()` (independent queries, run concurrently),
   then `ac_status()` + `zone_status()` to prime the internal status watch.
   The response wrapper types in the crate are private, so the primitives are
   extracted by inference into our owned `StaticInfo` (`AcCap`, names, console
   identity).
3. **Run a connected session** (`run_connected`): publish an initial snapshot
   from the primed watch, then `select!` on:
   - `status_rx.changed()` -- rebuild + publish a new `Snapshot`;
   - `cmd_rx.recv()` -- apply the command (with a `COMMAND_TIMEOUT`), fold the
     post-change status into the snapshot, reply on the oneshot.
     Either the status watch closing (connection lost) or a command timing out
     returns `Err(())`, which triggers a reconnect: publish a disconnected
     snapshot (last-known state preserved, `connected = false`), back off, loop.

On disconnect the last-known `Snapshot` is preserved so the UI keeps showing
cards under a "disconnected" banner; SSE clients receive a `state` event.

### 3.3 Command timeout and poisoned connections

The console can silently hang on a single API call. Because commands are
applied serially in one task, a hung request would otherwise pile every later
click up behind it and deadlock the UI. `COMMAND_TIMEOUT` (10s) wraps every
console call:

- A normal API error (the console answered with an error) is replied to the
  handler and the connection is kept.
- A **timeout** replies to the handler with "console request timed out" and
  returns `Err(())`, so the manager drops the handle and reconnects.

This is why the request-log middleware buffers and logs every control action
with its elapsed time: the last line before a stall names exactly the action
that hung.

## 4. The Snapshot

`Snapshot` is **our own type**. The crate's `CurrentStatus` has private fields
and carries no name or capability data, so we map the crate's types into a
render-ready model. Every view struct derives `Clone, PartialEq` so the SSE
handler can diff old vs new per id (section 6).

```rust
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub connected: bool,
    pub console: ConsoleInfo,            // static, from discovery + console_version
    pub acs: BTreeMap<u8, AcView>,       // capabilities (static) + live status
    pub zones: BTreeMap<u8, ZoneView>,   // names (static) + live status + owning AC
}
```

`PartialEq` is implemented by hand to compare only the diffable fields. (No
`updated_at`/`Instant` metadata is stored; the watch channel itself is the
change signal.) Helpers on `Snapshot`:

- `bulk_temp_available()` -- whether any sensor zone exists (gates the bulk
  Temp button).
- `bulk_mode()` -- the control mode currently in effect across sensor zones:
  `Temperature` if all sensor zones are in temperature mode, else `Airflow`.
  Sensorless zones are ignored.
- `ac_has_open_zone(ac_id)` -- whether any zone of that AC is On/Turbo; used
  by the AC power handler to reject starting an AC with all zones off.

`StaticInfo` (capabilities + names + console identity) is rebuilt once per
connection and kept across the session; live status is merged into it each time
a snapshot is built. `StaticInfo::ac_for_zone()` derives zone -> AC ownership
from each `AcCapability`'s `zone_start_index .. + zone_count` range.

### 4.1 The view structs

```rust
struct ConsoleInfo {
    name: String,
    address: Option<IpAddr>, airtouch_id: Option<u32>, console_id: Option<String>,
    versions: Vec<String>, update_available: bool,
    ac_count: usize, zone_count: usize,
}

struct AcView {
    id: u8, name: String,
    zone_start_index: u8, zone_count: u8,
    supported_modes: Vec<&'static str>, supported_fan_speeds: Vec<&'static str>,
    setpoint_cool: (u8, u8), setpoint_heat: (u8, u8),
    status: Option<AcStatusView>,        // None until first status received
}

struct AcStatusView {
    power: Option<&'static str>,         // On/Off/AwayOff/AwayOn/Sleep
    mode: Option<&'static str>,          // Auto/Heat/Dry/Fan/Cool/AutoHeat/AutoCool
    fan_speed: Option<&'static str>,
    fan_intelligent_auto: bool,          // separate from fan_speed; own toggle
    setpoint: Option<Temperature>,
    temperature: Option<Temperature>,
    flags: Vec<&'static str>,
    error: Option<u16>,
    // Pre-formatted setpoint strings for the +/- stepper:
    setpoint_str: Option<String>, setpoint_down: Option<String>, setpoint_up: Option<String>,
}

struct ZoneView {
    id: u8, name: String, ac_id: Option<u8>,
    power: ZonePowerView,                // Off/On/Turbo (status variant)
    has_sensor: bool,
    control_mode: ControlModeView,       // Airflow | Temperature | Unknown
    airflow_pct: u8,                     // always available; both modes report a %
    setpoint: Option<Temperature>,       // Some only in Temperature mode
    sensor: Option<SensorView>,          // None=NoSensor, Some(NotAvailable|Temperature(t))
    flags: Vec<&'static str>,             // LowBattery/Spill
}

enum BulkModeView { Airflow, Temperature }   // the bulk bar's selected mode
```

### 4.2 The Temperature caveat

`airtouch5::types::Temperature` has **no public numeric accessor**. We keep the
`Temperature` through to the template and render via `Display` (e.g.
`format!("{}", t)` -> `24.3`). For the few numeric paths we need, we parse the
`Display` string back to `f32` (`temp_to_f32`). The natural control path uses
`Increment`/`Decrement`, which sidesteps the missing accessor entirely.

For the AC setpoint stepper, `build_ac_status_view` pre-computes
`setpoint_str` / `setpoint_down` / `setpoint_up` (stepping by 1.0 C, clamped to
10.0 - 25.0) so the template never does arithmetic. The mock controller calls
`recompute_setpoint_strings()` after mutating a setpoint.

### 4.3 Crate -> view mapping

| crate type (`types::status`)                         | view field                                     | notes                                                                                  |
| ---------------------------------------------------- | ---------------------------------------------- | -------------------------------------------------------------------------------------- |
| `AcStatus.power: Option<AcPower>`                    | `AcStatusView.power`                           | `On/Off/AwayOff/AwayOn/Sleep`                                                          |
| `AcStatus.mode: Option<AcMode>`                      | `.mode`                                        | `Auto/Heat/Dry/Fan/Cool/AutoHeat/AutoCool` (the three Auto variants all select Auto)   |
| `AcStatus.fan_speed: Option<(FanSpeed,bool)>`        | `.fan_speed` + `.fan_intelligent_auto`         | the bool is the IntelligentAuto modifier, surfaced as its own toggle                   |
| `AcStatus.setpoint/temperature: Option<Temperature>` | kept as `Temperature`                          | render via `Display`                                                                   |
| `AcFlags` (bitflags)                                 | `.flags: Vec<&str>`                            | `iter_names()`                                                                         |
| `ZoneStatus.power: ZonePower`                        | `ZoneView.power`                               | `Off/On/Turbo` (status enum)                                                           |
| `ZoneStatus.control: ZoneControl`                    | `.control_mode` + `.airflow_pct` + `.setpoint` | `Airflow(pct)` -> Airflow; `Temperature(pct,temp)` -> Temperature, setpoint=Some(temp) |
| `ZoneStatus.sensor_reading: ZoneSensorReading`       | `.has_sensor` + `.sensor`                      | `NoSensor`->false/None; `NotAvailable`->true/Some(NA); `Temperature(t)`->true/Some(t)  |
| `ZoneFlags` (bitflags)                               | `.flags`                                       | `LowBattery/Spill`                                                                     |

> **Two different enums share names.** `types::status::ZonePower`
> (`Off/On/Turbo`) is _what the zone is doing now_;
> `types::control::ZonePower` (`Toggle/Off/On/Turbo`) is a _command_. They are
> distinct types despite the shared name -- same for `AcPower`/`AcMode`/
> `FanSpeed`. The mapping functions must use the correct module for each
> direction. `ZoneControlReq`/`AcControlReq` in `command.rs` are the bridge:
> they hold the control-enum values and `to_zone_control()` / `to_ac_control()`
> build the crate's `ZoneControl` / `AcControl` with only the relevant fields
> set.

### 4.4 Capability extraction

`connect_and_prefill` flattens each `AcCapability` into our owned `AcCap`:
`supported_modes` and `supported_fan_speeds` come from `iter_names()` on the
crate's bitflags. `IntelligentAuto` is filtered out of `supported_fan_speeds`
(it is a modifier, not a selectable base speed) and rendered as its own "Int
Auto" toggle. The template uses `mode_supported(...)` / `fan_supported(...)`
to hide buttons the unit does not support.

## 5. Commands (web -> manager)

```rust
enum Command {
    Refresh { reply },                                    // re-pull full status (the [refresh] button)
    ControlZone { id: u8, req: ZoneControlReq, reply },
    ControlAc   { id: u8, req: AcControlReq,   reply },
}

enum ZoneControlReq {
    Power(types::control::ZonePower),                     // On/Off/Turbo/Toggle
    SetControlType(types::control::ZoneControlType),      // Airflow | Temperature | Toggle
    StepValue(types::control::ZoneControlValue),          // Increment | Decrement
    SetAirflow(u8),                                        // -> Airflow(pct)
    SetTemperature(Temperature),                           // also forces Temperature mode
}

enum AcControlReq {
    Power(AcPower), Mode(AcMode), FanSpeed(FanSpeed), Setpoint(Temperature),
}
```

The manager translates each into the crate's `ZoneControl` / `AcControl` (the
other fields `None`) and calls `control_zone` / `control_ac`. The call returns
the post-change status message, which the manager folds into the `Snapshot`
immediately (by `apply`-ing a `StatusChange` onto the borrowed `CurrentStatus`)
and re-publishes, so the handler renders the new state without waiting for the
async watch update. The async update arrives shortly after and reconciles.

Protocol constraints enforced at the edges:

- Setpoint temperatures must be 10.0 - 25.0 C (`parse_setpoint` rejects outside).
- Airflow percentages must be 0 - 100 (`parse_airflow` rejects outside).
- `ZoneControl.control` must be `None` for sensor-less zones. The per-zone Temp
  button is disabled for them, and the bulk "All zones" temperature switch
  skips them.
- The per-zone `/control-type` and `/step` routes compute an absolute value
  server-side and send `SetAirflow` / `SetTemperature` rather than
  `SetControlType` / `StepValue`. The console silently ignores a
  control-type-only message (200 OK but no mode change -> no UI feedback),
  and a relative Increment/Decrement powers an OFF zone on. Absolute values
  carry no power field, so an OFF zone stays off while its value/mode still
  updates -- the same property the bulk presets rely on.

## 6. HTTP routes and the htmx/SSE contract

All fragment responses are `text/html` (a partial). Live updates are pushed
over a single SSE stream.

### 6.1 Pages and partials

| Method | Path                   | Handler                     | Returns                             |
| ------ | ---------------------- | --------------------------- | ----------------------------------- |
| GET    | `/`                    | `pages::index`              | `index.html` shell                  |
| GET    | `/partials/system`     | `pages::partial_system`     | `#system`                           |
| GET    | `/partials/acs`        | `pages::partial_acs`        | `#acs` (all AC cards)               |
| GET    | `/partials/acs/{id}`   | `pages::partial_ac`         | `#ac-<id>`                          |
| GET    | `/partials/zones`      | `pages::partial_zones`      | `#zones` (bulk bar + all rows)      |
| GET    | `/partials/zones/{id}` | `pages::partial_zone`       | `#zone-<id>`                        |
| GET    | `/partials/automation` | `pages::partial_automation` | `#automation` (both program cards)  |
| POST   | `/refresh`             | `pages::refresh`            | re-pull status, re-render `#system` |

### 6.2 SSE

| Method | Path      | Returns             |
| ------ | --------- | ------------------- |
| GET    | `/events` | `text/event-stream` |

On connect, the stream emits a **full** initial render (the `state`, `system`,
every `ac-<id>`, every `zone-<id>` fragment) so a fresh browser populates
everything, then **per-change diffs** thereafter. Each event's `data:` is the
matching HTML fragment with a stable element `id`:

| event       | `data:`                          | browser target           |
| ----------- | -------------------------------- | ------------------------ |
| `state`     | `<div id="connection-state">...` | swap `#connection-state` |
| `system`    | `<div id="system">...`           | swap `#system`           |
| `ac-<id>`   | `<div id="ac-<id>" ...>...`      | swap `#ac-<id>`          |
| `zone-<id>` | `<div id="zone-<id>" ...>...`    | swap `#zone-<id>`        |

**Per-id event names are deliberate.** The htmx-sse extension swaps an event's
data into _every_ element listening for that event name, so a generic `zone`
event would swap the same fragment into every card. Per-id names (`zone-3`,
`zone-7`) isolate each card to its own event. Each fragment element carries
its own `sse-swap="zone-<id>"` (or `ac-<id>`, `system`, `state`) plus
`hx-swap="outerHTML"`.

**Per-id dirty diffing.** The SSE handler keeps the previous `Snapshot` (clone).
On each `watch::changed()` it compares `prev.console` / `prev.connected` and
the `BTreeMap`s key-by-key (view types are `PartialEq`), and emits only the
changed ids. Newly-appearing ids emit their full fragment; ids that vanish are
not re-emitted (a count change is rare and is covered by the full re-render on
reconnect). A `pending` `VecDeque` batches multiple diffs from one wake so the
unfold loop drains them before awaiting again. If the watch sender drops
(manager gone), the stream ends.

Client wiring (in `index.html`):

```html
<div hx-ext="sse" sse-connect="/events">
  ... partials, each with its own sse-swap="..." hx-swap="outerHTML" ...
</div>
```

### 6.3 Zone control endpoints

| Method | Path                      | Form field(s)                  | Action                                                                                                                                                      |
| ------ | ------------------------- | ------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------- |
| POST   | `/zone/{id}/power`        | `power=on\|off\|turbo\|toggle` | `ZonePower`                                                                                                                                                 |
| POST   | `/zone/{id}/control-type` | `type=airflow\|temperature`    | `SetAirflow(pct)` / `SetTemperature(t)` (temp rejected if `!has_sensor`; absolute value so an off zone is not powered on)                                   |
| POST   | `/zone/{id}/step`         | `dir=up\|down`                 | `SetAirflow(pct +/-5%)` / `SetTemperature(t +/-1.0 C)` (server-computed absolute value; the relative Increment/Decrement opcode would power an off zone on) |
| POST   | `/zone/{id}/airflow`      | `pct=0..100`                   | `SetAirflow(pct)`                                                                                                                                           |
| POST   | `/zone/{id}/setpoint`     | `temp=10.0..25.0`              | `SetTemperature(t)` (also forces Temperature mode)                                                                                                          |

Bulk endpoints apply to every zone and re-render the whole `#zones` partial:

| Method | Path            | Form field(s)                             | Action                                                                    |
| ------ | --------------- | ----------------------------------------- | ------------------------------------------------------------------------- |
| POST   | `/zones/power`  | `power=on\|off`                           | turn every zone on or off in one shot (skips zones already in that state) |
| POST   | `/zones/preset` | `mode=airflow\|temperature` + `value=...` | set every zone to a preset (% to all, temp to sensor zones only)          |

Each single-zone POST sends the command, awaits the reply, and returns the
updated `zone.html` fragment for that id; the browser swaps it into
`#zone-<id>`. The bulk endpoints return the whole `zones.html` partial into
`#zones`, and pass an explicit `BulkModeView` to the renderer so the bulk bar
reflects the user's last selection rather than only the live zone states.

### 6.4 AC control endpoints

| Method | Path                | Form field(s)                                                          | Action                                            |
| ------ | ------------------- | ---------------------------------------------------------------------- | ------------------------------------------------- |
| POST   | `/ac/{id}/power`    | `power=on\|off\|away\|sleep\|toggle`                                   | `AcPower`                                         |
| POST   | `/ac/{id}/mode`     | `mode=auto\|heat\|dry\|fan\|cool`                                      | `AcMode`                                          |
| POST   | `/ac/{id}/fan`      | `fan=auto\|quiet\|low\|medium\|high\|powerful\|turbo\|intelligentauto` | `FanSpeed`                                        |
| POST   | `/ac/{id}/setpoint` | `temp=<float>`                                                         | `Setpoint` (validated against the protocol range) |

**AC-on guard.** Starting an AC (explicit `on`, or a `toggle` that resolves to
on) is rejected with a 422 while every one of its zones is off: the console
would run the unit with no open airflow path. The handler checks
`Snapshot::ac_has_open_zone(id)` before sending the command. Turning an AC
off, or using Away/Sleep, is always allowed.

### 6.5 Errors

`AppError(String)` renders as HTTP 422 with a tiny `<div class="err-line">`
HTML fragment. htmx only swaps on 2xx, so a 422 surfaces via htmx's
`htmx:responseError` event; the message is also human-readable for curl.
Handlers return `AppError::msg(...)` for invalid form values, unknown ids, and
manager/console failures.

### 6.6 Automation configuration endpoints

| Method | Path                              | Form field(s)          | Action                                        |
| ------ | --------------------------------- | ---------------------- | --------------------------------------------- |
| POST   | `/automation/setpoint-off/toggle` | `enabled=true\|false`  | Enable/disable the setpoint auto-off program. |
| POST   | `/automation/setpoint-off/hold`   | `mins=15\|30\|60`      | Set the setpoint condition hold time.         |
| POST   | `/automation/idle-off/toggle`     | `enabled=true\|false`  | Enable/disable the idle auto-off program.     |
| POST   | `/automation/idle-off/timeout`    | `mins=15\|30\|60\|120` | Set the idle timeout.                         |

Each handler mutates the shared `AutomationStore` (persisting to disk when a
path is configured), then returns the re-rendered `#automation` partial so the
UI reflects the new enable/parameter state immediately. Unknown preset values
are rejected with a 422. The automation partial is **not** part of the SSE
diff stream -- it only changes through these POSTs.

## 7. The mock controller

`mock.rs` implements the exact same `ManagerHandle` contract (a `watch::Sender`

- `mpsc::Receiver<Command>`) without any `AirTouch5` handle or wire protocol.
  It owns a `Snapshot`, applies commands by mutating it (mirroring the console's
  semantics: 5% / 1.0 C steps, clamped setpoints, sensorless rejection,
  IntelligentAuto flag, the AC-on guard lives in the handler), and re-publishes.
  Because the router/handlers/templates/SSE code is unchanged, the mock drives
  the real UI path end to end.

`spawn_mock_controller(initial)` returns `(ManagerHandle, MockController)`. The
`MockController` lets tests inject arbitrary live changes (as if someone
adjusted a zone at the wall console) via `mutate(FnOnce(&mut Snapshot))`, which
exercises the SSE dirty-diff path. `sample_snapshot()` builds the one-AC /
six-zone fixture used by `aircon-mock` and the test suite.

## 8. Logging and request middleware

`web/log.rs` is a `from_fn` middleware applied as the outermost layer so its
elapsed time covers the whole request. It distinguishes control actions (any
`POST` to `/refresh`, `/zone/...`, `/zones/...`, `/ac/...`) from everything
else:

- **Control actions** are buffered (up to 1 MiB) and logged at `info` with the
  client IP, `METHOD path action=<form body>`, status, and elapsed time.
  Buffering the body lets the handler still receive a re-readable request.
- **All other requests** (pages, partials, SSE, vendor assets) are logged at
  `debug` with IP, method, path, status, elapsed.

The client IP comes from axum's `ConnectInfo<SocketAddr>` extension, populated
only because `serve` uses `into_make_service_with_connect_info`. The e2e test
harness mirrors this so the middleware has a real IP. This logging exists
specifically to diagnose hung console requests (section 3.3): the last
interaction line before a stall names the action that hung.

## 9. Shutdown

`serve()` uses an **immediate** shutdown, not axum's graceful shutdown. The
serve future races against a shutdown signal (Ctrl-C, SIGTERM, or an optional
`--timeout` deadline); whichever fires first wins, and the serve future is
dropped, closing the listener and any in-flight connections. Graceful shutdown
would instead wait for in-flight requests to finish, and with SSE streams held
open for the life of the page that effectively never happens -- a plain Ctrl-C
would hang.

## 10. Testing

`tests/e2e.rs` spawns the mock controller behind the real axum router on
`127.0.0.1:0` and drives it with a real `reqwest` client (including a streaming
SSE connection). Each test is wrapped in a `tokio::time::timeout` hard cap so it
can never hang. The suite covers: the index render, partials, zone stepping
and clamping, sensor requirements, the "controls stay usable while off" rules,
hidden unsupported fan speeds, Auto selection across Auto/AutoHeat/AutoCool,
temperature-mode setpoints, the bulk bar (presets, control-type switches that
skip sensorless zones, invalid-value rejection, the no-sensors Temp disable),
AC setpoint/power handling including the AC-on-with-zones-off guard, 422s on
unknown ids, the immutable vendor-asset cache, the `/refresh` re-pull, the
SSE live-change path driven through `MockController::mutate`, and the
automation UI (section render, enable toggles, parameter presets, preset
rejection). The `automation` module also has unit tests for the pure
conditions (`setpoint_condition`, the control fingerprint) and short-duration
engine tests against the mock controller that verify both programs actually
turn the AC off (and that a control change resets the idle timer).

## 11. Adding a new control or view

1. If the crate exposes a new field, extend the relevant view struct in
   `snapshot.rs` (keep it `Clone, PartialEq` so SSE diffing keeps working) and
   add the mapping in `build_*_view`.
2. Add a `*Req` variant in `command.rs` and its `to_*_control()` translation.
3. Add the route in `web/mod.rs` and a handler in `web/handlers/`. The handler
   parses the form, sends the `Command`, awaits the reply, and returns the
   matching rendered fragment (`templates::render_*`).
4. Add the control to the template, targeting the right `id` with
   `hx-swap="outerHTML"`.
5. Add an e2e test against the mock controller.

Keep in mind the two-enum split (status vs control) and the `Temperature`
numeric-accessor caveat: prefer `Increment`/`Decrement`, and for direct
setpoints parse the `Display` string via `temp_to_f32`.

## 12. Automation engine

`automation/mod.rs` runs two hard-coded programs as a background task. The
engine is spawned by the binaries (`spawn_automation`) with a configurable
tick interval (`--automation-tick-secs`, default 60s; `0` disables it). It
holds a `ManagerHandle`, watches the live snapshot, and fires by sending
`Command::ControlAc { Power(Off) }` to every On AC. Zones are never touched.

### 12.1 Shared config store

`AutomationStore` (cheaply `Clone` -- an `Arc<RwLock<AutomationConfig>>` plus an
optional `PathBuf`) is the single source of truth for the enable flags and
parameters. The engine reads it each tick; the web handlers mutate it via
typed setters (`set_setpoint_off_enabled`, `set_setpoint_off_hold`, ...) that
persist to the configured JSON file atomically (write-to-tmp + rename), and
the parent directory is created on first write. On
startup `AutomationStore::load(path)` reads the file back; the path is the
`--automation-config` flag value, or the XDG default
(`$XDG_CONFIG_HOME/aircon/automation.json`) when the flag is unset. tests use
`AutomationStore::new(config)` for an in-memory, non-persisting store. Defaults:
both programs disabled, 15-minute setpoint hold, 30-minute idle timeout.

### 12.2 Setpoint auto-off

Fires when, for every on-zone: it is in temperature control mode, has an
available sensor reading, and its reading has "reached" its setpoint. Reach is
decided by the owning AC's mode (`zone_satisfied`): cooling modes (Cool /
AutoCool / Dry) are satisfied at `reading <= setpoint + tol`, heating modes
(Heat / AutoHeat) at `reading >= setpoint - tol`; Auto / Fan / unknown fall
back to a symmetric `|reading - setpoint| <= tol` (`tol = 0.5 C`). A sensor
zone with no reading, or any on-zone in airflow mode, disqualifies the program
(safe: do not turn off). The condition must **remain** true for the hold time
(`setpoint_since: Option<Instant>`); a flicker resets it. The fire is also
guarded by "some AC is On" so a satisfied-but-already-off system does not
re-fire.

### 12.3 Idle auto-off

Fires after `idle_off_timeout` with no **control** changes. "Control changes"
are tracked by a `control_fingerprint` string over every zone power /
control-mode / airflow / setpoint and every AC power / mode / fan / fan-int-auto
/ setpoint -- deliberately **excluding** the live sensor readings and AC "now"
temperatures, which drift continuously and would otherwise keep the idle timer
alive forever. `last_change` is reset whenever the published snapshot's
fingerprint changes (catching both web and wall-console interactions), and
also when a program fires. The fire is guarded by "some AC is On".

### 12.4 Why ACs only

Both programs turn the **AC units** off rather than the zones. An AC running
with no open airflow path is undesirable, and AC power-off is the single
action that actually stops the system; zones left in their on/off state are
harmless once the AC is off. Away/Sleep AC states are never touched -- only
ACs whose power is `On` get a `Power(Off)` command, so a user's deliberate
Away/Sleep setting survives an idle or setpoint trigger.
