//! End-to-end integration tests.
//!
//! Each test spawns a mock controller + the real axum router on a task bound to
//! `127.0.0.1:0`, then drives it with a real `reqwest` HTTP client (including a
//! streaming SSE connection). Every test is wrapped in a `tokio::time::timeout`
//! hard cap so it can never hang.

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::StreamExt;

use aircon::mock::{self, MockController};
use aircon::manager::snapshot::{ControlModeView, ZonePowerView};
use aircon::web;

/// Per-test hard timeout. Tests against the mock controller are instant; this is
/// a safety net against accidental hangs (e.g. an SSE read that never completes).
const TEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Spawn the mock controller behind the real router on an ephemeral port.
async fn spawn_server() -> (SocketAddr, MockController) {
    let (manager, mock_ctrl) = mock::spawn_mock_controller(mock::sample_snapshot());
    let app = web::build_router(manager);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        // No graceful shutdown: cancelled when the test runtime drops.
        let _ = axum::serve(listener, app).await;
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

        // The airflow button tag spans from its opening `<button` to `% </button>`;
        // it must not carry a `disabled` attribute.
        let airflow_btn = body
            .split_once("{\"type\":\"airflow\"}")
            .expect("airflow control-type button missing")
            .1
            .split_once("% </button>")
            .expect("airflow button closing tag missing")
            .0;
        assert!(
            !airflow_btn.contains("disabled"),
            "airflow (%) button must stay enabled when off/sensorless, got: {body}"
        );

        // The temperature button, by contrast, must remain disabled.
        let temp_btn = body
            .split_once("{\"type\":\"temperature\"}")
            .expect("temperature control-type button missing")
            .1
            .split_once(">Temp</button>")
            .expect("temp button closing tag missing")
            .0;
        assert!(
            temp_btn.contains("disabled"),
            "temperature button must be disabled without a sensor, got: {body}"
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
        assert!(body.contains("21.5"), "setpoint must be settable while zone off, got: {body}");

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
                    .split_once(&format!(">{}</button>",
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
        // the derived bulk mode is airflow (%) -- the airflow preset row
        // (25/50/75/100) must show and the temperature presets must not.
        let body = client()
            .get(format!("http://{addr}/partials/zones"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(body.contains("All zones"), "bulk bar label missing");
        assert!(
            body.contains("/zones/control-type"),
            "bulk mode toggle buttons missing"
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
        assert!(
            !body.contains("\"mode\":\"temperature\""),
            "temperature presets must not render in airflow bulk mode, got: {body}"
        );
    })
    .await;
}

#[tokio::test]
async fn bulk_control_type_airflow_switches_all_zones() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        // Living Room (zone 0) and Study (zone 3) start in temperature mode.
        // Switching every zone to airflow must move them out of temperature
        // mode and re-render the bulk bar with the % button active.
        let body = client()
            .post(format!("http://{addr}/zones/control-type"))
            .form(&[("type", "airflow")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        // The bulk % button is the active one now.
        let airflow_btn = body
            .split_once("{\"type\":\"airflow\"}")
            .expect("bulk airflow button missing")
            .1
            .split_once("% </button>")
            .expect("bulk airflow button close missing")
            .0;
        assert!(
            airflow_btn.contains("class=\"active\""),
            "bulk % button must be active after airflow select, got: {airflow_btn}"
        );

        // The temperature presets must no longer render.
        assert!(
            !body.contains("\"mode\":\"temperature\""),
            "temperature presets must not render in airflow bulk mode, got: {body}"
        );

        // Living Room (zone 0) was in temperature mode; it must now render an
        // airflow percentage (its airflow_pct is 0 -> "0%") instead of a
        // setpoint.
        let zone0 = body
            .split_once("id=\"zone-0\"")
            .expect("zone 0 row missing")
            .1
            .split_once("id=\"zone-")
            .map(|(seg, _)| seg)
            .unwrap_or_else(|| body.split_once("id=\"zone-0\"").unwrap().1);
        assert!(
            zone0.contains("0%"),
            "zone 0 must be in airflow mode after bulk switch, got: {zone0}"
        );
    })
    .await;
}

#[tokio::test]
async fn bulk_control_type_temperature_skips_sensorless_zones() {
    capped(async {
        let (addr, _m) = spawn_server().await;
        // Kitchen (zone 2) has a sensor and starts in airflow mode at 80%.
        // Bedroom (zone 1) has no sensor. Switching all zones to temperature
        // must move Kitchen into temperature mode but leave Bedroom alone
        // (sensorless zones cannot be temperature-controlled).
        let body = client()
            .post(format!("http://{addr}/zones/control-type"))
            .form(&[("type", "temperature")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        // Bulk Temp button is active and the temperature presets render.
        let temp_btn = body
            .split_once("{\"type\":\"temperature\"}")
            .expect("bulk temperature button missing")
            .1
            .split_once(">Temp</button>")
            .expect("bulk temp button close missing")
            .0;
        assert!(
            temp_btn.contains("class=\"active\""),
            "bulk Temp button must be active after temperature select, got: {temp_btn}"
        );
        for t in ["20", "21", "22", "23"] {
            assert!(
                body.contains(&format!("{{\"mode\":\"temperature\",\"value\":\"{t}\"}}")),
                "temperature preset {t:?} missing, got: {body}"
            );
        }

        // Kitchen (zone 2) is now in temperature mode with a fresh 20.0 C
        // setpoint (the mock seeds 20.0 when switching to temperature).
        let zone2 = body
            .split_once("id=\"zone-2\"")
            .expect("zone 2 row missing")
            .1
            .split_once("id=\"zone-3\"")
            .map(|(seg, _)| seg)
            .unwrap_or_else(|| body.split_once("id=\"zone-2\"").unwrap().1);
        assert!(
            zone2.contains("20.0"),
            "zone 2 must be in temperature mode after bulk switch, got: {zone2}"
        );

        // Bedroom (zone 1) stays in airflow mode (still shows "20%").
        let zone1 = body
            .split_once("id=\"zone-1\"")
            .expect("zone 1 row missing")
            .1
            .split_once("id=\"zone-2\"")
            .map(|(seg, _)| seg)
            .unwrap_or_else(|| body.split_once("id=\"zone-1\"").unwrap().1);
        assert!(
            zone1.contains("20%"),
            "sensorless zone 1 must remain in airflow mode, got: {zone1}"
        );
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
            let row = row.split_once("id=\"zone-").map(|(seg, _)| seg).unwrap_or(row);
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
            let row = row.split_once("id=\"zone-").map(|(seg, _)| seg).unwrap_or(row);
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
        assert!(body.contains("80%"), "zone 2 must be unchanged, got: {body}");
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
            if !b.contains("Temp</button>") || b.contains("disabled") {
                break b;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };

        let temp_btn = body
            .split_once("{\"type\":\"temperature\"}")
            .expect("bulk temperature button missing")
            .1
            .split_once(">Temp</button>")
            .expect("bulk temp button close missing")
            .0;
        assert!(
            temp_btn.contains("disabled"),
            "bulk Temp button must be disabled when no zone has a sensor, got: {temp_btn}"
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
