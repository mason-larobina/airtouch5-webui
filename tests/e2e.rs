//! End-to-end integration tests.
//!
//! Each test spawns a mock controller + the real axum router on a task bound to
//! `127.0.0.1:0`, then drives it with a real `reqwest` HTTP client (including a
//! streaming SSE connection). Every test is wrapped in a `tokio::time::timeout`
//! hard cap so it can never hang.

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::StreamExt;

use aircon::automation::AutomationConfig;
use aircon::manager::snapshot::{ControlModeView, ZonePowerView};
use aircon::mock::{self, MockController};
use aircon::web;

/// Per-test hard timeout. Tests against the mock controller are instant; this is
/// a safety net against accidental hangs (e.g. an SSE read that never completes).
const TEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Spawn the mock controller behind the real router on an ephemeral port.
async fn spawn_server() -> (SocketAddr, MockController) {
    let (manager, mock_ctrl) = mock::spawn_mock_controller(mock::sample_snapshot());
    let automation = aircon::automation::AutomationStore::new(AutomationConfig::default());
    let app = web::build_router(manager, automation);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        // No graceful shutdown: cancelled when the test runtime drops. Use
        // the connect-info make service so the request-log middleware has a
        // real client IP to log, exactly as the production binary does.
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });
    (addr, mock_ctrl)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

/// Run a future with the per-test hard timeout and assert it completes.
async fn capped<F, T>(f: F) -> T
where
    F: std::future::Future<Output = T>,
{
    tokio::time::timeout(TEST_TIMEOUT, f)
        .await
        .expect("test exceeded the hard timeout")
}

#[tokio::test]
async fn index_renders_console_and_zones() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        let body = client()
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(body.contains("LivingRoom-AT5"), "console name missing");
        assert!(body.contains("Whole House"), "AC name missing");
        assert!(body.contains("Living Room"), "zone 0 missing");
        assert!(body.contains("Bathroom"), "zone 7 missing");
        assert!(
            body.contains("sse-connect=\"/events\""),
            "SSE bootstrap missing"
        );
    })
    .await;
}

#[tokio::test]
async fn system_partial_shows_console_metadata() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        let body = client()
            .get(format!("http://{addr}/partials/system"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(body.contains("LivingRoom-AT5"));
        assert!(body.contains("192.168.1.42"));
        assert!(body.contains("#13"));
        assert!(body.contains("available"), "update-available flag missing");
    })
    .await;
}

#[tokio::test]
async fn zone_step_increments_airflow() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        // Kitchen (zone 2) starts at 80% in airflow mode; +5 -> 85%.
        let body = client()
            .post(format!("http://{addr}/zone/2/step"))
            .form(&[("dir", "up")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(body.contains("85%"), "expected 85% after step, got: {body}");
        assert!(body.contains("Kitchen"));
    })
    .await;
}

#[tokio::test]
async fn zone_step_decrements_airflow_clamped() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        // Zone 2 is at 80%; step down three times -> 75, 70, 65.
        let c = client();
        for expected in ["75%", "70%", "65%"] {
            let body = c
                .post(format!("http://{addr}/zone/2/step"))
                .form(&[("dir", "down")])
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            assert!(body.contains(expected), "expected {expected}, got: {body}");
        }
    })
    .await;
}

#[tokio::test]
async fn zone_step_while_off_keeps_zone_off() {
    capped(async {
        let (addr, mock) = spawn_server().await;
        // Kitchen (zone 2) starts ON at 80% in airflow mode. Turn it OFF at the
        // wall console, then step it up. The +/- must update the value (80 ->
        // 85) WITHOUT powering the zone back on. The real console powers an off
        // zone on for a relative Increment/Decrement, so the handler sends an
        // absolute SetAirflow (no power field) instead -- the mock mirrors that
        // console behaviour for StepValue, so this test fails if the handler
        // ever reverts to sending StepValue.
        mock.mutate(|s| {
            if let Some(z) = s.zones.get_mut(&2) {
                z.power = ZonePowerView::Off;
            }
        })
        .await;

        let _off_body = loop {
            let b = client()
                .get(format!("http://{addr}/partials/zones/2"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            if b.contains("zone-row off") {
                break b;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };

        let body = client()
            .post(format!("http://{addr}/zone/2/step"))
            .form(&[("dir", "up")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            body.contains("zone-row off"),
            "stepping an off zone must keep it off, got: {body}"
        );
        assert!(
            body.contains("85%"),
            "stepping an off zone must still update its value (80 -> 85), got: {body}"
        );
    })
    .await;
}

#[tokio::test]
async fn zone_control_type_to_temperature_switches_mode() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        // Kitchen (zone 2) has a sensor and starts in airflow (%) mode. Posting
        // the per-zone control-type toggle to switch to temperature must take
        // effect: the C button becomes active and the stepper shows a setpoint.
        // A control-type-only message is silently ignored by the real console
        // (200 OK but no mode change, so no UI feedback); the handler sends an
        // absolute SetTemperature instead so the console honours the switch.
        let body = client()
            .post(format!("http://{addr}/zone/2/control-type"))
            .form(&[("type", "temperature")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        // The % / C mode-toggle column is gone: the setpoint value is now a
        // single tappable button that doubles as the mode switch. After the
        // switch it must show a temperature setpoint (kitchen has no prior
        // setpoint, so the handler falls back to 20.0 C), not a %.
        let val_btn = body
            .split_once("class=\"val\"")
            .expect("val button missing")
            .1
            .split_once("</button>")
            .expect("val button closing tag missing")
            .0;
        // The button's inner text is everything after the opening tag's `>`;
        // the attributes (e.g. the title) may contain a stray %.
        let inner = val_btn
            .split_once('>')
            .expect("val button opening tag missing")
            .1
            .trim();
        assert!(
            inner.contains(" C"),
            "val must show a temperature setpoint after the switch, got: {body}"
        );
        assert!(
            !inner.contains('%'),
            "val must not show a % airflow value after switching to temperature, got: {body}"
        );

        // Tapping the value must toggle back to % control via the toggle
        // endpoint (it is only wired up for zones with a sensor).
        assert!(
            body.contains("/zone/2/control-type/toggle"),
            "val button must POST the toggle endpoint, got: {body}"
        );
    })
    .await;
}

#[tokio::test]
async fn zone_setpoint_requires_sensor() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        // Bedroom (zone 1) has no sensor; switching to temperature control is
        // rejected by the mock controller (mirrors the protocol constraint).
        let resp = client()
            .post(format!("http://{addr}/zone/1/control-type"))
            .form(&[("type", "temperature")])
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::UNPROCESSABLE_ENTITY,
            "expected 422 for sensorless temperature control"
        );
    })
    .await;
}

