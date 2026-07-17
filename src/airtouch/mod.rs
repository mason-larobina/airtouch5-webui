//! Thin helpers around the `airtouch5` crate.
//!
//! `connect_and_prefill` connects, fetches capabilities/names/version, primes
//! the live-status watch, and builds a `StaticInfo` ready for the manager to
//! render from. The `airtouch5` response wrapper types live in a private
//! module, so we extract the primitive data we need (using type inference)
//! rather than naming them.

use std::collections::BTreeMap;
use std::time::Duration;

use airtouch5::discovery::{discover_timeout, Console, DiscoveryError};
use airtouch5::AirTouch5;

use crate::manager::snapshot::{AcCap, StaticInfo};

/// Discover a console, retrying with an exponential backoff until one is found
/// or `timeout` elapses.
pub async fn discover_with_retry(timeout: Duration) -> Console {
    let mut backoff = Duration::from_millis(500);
    loop {
        match discover_timeout(Some(timeout)).await {
            Ok(c) => return c,
            Err(DiscoveryError::NoResponse) => {
                tracing::warn!(
                    "no AirTouch 5 console found; retrying in {:?}",
                    backoff
                );
            }
            Err(e) => {
                tracing::warn!("discovery error: {e}; retrying in {backoff:?}");
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// Connect to `console`, fetch the static info (capabilities, zone names,
/// console version), and prime the live-status watch. Returns the `AirTouch5`
/// handle plus a render-ready `StaticInfo`.
pub async fn connect_and_prefill(
    console: Console,
) -> std::io::Result<(AirTouch5, StaticInfo)> {
    let at5 = AirTouch5::with_ipaddr(console.address).await?;

    // Capabilities, names, and version are independent -> run concurrently.
    // The response wrapper types are private in the crate, so we let type
    // inference name them and consume them immediately.
    let (caps_resp, names_resp, version_resp) = tokio::try_join!(
        at5.ac_capabilities(),
        at5.zone_names(),
        at5.console_version(),
    )?;

    // Prime the internal status watch so subscribe_status() has data.
    let _ = at5.ac_status().await?;
    let _ = at5.zone_status().await?;

    // Extract AC capabilities into our own owned AcCap map.
    let mut caps = BTreeMap::new();
    for (id, cap) in caps_resp.by_index() {
        caps.insert(
            id,
            AcCap {
                id,
                name: cap.name.clone(),
                zone_start_index: cap.zone_start_index,
                zone_count: cap.zone_count,
                supported_modes: cap
                    .supported_modes
                    .iter_names()
                    .map(|(n, _)| n)
                    .collect(),
                // IntelligentAuto is a modifier, not a selectable base speed;
                // skip it in the segmented control (it has its own toggle).
                supported_fan_speeds: cap
                    .supported_fan_speeds
                    .iter_names()
                    .filter_map(|(n, _)| {
                        if n == "IntelligentAuto" {
                            None
                        } else {
                            Some(n)
                        }
                    })
                    .collect(),
                setpoint_cool: (cap.setpoint_cool_min, cap.setpoint_cool_max),
                setpoint_heat: (cap.setpoint_heat_min, cap.setpoint_heat_max),
            },
        );
    }

    let static_info = StaticInfo::from_data(
        &console,
        caps,
        names_resp.zones.clone(),
        version_resp.versions.clone(),
        version_resp.update_available,
    );

    Ok((at5, static_info))
}
