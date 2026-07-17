//! `/events`: a single SSE stream that pushes server-rendered HTML fragments.
//!
//! On each `snapshot_rx.changed()`, we diff the previous `Snapshot` against the
//! new one (the view structs are `PartialEq`) and only re-emit fragments whose
//! ids changed:
//!
//! - `system` event -> `#system` (console info changed)
//! - `state` event  -> `#connection-state` (connected flag changed)
//! - `ac-<id>` event -> `#ac-<id>` (one event per changed AC id)
//! - `zone-<id>` event -> `#zone-<id>` (one event per changed zone id)
//!
//! Each event's `data:` is the matching HTML fragment; the client's
//! `sse-swap="ac-<id>"` / `sse-swap="zone-<id>"` / `sse-swap="system"` /
//! `sse-swap="state"` listeners match the event name and swap the fragment
//! by its element id. Per-id event names are used (rather than a generic
//! `ac`/`zone`) because the htmx-sse extension swaps an event's data into
//! *every* element listening for that event name; per-id names isolate each
//! card to its own event.

use std::collections::VecDeque;
use std::convert::Infallible;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream::{self, Stream, StreamExt};

use crate::manager::snapshot::Snapshot;
use crate::templates;
use crate::web::state::AppState;

/// Axum handler for `GET /events`.
pub async fn sse_events(axum::extract::State(state): axum::extract::State<AppState>) -> Response {
    let rx = state.manager.snapshot_rx.clone();
    let stream = make_event_stream(rx);
    Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
}

/// Internal state carried across stream yields.
struct SseState {
    rx: tokio::sync::watch::Receiver<Snapshot>,
    prev: Snapshot,
    pending: VecDeque<Event>,
}

/// Build the SSE event stream from a `watch::Receiver<Snapshot>`.
///
/// Emits a full initial render (every fragment) so a freshly-connected browser
/// populates everything, then per-change diffs thereafter.
fn make_event_stream(
    rx: tokio::sync::watch::Receiver<Snapshot>,
) -> impl Stream<Item = Result<Event, Infallible>> + Send {
    let initial = rx.borrow().clone();
    let initial_events: Vec<Event> = full_events(&initial);

    let state = SseState {
        rx,
        prev: initial,
        pending: VecDeque::new(),
    };

    stream::iter(initial_events.into_iter().map(Ok)).chain(stream::unfold(state, |mut s| async move {
        loop {
            if let Some(ev) = s.pending.pop_front() {
                return Some((Ok(ev), s));
            }
            // Wait for the next snapshot change.
            if s.rx.changed().await.is_err() {
                // Sender dropped (manager gone) -> end the stream.
                return None;
            }
            let new = s.rx.borrow().clone();
            if new == s.prev {
                // No net change worth re-emitting.
                continue;
            }
            for ev in diff_events(&s.prev, &new) {
                s.pending.push_back(ev);
            }
            s.prev = new;
            // Loop back to drain `pending`.
        }
    }))
}

/// The full set of events for an initial render: state, system, every AC, every
/// zone. (We deliberately emit per-id `ac-<id>`/`zone-<id>` events rather than a
/// single `acs`/`zones` blob so the browser's `sse-swap` listeners on individual
/// cards fire.)
fn full_events(snap: &Snapshot) -> Vec<Event> {
    let mut out = Vec::new();
    out.push(named("state", templates::render_connection_state(snap)));
    out.push(named("system", templates::render_system(snap)));
    for ac in snap.acs.values() {
        out.push(named(&format!("ac-{}", ac.id), templates::render_ac(ac)));
    }
    for zone in snap.zones.values() {
        out.push(named(&format!("zone-{}", zone.id), templates::render_zone(zone)));
    }
    out
}

/// Diff two snapshots and emit only the changed fragments.
fn diff_events(prev: &Snapshot, new: &Snapshot) -> Vec<Event> {
    let mut out = Vec::new();

    if prev.connected != new.connected {
        out.push(named("state", templates::render_connection_state(new)));
    }
    if prev.console != new.console {
        out.push(named("system", templates::render_system(new)));
    }

    // Changed or newly-appearing ACs.
    for (id, ac) in &new.acs {
        let changed = match prev.acs.get(id) {
            Some(old) => old != ac,
            None => true,
        };
        if changed {
            out.push(named(&format!("ac-{}", id), templates::render_ac(ac)));
        }
    }

    // Changed or newly-appearing zones.
    for (id, zone) in &new.zones {
        let changed = match prev.zones.get(id) {
            Some(old) => old != zone,
            None => true,
        };
        if changed {
            out.push(named(&format!("zone-{}", id), templates::render_zone(zone)));
        }
    }

    out
}

/// Build a named SSE event whose data is an HTML fragment.
fn named(name: &str, html: String) -> Event {
    Event::default().event(name).data(html)
}