#[tokio::test]
async fn zone_control_type_toggle_round_trips() {
    capped(async {
        let addr = spawn_server().await.0;
        // Kitchen (zone 2) has a sensor and starts in airflow (%) mode. The
        // toggle endpoint switches to the opposite mode; toggling again
        // returns it to airflow. Each response re-renders the zone row, so
        // the val button reflects the new mode (C vs %).
        let to_temp = client()
            .post(format!("http://{addr}/zone/2/control-type/toggle"))
            .send()
            .await
            .unwrap();
        assert_eq!(to_temp.status(), reqwest::StatusCode::OK);
        let body = to_temp.text().await.unwrap();
        let inner = body
            .split_once("class=\"val\"")
            .expect("val button missing")
            .1
            .split_once("</button>")
            .expect("val button closing tag missing")
            .0
            .split_once('>')
            .expect("val button opening tag missing")
            .1
            .trim();
        assert!(
            inner.contains(" C"),
            "first toggle (airflow -> temp) must show a setpoint, got: {body}"
        );

        let to_air = client()
            .post(format!("http://{addr}/zone/2/control-type/toggle"))
            .send()
            .await
            .unwrap();
        assert_eq!(to_air.status(), reqwest::StatusCode::OK);
        let body = to_air.text().await.unwrap();
        let inner = body
            .split_once("class=\"val\"")
            .expect("val button missing")
            .1
            .split_once("</button>")
            .expect("val button closing tag missing")
            .0
            .split_once('>')
            .expect("val button opening tag missing")
            .1
            .trim();
        assert!(
            inner.contains('%'),
            "second toggle (temp -> airflow) must show a %, got: {body}"
        );
    })
    .await;
}

#[tokio::test]
async fn zone_airflow_toggle_enabled_without_sensor() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        // Bedroom (zone 1) is off and has no sensor. Switching to airflow (%)
        // control must stay possible even when off / sensorless -- the %
        // button is the one that must NOT be disabled. The Temp button stays
        // disabled (temperature control needs a sensor).
        let body = client()
            .get(format!("http://{addr}/partials/zones/1"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        // With the % / C mode-toggle gone, the setpoint value button is the
        // sole mode switch. For a sensorless zone it is locked in airflow
        // (%) mode: the button must be `disabled` (no toggle available) and
        // must not carry a toggle hx-post.
        let val_btn = body
            .split_once("class=\"val\"")
            .expect("val button missing")
            .1
            .split_once("</button>")
            .expect("val button closing tag missing")
            .0;
        assert!(
            val_btn.contains("disabled"),
            "val button must be disabled without a sensor, got: {body}"
        );
        assert!(
            !val_btn.contains("/control-type/toggle"),
            "sensorless val button must not POST the toggle endpoint, got: {body}"
        );
    })
    .await;
}

