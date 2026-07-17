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
use aircon::manager::snapshot::ZonePowerView;
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
        // AC 0 is On; toggling -> Off.
        let body = client()
            .post(format!("http://{addr}/ac/0/power"))
            .form(&[("power", "toggle")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            body.contains("power-badge off"),
            "expected off badge after toggle, got: {body}"
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