#[tokio::test]
async fn zone_off_with_sensor_keeps_controls_enabled() {
    capped(async {
        let (addr, mock) = spawn_server().await;
        // Living Room (zone 0) has a sensor and is in temperature mode. Turn
        // the zone OFF at the wall console: the tmp/% selection and setpoint
        // stepper must stay enabled and functional while the zone is off.
        mock.mutate(|s| {
            if let Some(z) = s.zones.get_mut(&0) {
                z.power = ZonePowerView::Off;
            }
        })
        .await;

        // The mutation is applied on the mock task, so poll the partial until
        // the row is marked off (bounded by the per-test hard timeout).
        let body = loop {
            let b = client()
                .get(format!("http://{addr}/partials/zones/0"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            if b.contains("zone-row off") {
                break b;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };

        // A zone that is off but has a sensor must render NO disabled controls:
        // the % button, the Temp button, and the +/- stepper all stay enabled.
        assert!(
            !body.contains("disabled"),
            "off zone with a sensor must keep tmp/% and setpoint controls enabled, got: {body}"
        );

        // Setting the setpoint while the zone is off must succeed.
        let body = client()
            .post(format!("http://{addr}/zone/0/setpoint"))
            .form(&[("temp", "21.5")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            body.contains("21.5"),
            "setpoint must be settable while zone off, got: {body}"
        );

        // Stepping the setpoint while the zone is off must succeed (21.5 -> 22.5).
        let body = client()
            .post(format!("http://{addr}/zone/0/step"))
            .form(&[("dir", "up")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            body.contains("22.5"),
            "setpoint stepper must work while zone off, got: {body}"
        );
    })
    .await;
}

#[tokio::test]
async fn system_off_keeps_zone_controls_working() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        // Turn the AC (system) OFF.
        let resp = client()
            .post(format!("http://{addr}/ac/0/power"))
            .form(&[("power", "off")])
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);

        // The AC's OFF button is now the selected one (system is off).
        let ac_body = client()
            .get(format!("http://{addr}/partials/acs/0"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            ac_body.contains("class=\"btn selected\"\n              hx-post=\"/ac/0/power\" hx-vals='{\"power\":\"off\"}'"),
            "expected the AC OFF button selected, got: {ac_body}"
        );

        // While the system is off, the zone controls must stay enabled.
        let body = client()
            .get(format!("http://{addr}/partials/zones/0"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            !body.contains("disabled"),
            "zone controls must stay enabled when the system (AC) is off, got: {body}"
        );

        // And setting a zone setpoint while the system is off must succeed.
        let body = client()
            .post(format!("http://{addr}/zone/0/setpoint"))
            .form(&[("temp", "20.5")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            body.contains("20.5"),
            "zone setpoint must be settable while the system is off, got: {body}"
        );
    })
    .await;
}

#[tokio::test]
async fn ac_hides_unsupported_fan_speeds() {
    capped(async {
        let (addr, mock) = spawn_server().await;
        // AC 0 advertises every fan speed in the sample. Restrict it to a
        // subset (Auto, Low, High) to mirror a system that does not support
        // Quiet / Med / Power / Turbo: those must be hidden, not disabled.
        mock.mutate(|s| {
            if let Some(ac) = s.acs.get_mut(&0) {
                ac.supported_fan_speeds = vec!["Auto", "Low", "High"];
            }
        })
        .await;

        let body = client()
            .get(format!("http://{addr}/partials/acs/0"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        // Supported fan speeds must still be rendered as buttons.
        for supported in ["auto", "low", "high"] {
            assert!(
                body.contains(&format!("{{\"fan\":\"{supported}\"}}")),
                "supported fan speed {supported:?} must be rendered, got: {body}"
            );
        }

        // Unsupported fan speeds must be entirely absent (not just disabled).
        for unsupported in ["quiet", "medium", "powerful", "turbo"] {
            assert!(
                !body.contains(&format!("{{\"fan\":\"{unsupported}\"}}")),
                "unsupported fan speed {unsupported:?} must be hidden, got: {body}"
            );
        }
        // No disabled fan buttons remain (the only `disabled` left, if any,
        // would be a setpoint stepper when out of range -- none here).
        let fan_section = body
            .split_once("aria-label=\"fan speed\"")
            .expect("fan speed section missing")
            .1
            .split_once("Int Auto")
            .expect("Int Auto marker missing")
            .0;
        assert!(
            !fan_section.contains("disabled"),
            "fan speed segmented control must contain no disabled buttons, got: {fan_section}"
        );
    })
    .await;
}

#[tokio::test]
async fn ac_auto_button_selected_for_autoheat_autocool() {
    capped(async {
        let (addr, mock) = spawn_server().await;
        // The console reports Auto mode as one of "Auto", "AutoHeat", or
        // "AutoCool" (the heat/cool split is the console's own decision; the
        // controllable mode is just Auto). The single Auto button must read as
        // selected for all three, and Heat/Cool must NOT be selected.
        for mode in ["AutoHeat", "AutoCool", "Auto"] {
            mock.mutate(|s| {
                if let Some(ac) = s.acs.get_mut(&0) {
                    if let Some(st) = ac.status.as_mut() {
                        st.mode = Some(mode);
                    }
                }
            })
            .await;

            let body = client()
                .get(format!("http://{addr}/partials/acs/0"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();

            // The Auto mode button is the one whose tag carries
            // {"mode":"auto"}; it must be marked active.
            let auto_btn = body
                .split_once("{\"mode\":\"auto\"}")
                .expect("auto mode button missing")
                .1
                .split_once(">Auto</button>")
                .expect("auto button closing tag missing")
                .0;
            assert!(
                auto_btn.contains("class=\"active\""),
                "Auto button must be selected when console reports {mode:?}, got: {auto_btn}"
            );

            // The Heat and Cool buttons must NOT be selected for any Auto
            // variant.
            for other in ["heat", "cool"] {
                let btn = body
                    .split_once(&format!("{{\"mode\":\"{other}\"}}"))
                    .expect("{other} mode button missing")
                    .1
                    .split_once(&format!(
                        ">{}</button>",
                        if other == "heat" { "Heat" } else { "Cool" }
                    ))
                    .expect("{other} button closing tag missing")
                    .0;
                assert!(
                    !btn.contains("class=\"active\""),
                    "{other} button must not be selected when console reports {mode:?}, got: {btn}"
                );
            }
        }
    })
    .await;
}

#[tokio::test]
async fn zone_temperature_mode_stores_setpoint() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        // Living Room (zone 0) has a sensor; set a setpoint.
        let body = client()
            .post(format!("http://{addr}/zone/0/setpoint"))
            .form(&[("temp", "21.5")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(body.contains("21.5"), "expected setpoint 21.5, got: {body}");
        assert!(body.contains("Living Room"));
    })
    .await;
}

#[tokio::test]
async fn zones_partial_renders_bulk_bar_with_airflow_presets() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        // The sample snapshot has a mix of airflow and temperature zones, so
        // the derived bulk mode is airflow (%). The % / Temp toggle is now
        // purely visual, so BOTH preset rows are always rendered (when temp
        // is available) and CSS picks the visible one via `data-bulk-mode`.
        let body = client()
            .get(format!("http://{addr}/partials/zones"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            body.contains("hx-post=\"/zones/power\" hx-vals='{\"power\":\"on\"}'"),
            "bulk ON button missing, got: {body}"
        );
        assert!(
            body.contains("hx-post=\"/zones/power\" hx-vals='{\"power\":\"off\"}'"),
            "bulk OFF button missing, got: {body}"
        );

        // The % / Temp toggle must NOT issue a request -- it only carries a
        // `data-mode` attribute the client-side script keys off.
        assert!(
            !body.contains("/zones/control-type"),
            "bulk mode toggle must not issue a request, got: {body}"
        );
        assert!(
            body.contains("data-mode=\"airflow\""),
            "bulk % toggle needs data-mode=airflow, got: {body}"
        );
        assert!(
            body.contains("data-mode=\"temperature\""),
            "bulk Temp toggle needs data-mode=temperature, got: {body}"
        );

        // The bar starts in airflow mode.
        assert!(
            body.contains("data-bulk-mode=\"airflow\""),
            "bulk bar must start in airflow mode, got: {body}"
        );

        // Both preset rows are rendered; the airflow one is shown and the
        // temperature one is hidden by CSS.
        assert!(
            body.contains("preset-row preset-airflow"),
            "airflow preset row missing, got: {body}"
        );
        assert!(
            body.contains("preset-row preset-temperature"),
            "temperature preset row missing, got: {body}"
        );
        for (val, label) in [("25", "25%"), ("50", "50%"), ("75", "75%"), ("100", "100%")] {
            assert!(
                body.contains(&format!("{{\"mode\":\"airflow\",\"value\":\"{val}\"}}")),
                "airflow preset {label:?} (value {val:?}) missing, got: {body}"
            );
            assert!(
                body.contains(&format!(">{label}</button>")),
                "airflow preset label {label:?} missing, got: {body}"
            );
        }
        for t in ["20", "21", "22", "23"] {
            assert!(
                body.contains(&format!("{{\"mode\":\"temperature\",\"value\":\"{t}\"}}")),
                "temperature preset {t:?} missing, got: {body}"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn bulk_mode_toggle_is_visual_only() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        // The % / Temp toggle must not post: it only swaps which preset row is
        // visible. Clicking a preset is what actually sends the command (see
        // the bulk_preset_* tests); here we just assert the toggle carries no
        // hx-post and that a preset click keeps the requested mode active on
        // the re-rendered bar.
        let body = client()
            .get(format!("http://{addr}/partials/zones"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        // Neither toggle button issues a request.
        let airflow_btn = body
            .split_once("data-mode=\"airflow\"")
            .expect("bulk airflow toggle missing")
            .1
            .split_once("</button>")
            .expect("bulk airflow toggle close missing")
            .0;
        assert!(
            !airflow_btn.contains("hx-post"),
            "bulk % toggle must be visual only, got: {airflow_btn}"
        );
        let temp_btn = body
            .split_once("data-mode=\"temperature\"")
            .expect("bulk temperature toggle missing")
            .1
            .split_once("</button>")
            .expect("bulk temperature toggle close missing")
            .0;
        assert!(
            !temp_btn.contains("hx-post"),
            "bulk Temp toggle must be visual only, got: {temp_btn}"
        );

        // Clicking the 22 C temperature preset re-renders the bar in
        // temperature mode (the temp toggle becomes active and the airflow
        // preset row is the hidden one).
        let body = client()
            .post(format!("http://{addr}/zones/preset"))
            .form(&[("mode", "temperature"), ("value", "22")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            body.contains("data-bulk-mode=\"temperature\""),
            "bulk bar must switch to temperature mode after a temp preset, got: {body}"
        );
        let temp_active = body
            .split_once("data-mode=\"temperature\"")
            .expect("bulk temperature toggle missing")
            .1
            .split_once("</button>")
            .expect("bulk temperature toggle close missing")
            .0;
        assert!(
            temp_active.contains("class=\"active\""),
            "bulk Temp toggle must be active after a temp preset, got: {temp_active}"
        );
    })
    .await;
}

#[tokio::test]
async fn bulk_power_turns_every_zone_on_and_off() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        // Turn every zone off in one shot.
        let body = client()
            .post(format!("http://{addr}/zones/power"))
            .form(&[("power", "off")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        for id in [0, 1, 2, 3, 7] {
            let row = body
                .split_once(&format!("id=\"zone-{id}\""))
                .unwrap_or_else(|| panic!("zone {id} row missing"))
                .1;
            let row = row
                .split_once("id=\"zone-")
                .map(|(seg, _)| seg)
                .unwrap_or(row);
            assert!(
                row.contains("zone-toggle off"),
                "zone {id} must be off after bulk off, got: {row}"
            );
        }

        // Turn every zone back on.
        let body = client()
            .post(format!("http://{addr}/zones/power"))
            .form(&[("power", "on")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        for id in [0, 1, 2, 3, 7] {
            let row = body
                .split_once(&format!("id=\"zone-{id}\""))
                .unwrap_or_else(|| panic!("zone {id} row missing"))
                .1;
            let row = row
                .split_once("id=\"zone-")
                .map(|(seg, _)| seg)
                .unwrap_or(row);
            assert!(
                !row.contains("zone-toggle off"),
                "zone {id} must not be off after bulk on, got: {row}"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn bulk_preset_airflow_sets_every_zone() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        // Apply the 50% airflow preset to every zone.
        let body = client()
            .post(format!("http://{addr}/zones/preset"))
            .form(&[("mode", "airflow"), ("value", "50")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(body.contains("Living Room"));
        // Every zone row must now report 50% airflow, including the ones that
        // were previously in temperature mode (zones 0 and 3).
        for id in [0, 2, 3, 7] {
            let row = body
                .split_once(&format!("id=\"zone-{id}\""))
                .unwrap_or_else(|| panic!("zone {id} row missing"))
                .1;
            // Stop at the next zone row so we only inspect this one.
            let row = row
                .split_once("id=\"zone-")
                .map(|(seg, _)| seg)
                .unwrap_or(row);
            assert!(
                row.contains("50%"),
                "zone {id} must show 50% after bulk airflow preset, got: {row}"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn bulk_preset_temperature_sets_sensor_zones_only() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        // Apply the 22 C preset: every sensor zone must land on a 22.0 C
        // setpoint in temperature mode. Sensorless zones are untouched.
        let body = client()
            .post(format!("http://{addr}/zones/preset"))
            .form(&[("mode", "temperature"), ("value", "22")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        // Sensor zones: 0 (Living Room), 2 (Kitchen), 3 (Study), 7 (Bathroom).
        for id in [0, 2, 3, 7] {
            let row = body
                .split_once(&format!("id=\"zone-{id}\""))
                .unwrap_or_else(|| panic!("zone {id} row missing"))
                .1;
            let row = row
                .split_once("id=\"zone-")
                .map(|(seg, _)| seg)
                .unwrap_or(row);
            assert!(
                row.contains("22.0"),
                "sensor zone {id} must show 22.0 C after bulk temp preset, got: {row}"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn bulk_preset_rejects_invalid_value() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        // An out-of-range airflow value must be rejected with 422 and must not
        // mutate any zone.
        let resp = client()
            .post(format!("http://{addr}/zones/preset"))
            .form(&[("mode", "airflow"), ("value", "150")])
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::UNPROCESSABLE_ENTITY,
            "out-of-range airflow preset must be 422"
        );
        // Zone 2 (Kitchen) is still at its original 80%.
        let body = client()
            .get(format!("http://{addr}/partials/zones/2"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            body.contains("80%"),
            "zone 2 must be unchanged, got: {body}"
        );
    })
    .await;
}

#[tokio::test]
async fn bulk_temp_button_disabled_without_any_sensors() {
    capped(async {
        let (addr, mock) = spawn_server().await;
        // Strip sensors from every zone: the bulk Temp button must render
        // disabled and the % button must be the active one.
        mock.mutate(|s| {
            for z in s.zones.values_mut() {
                z.has_sensor = false;
                z.sensor = None;
                z.control_mode = ControlModeView::Airflow;
                z.setpoint = None;
            }
        })
        .await;

        let body = loop {
            let b = client()
                .get(format!("http://{addr}/partials/zones"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            // Wait until the snapshot without sensors has propagated.
            if !b.contains("\u{b0}C</button>") || b.contains("disabled") {
                break b;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };

        let temp_btn = body
            .split_once("data-mode=\"temperature\"")
            .expect("bulk temperature toggle missing")
            .1
            .split_once(">\u{b0}C</button>")
            .expect("bulk temp toggle close missing")
            .0;
        assert!(
            temp_btn.contains("disabled"),
            "bulk Temp button must be disabled when no zone has a sensor, got: {temp_btn}"
        );
        // With no sensors the temperature preset row must not render at all.
        assert!(
            !body.contains("preset-row preset-temperature"),
            "temperature preset row must not render without sensors, got: {body}"
        );
        assert!(
            body.contains("data-bulk-mode=\"airflow\""),
            "bulk bar must stay in airflow mode without sensors, got: {body}"
        );
    })
    .await;
}

#[tokio::test]
async fn ac_setpoint_renders_updated_value() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        let body = client()
            .post(format!("http://{addr}/ac/0/setpoint"))
            .form(&[("temp", "22.0")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(body.contains("22.0"), "expected setpoint 22.0, got: {body}");
        assert!(body.contains("Whole House"));
    })
    .await;
}

#[tokio::test]
async fn ac_power_toggle_turns_off() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        // AC 0 is On; toggling -> Off. The OFF button should now be the
        // selected (uniform accent) one and the ON button neutral. There is no
        // per-state color theme anymore -- a single `selected` class marks the
        // active button.
        let body = client()
            .post(format!("http://{addr}/ac/0/power"))
            .form(&[("power", "toggle")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        // The OFF button is the selected one.
        assert!(
            body.contains("class=\"btn selected\"\n              hx-post=\"/ac/0/power\" hx-vals='{\"power\":\"off\"}'"),
            "expected the OFF button to be selected, got: {body}"
        );
        // No power button carries a per-state theme class anymore.
        assert!(
            !body.contains("class=\"btn on\"") && !body.contains("class=\"btn off\""),
            "power buttons should use the uniform `selected` class, got: {body}"
        );
        assert!(
            !body.contains("power-badge"),
            "the top-right power badge should be gone, got: {body}"
        );
    })
    .await;
}

#[tokio::test]
async fn ac_power_on_rejected_when_all_zones_off() {
    capped(async {
        let (addr, mock) = spawn_server().await;
        // Turn every zone on AC 0 off at the wall console.
        mock.mutate(|s| {
            for z in s.zones.values_mut() {
                if z.ac_id == Some(0) {
                    z.power = ZonePowerView::Off;
                }
            }
        })
        .await;

        // Starting the AC now must be rejected (422) with a helpful message.
        let resp = client()
            .post(format!("http://{addr}/ac/0/power"))
            .form(&[("power", "on")])
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        let body = resp.text().await.unwrap();
        assert!(
            body.contains("at least one zone"),
            "expected a 'turn on a zone' message, got: {body}"
        );
    })
    .await;
}

#[tokio::test]
async fn ac_power_toggle_rejected_when_all_zones_off() {
    capped(async {
        let (addr, mock) = spawn_server().await;
        // AC 0 is On; turn it off, and turn every zone off.
        mock.mutate(|s| {
            if let Some(ac) = s.acs.get_mut(&0)
                && let Some(st) = ac.status.as_mut()
            {
                st.power = Some("Off");
            }
            for z in s.zones.values_mut() {
                if z.ac_id == Some(0) {
                    z.power = ZonePowerView::Off;
                }
            }
        })
        .await;

        // Toggling (which would turn the AC on) must be rejected while all
        // zones are off.
        let resp = client()
            .post(format!("http://{addr}/ac/0/power"))
            .form(&[("power", "toggle")])
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    })
    .await;
}

#[tokio::test]
async fn ac_power_on_allowed_when_a_zone_is_on() {
    capped(async {
        let (addr, mock) = spawn_server().await;
        // Turn the AC off but leave zone 0 on, then start the AC: allowed.
        mock.mutate(|s| {
            if let Some(ac) = s.acs.get_mut(&0)
                && let Some(st) = ac.status.as_mut()
            {
                st.power = Some("Off");
            }
            if let Some(z) = s.zones.get_mut(&0) {
                z.power = ZonePowerView::On;
            }
            for z in s.zones.values_mut() {
                if z.id != 0 && z.ac_id == Some(0) {
                    z.power = ZonePowerView::Off;
                }
            }
        })
        .await;

        let resp = client()
            .post(format!("http://{addr}/ac/0/power"))
            .form(&[("power", "on")])
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
    })
    .await;
}

#[tokio::test]
async fn unknown_zone_id_is_422() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        let resp = client()
            .post(format!("http://{addr}/zone/99/power"))
            .form(&[("power", "on")])
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    })
    .await;
}

#[tokio::test]
async fn vendor_assets_cached_immutable() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        let resp = client()
            .get(format!("http://{addr}/vendor/htmx-2.0.4.js"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let cc = resp
            .headers()
            .get(reqwest::header::CACHE_CONTROL)
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        assert!(
            cc.contains("immutable") && cc.contains("max-age=31536000"),
            "expected long-immutable cache-control, got: {cc:?}"
        );
        let body = resp.text().await.unwrap();
        assert!(!body.is_empty(), "htmx body empty");
    })
    .await;
}

#[tokio::test]
async fn icon_asset_is_served() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        let resp = client()
            .get(format!("http://{addr}/icons/battery-low.svg"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body = resp.text().await.unwrap();
        assert!(
            body.contains("<svg"),
            "expected an SVG body, got: {body}"
        );
    })
    .await;
}

#[tokio::test]
async fn refresh_repulls_status() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        // The mock's Refresh just re-publishes; the handler returns the system
        // fragment.
        let resp = client()
            .post(format!("http://{addr}/refresh"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body = resp.text().await.unwrap();
        assert!(body.contains("id=\"system\""));
    })
    .await;
}

#[tokio::test]
async fn sse_emits_zone_fragment_on_live_change() {
    capped(async {
        let (addr, mock_ctrl) = spawn_server().await;

        // Open the SSE stream.
        let resp = client()
            .get(format!("http://{addr}/events"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let mut stream = resp.bytes_stream();

        // Let the initial full render flush, then inject a live change: turn
        // Kitchen (zone 2, currently On) Off at the "wall console".
        let injector = mock_ctrl.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            injector
                .mutate(|s| {
                    if let Some(z) = s.zones.get_mut(&2) {
                        z.power = ZonePowerView::Off;
                    }
                })
                .await;
        });

        // Collect SSE events until we see the post-mutation zone-2 fragment
        // (it carries `zone-row off`, which the initial render did not).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut buf = Vec::<u8>::new();
        let mut saw_zone_off = false;
        while !saw_zone_off {
            let chunk = tokio::time::timeout_at(deadline, stream.next())
                .await
                .expect("SSE timed out waiting for the mutation event");
            let chunk = chunk.expect("stream errored").expect("chunk errored");
            buf.extend_from_slice(&chunk);

            while let Some(idx) = buf.windows(2).position(|w| w == b"\n\n") {
                let raw = buf.drain(..idx + 2).collect::<Vec<_>>();
                if let Some((event, data)) = parse_sse_event(&raw) {
                    if event == "zone-2" && data.contains("zone-row off") {
                        saw_zone_off = true;
                    }
                }
            }
        }
        assert!(saw_zone_off, "did not see the post-mutation zone-2 event");
    })
    .await;
}

/// Parse one SSE event block (raw bytes ending in `\n\n`) into `(event, data)`.
/// Comment lines (`:`) and blank lines are ignored; `data:` lines are joined.
fn parse_sse_event(raw: &[u8]) -> Option<(String, String)> {
    let text = std::str::from_utf8(raw).ok()?;
    let mut event = String::new();
    let mut data = String::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => event = value.to_string(),
            "data" => {
                if data.is_empty() {
                    data = value.to_string();
                } else {
                    data.push('\n');
                    data.push_str(value);
                }
            }
            _ => {}
        }
    }
    if event.is_empty() && data.is_empty() {
        return None;
    }
    Some((event, data))
}

/// Extract the card for one program from the automation partial body, using the
/// stable `data-program="..."` attribute as the split anchor (the cards also
/// have HTML comments that contain the program name, so splitting on the name
/// alone is ambiguous).
fn program_card<'a>(body: &'a str, program: &str) -> &'a str {
    let anchor = format!("data-program=\"{program}\"");
    body.split(&anchor)
        .nth(1)
        .unwrap_or("")
        .split("program-card")
        .next()
        .unwrap_or("")
}

#[tokio::test]
async fn index_renders_automation_section() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        let body = client()
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(body.contains("Automation"), "section label missing");
        assert!(
            body.contains("Setpoint auto-off"),
            "setpoint program missing"
        );
        assert!(body.contains("Idle auto-off"), "idle program missing");
        // Both programs default to disabled: the toggle reads "Disabled" with the
        // off styling.
        let setpoint = program_card(&body, "setpoint-off");
        assert!(
            setpoint.contains("program-toggle off") && setpoint.contains("Disabled"),
            "setpoint program should default to Disabled: {setpoint}"
        );
        let idle = program_card(&body, "idle-off");
        assert!(
            idle.contains("program-toggle off") && idle.contains("Disabled"),
            "idle program should default to Disabled: {idle}"
        );
    })
    .await;
}

#[tokio::test]
async fn automation_partial_get() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        let body = client()
            .get(format!("http://{addr}/partials/automation"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(body.contains("id=\"automation\""));
        assert!(body.contains("Setpoint auto-off"));
        assert!(body.contains("Idle auto-off"));
    })
    .await;
}

#[tokio::test]
async fn toggle_setpoint_off_enables_and_reflects() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        let body = client()
            .post(format!("http://{addr}/automation/setpoint-off/toggle"))
            .form(&[("enabled", "true")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let setpoint = program_card(&body, "setpoint-off");
        // The toggle should now read "Enabled" with the on (green) styling.
        assert!(
            setpoint.contains("program-toggle on") && setpoint.contains("Enabled"),
            "toggle should read Enabled after enabling: {setpoint}"
        );
        // The hold presets should now be enabled (not disabled).
        assert!(
            setpoint.contains(">15m</button>"),
            "15m preset present: {setpoint}"
        );
        assert!(
            !setpoint.contains("preset\") disabled"),
            "presets should be enabled when the program is on: {setpoint}"
        );
    })
    .await;
}

#[tokio::test]
async fn set_setpoint_off_hold_rejects_unknown_preset() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        let resp = client()
            .post(format!("http://{addr}/automation/setpoint-off/hold"))
            .form(&[("mins", "7")])
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            422,
            "unknown hold preset should be rejected"
        );
    })
    .await;
}

#[tokio::test]
async fn set_idle_off_timeout_persists_value() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        // Enable first so the presets are interactive.
        let _ = client()
            .post(format!("http://{addr}/automation/idle-off/toggle"))
            .form(&[("enabled", "true")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let body = client()
            .post(format!("http://{addr}/automation/idle-off/timeout"))
            .form(&[("mins", "120")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let idle = program_card(&body, "idle-off");
        assert!(idle.contains(">2h</button>"), "2h preset present: {idle}");
        // The 120-min (2h) preset button should be the active one. The button
        // markup is `class="btn preset{% if ... == 120 %} active{% endif %}"`,
        // so the ` active` class appears right before the `hx-vals` for 120.
        let before_120 = idle.split("mins\":\"120\"").next().unwrap();
        assert!(
            before_120
                .rsplit("btn preset")
                .next()
                .unwrap()
                .contains(" active"),
            "120-min preset should be active: {idle}"
        );
    })
    .await;
}

#[tokio::test]
async fn toggle_idle_off_then_disable_resets_active() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        // Enable.
        let body = client()
            .post(format!("http://{addr}/automation/idle-off/toggle"))
            .form(&[("enabled", "true")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            program_card(&body, "idle-off").contains("program-toggle on")
                && program_card(&body, "idle-off").contains("Enabled")
        );
        // Disable again.
        let body = client()
            .post(format!("http://{addr}/automation/idle-off/toggle"))
            .form(&[("enabled", "false")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            program_card(&body, "idle-off").contains("program-toggle off")
                && program_card(&body, "idle-off").contains("Disabled"),
            "idle should be Disabled after disabling"
        );
    })
    .await;
}

#[tokio::test]
async fn setpoint_hold_preset_marks_active() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        let _ = client()
            .post(format!("http://{addr}/automation/setpoint-off/toggle"))
            .form(&[("enabled", "true")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let body = client()
            .post(format!("http://{addr}/automation/setpoint-off/hold"))
            .form(&[("mins", "60")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let setpoint = program_card(&body, "setpoint-off");
        // 1h (60 min) should be active; 15m should not.
        let before_60 = setpoint.split("mins\":\"60\"").next().unwrap();
        assert!(
            before_60
                .rsplit("btn preset")
                .next()
                .unwrap()
                .contains(" active"),
            "60-min hold should be active: {setpoint}"
        );
        let before_15 = setpoint.split("mins\":\"15\"").next().unwrap();
        assert!(
            !before_15
                .rsplit("btn preset")
                .next()
                .unwrap()
                .contains(" active"),
            "15-min hold should NOT be active: {setpoint}"
        );
    })
    .await;
}

/// With the setpoint auto-off program enabled but the on-zone still above its
/// setpoint, the card shows a muted "waiting for setpoint" status line.
#[tokio::test]
async fn setpoint_off_status_shows_waiting_badge() {
    capped(async {
        let (addr, _mock) = spawn_server().await;
        // Enable the program.
        let _ = client()
            .post(format!("http://{addr}/automation/setpoint-off/toggle"))
            .form(&[("enabled", "true")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let body = client()
            .get(format!("http://{addr}/partials/automation"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let setpoint = program_card(&body, "setpoint-off");
        assert!(
            setpoint.contains("program-status wait"),
            "expected a waiting status badge: {setpoint}"
        );
        assert!(
            setpoint.contains("Waiting for setpoint"),
            "expected waiting copy: {setpoint}"
        );
        assert!(
            setpoint.contains("zones reached"),
            "expected zone progress copy: {setpoint}"
        );
    })
    .await;
}

/// When every on-zone has reached its setpoint the card shows the green
/// countdown badge with the hold time (the engine is not spawned in the test
/// harness, so the full hold is shown as remaining).
#[tokio::test]
async fn setpoint_off_status_shows_countdown_badge() {
    capped(async {
        let (addr, mock) = spawn_server().await;
        // Drive the mock into a minimal at-setpoint state: one on zone in
        // temperature mode with a reading at/under its Cool setpoint.
        mock.mutate(|s| {
            let off_ids = [1u8, 2, 3, 6, 7];
            for id in off_ids {
                if let Some(z) = s.zones.get_mut(&id) {
                    z.power = ZonePowerView::Off;
                }
            }
            let z = s.zones.get_mut(&0).unwrap();
            z.power = ZonePowerView::On;
            z.control_mode = ControlModeView::Temperature;
            z.setpoint = Some(airtouch5::types::Temperature::from_float(23.0));
            z.sensor = Some(aircon::manager::snapshot::SensorView::Temperature(
                airtouch5::types::Temperature::from_float(22.0),
            ));
            let ac = s.acs.get_mut(&0).unwrap();
            let st = ac.status.as_mut().unwrap();
            st.power = Some("On");
            st.mode = Some("Cool");
        })
        .await;

        // Enable the program and fetch the card, polling until the mock has
        // republished the mutated snapshot and the countdown badge appears.
        let _ = client()
            .post(format!("http://{addr}/automation/setpoint-off/toggle"))
            .form(&[("enabled", "true")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let setpoint = loop {
            let body = client()
                .get(format!("http://{addr}/partials/automation"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            let card = program_card(&body, "setpoint-off");
            if card.contains("program-status ok") {
                break card.to_string();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert!(
            setpoint.contains("All zones at setpoint"),
            "expected at-setpoint copy: {setpoint}"
        );
        assert!(
            setpoint.contains("powering system off at"),
            "expected the wall-clock target-time copy: {setpoint}"
        );
        assert!(
            target_time(&setpoint).is_some(),
            "expected an HH:MM target time in the at-setpoint badge: {setpoint}"
        );
    })
    .await;
}

/// When an On AC is in a non-heating/cooling mode (Fan/Dry/Auto) the setpoint
/// auto-off card shows the amber "not active for this mode" note instead of
/// the countdown/waiting status -- even if a zone is sitting at its setpoint.
#[tokio::test]
async fn setpoint_off_status_shows_mode_note_when_not_heating_or_cooling() {
    capped(async {
        let (addr, mock) = spawn_server().await;
        // Drive the mock into a state that would otherwise be "at setpoint"
        // (one on temp zone at its Cool setpoint), then flip the AC to Fan --
        // a non-heating/cooling mode.
        mock.mutate(|s| {
            let off_ids = [1u8, 2, 3, 6, 7];
            for id in off_ids {
                if let Some(z) = s.zones.get_mut(&id) {
                    z.power = ZonePowerView::Off;
                }
            }
            let z = s.zones.get_mut(&0).unwrap();
            z.power = ZonePowerView::On;
            z.control_mode = ControlModeView::Temperature;
            z.setpoint = Some(airtouch5::types::Temperature::from_float(23.0));
            z.sensor = Some(aircon::manager::snapshot::SensorView::Temperature(
                airtouch5::types::Temperature::from_float(22.0),
            ));
            let ac = s.acs.get_mut(&0).unwrap();
            let st = ac.status.as_mut().unwrap();
            st.power = Some("On");
            st.mode = Some("Fan");
        })
        .await;

        let _ = client()
            .post(format!("http://{addr}/automation/setpoint-off/toggle"))
            .form(&[("enabled", "true")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        let setpoint = loop {
            let body = client()
                .get(format!("http://{addr}/partials/automation"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            let card = program_card(&body, "setpoint-off");
            if card.contains("program-status note") {
                break card.to_string();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert!(
            setpoint.contains("Not active for this mode"),
            "expected the mode note: {setpoint}"
        );
        assert!(
            setpoint.contains("heating and cooling"),
            "expected the heating/cooling hint: {setpoint}"
        );
        // No countdown or waiting badge should be shown alongside the note.
        assert!(
            !setpoint.contains("program-status ok"),
            "no countdown badge when mode is ineligible: {setpoint}"
        );
        assert!(
            !setpoint.contains("program-status wait"),
            "no waiting badge when mode is ineligible: {setpoint}"
        );
    })
    .await;
}

/// Enabling the idle auto-off program shows the "Powering system off at HH:MM"
/// status line (the AC is on in the sample snapshot).
#[tokio::test]
async fn idle_off_status_shows_target_time() {
    capped(async {
        let (addr, _mock) = spawn_server().await;
        let _ = client()
            .post(format!("http://{addr}/automation/idle-off/toggle"))
            .form(&[("enabled", "true")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let body = client()
            .get(format!("http://{addr}/partials/automation"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let idle = program_card(&body, "idle-off");
        assert!(
            idle.contains("program-status wait"),
            "expected an idle status badge: {idle}"
        );
        assert!(
            idle.contains("Powering system off at "),
            "expected the powering-off copy: {idle}"
        );
    })
    .await;
}

/// Changing the idle timeout preset changes the displayed target time: the
/// status line must reflect the new (later) shutoff time after the interaction.
#[tokio::test]
async fn idle_off_status_target_changes_with_timeout() {
    capped(async {
        let (addr, _mock) = spawn_server().await;
        // Enable with the default 30m timeout and capture the target time.
        let _ = client()
            .post(format!("http://{addr}/automation/idle-off/toggle"))
            .form(&[("enabled", "true")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let body = client()
            .get(format!("http://{addr}/partials/automation"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let before = program_card(&body, "idle-off");
        let t0 = target_time(before).expect("target time present before");

        // Bump the timeout to 2h and re-render; the target must move later.
        let body = client()
            .post(format!("http://{addr}/automation/idle-off/timeout"))
            .form(&[("mins", "120")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let after = program_card(&body, "idle-off");
        let t1 = target_time(after).expect("target time present after");
        assert_ne!(
            t0, t1,
            "target time should change after switching the timeout preset"
        );
    })
    .await;
}

/// Extract the "HH:MM" target time from an idle program card's status line.
fn target_time(card: &str) -> Option<String> {
    // Both cards say "...system off at HH:MM"; match on the common tail so it
    // works for the setpoint card ("...powering system off at") and the idle
    // card ("Powering system off at").
    let anchor = "system off at ";
    let rest = card.split(anchor).nth(1)?;
    let hhmm = rest.split('<').next()?.trim();
    if hhmm.len() == 5 && hhmm.as_bytes()[2] == b':' {
        Some(hhmm.to_string())
    } else {
        None
    }
}

/// The SSE initial full render emits an `automation` event carrying the idle
/// auto-off "Powering system off at HH:MM" status line once the program is
/// enabled, so a freshly-connected browser shows the target time live.
#[tokio::test]
async fn sse_initial_render_emits_idle_status() {
    capped(async {
        let (addr, _mock) = spawn_server().await;
        // Enable the idle program so the countdown (and target time) is active.
        let _ = client()
            .post(format!("http://{addr}/automation/idle-off/toggle"))
            .form(&[("enabled", "true")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        let resp = client()
            .get(format!("http://{addr}/events"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let mut stream = resp.bytes_stream();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut buf = Vec::<u8>::new();
        let mut saw_idle_status = false;
        while !saw_idle_status {
            let chunk = tokio::time::timeout_at(deadline, stream.next())
                .await
                .expect("SSE timed out waiting for the automation event");
            let chunk = chunk.expect("stream errored").expect("chunk errored");
            buf.extend_from_slice(&chunk);
            while let Some(idx) = buf.windows(2).position(|w| w == b"\n\n") {
                let raw = buf.drain(..idx + 2).collect::<Vec<_>>();
                if let Some((event, data)) = parse_sse_event(&raw) {
                    if event == "automation"
                        && data.contains("Powering system off at ")
                        && target_time(&data).is_some()
                    {
                        saw_idle_status = true;
                    }
                }
            }
        }
        assert!(
            saw_idle_status,
            "initial SSE render should include the idle target-time status"
        );
    })
    .await;
}

#[tokio::test]
async fn theme_cookie_round_trip() {
    capped(async {
        let (addr, _m) = spawn_server().await;

        // No cookie -> the default theme (midnight) is rendered.
        let body = client()
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            body.contains(r#"<html lang="en" data-theme="midnight">"#),
            "default data-theme missing"
        );
        assert!(
            body.contains(r##"<meta name="theme-color" content="#0f1115">"##),
            "default theme-color missing"
        );
        // Every theme gets a selector button.
        for name in ["midnight", "daylight", "terminal", "ember", "contrast"] {
            assert!(
                body.contains(&format!(r#"data-set-theme="{name}""#)),
                "selector button for {name} missing"
            );
        }

        // Selecting a theme sets a long-lived cookie; the body stays empty
        // (the client applies the theme itself, hx-swap="none").
        let resp = client()
            .post(format!("http://{addr}/theme"))
            .form(&[("name", "terminal")])
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let set_cookie = resp
            .headers()
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            set_cookie.contains("theme=terminal"),
            "cookie: {set_cookie}"
        );
        assert!(
            set_cookie.contains("Max-Age=31536000"),
            "cookie: {set_cookie}"
        );
        assert!(resp.text().await.unwrap().is_empty());

        // The index then renders that theme from the cookie.
        let body = client()
            .get(format!("http://{addr}/"))
            .header("cookie", "theme=ember")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(body.contains(r#"data-theme="ember""#));
        assert!(body.contains(r##"content="#171210""##));

        // Unknown values (stale cookie, bogus POST) fall back to the default.
        let body = client()
            .get(format!("http://{addr}/"))
            .header("cookie", "theme=bogus")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(body.contains(r#"data-theme="midnight""#));
        let resp = client()
            .post(format!("http://{addr}/theme"))
            .form(&[("name", "bogus")])
            .send()
            .await
            .unwrap();
        let set_cookie = resp
            .headers()
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            set_cookie.contains("theme=midnight"),
            "bogus theme should sanitize to default, got: {set_cookie}"
        );
    })
    .await;
}
