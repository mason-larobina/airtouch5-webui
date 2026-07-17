//! Askama template struct definitions.
//!
//! Each struct maps to one template file under `templates/`. Fragment templates
//! render a single root element carrying a stable `id` plus `sse-swap` /
//! `hx-swap="outerHTML"` so the htmx-sse extension can swap them in place.

use askama::Template;

use crate::manager::snapshot::{AcView, BulkModeView, Snapshot, ZoneView};

#[derive(Template)]
#[template(path = "index.html")]
pub struct IndexTemplate<'a> {
    pub snapshot: &'a Snapshot,
    pub bulk_mode: BulkModeView,
}

pub fn render_index(snapshot: &Snapshot) -> String {
    IndexTemplate {
        snapshot,
        bulk_mode: snapshot.bulk_mode(),
    }
    .render()
    .unwrap_or_default()
}

#[derive(Template)]
#[template(path = "partials/connection_state.html")]
pub struct ConnectionStateTemplate<'a> {
    pub snapshot: &'a Snapshot,
}

#[derive(Template)]
#[template(path = "partials/system.html")]
pub struct SystemTemplate<'a> {
    pub snapshot: &'a Snapshot,
}

#[derive(Template)]
#[template(path = "partials/acs.html")]
pub struct AcsTemplate<'a> {
    pub snapshot: &'a Snapshot,
}

#[derive(Template)]
#[template(path = "partials/ac.html")]
pub struct AcTemplate<'a> {
    pub ac: &'a AcView,
}

#[derive(Template)]
#[template(path = "partials/zones.html")]
pub struct ZonesTemplate<'a> {
    pub snapshot: &'a Snapshot,
    pub bulk_mode: BulkModeView,
}

#[derive(Template)]
#[template(path = "partials/zone.html")]
pub struct ZoneTemplate<'a> {
    pub zone: &'a ZoneView,
}

/// Render a fragment to a String for use as an SSE `data:` payload or a POST
/// response body.
pub fn render_zone(zone: &ZoneView) -> String {
    ZoneTemplate { zone }.render().unwrap_or_default()
}

pub fn render_ac(ac: &AcView) -> String {
    AcTemplate { ac }.render().unwrap_or_default()
}

pub fn render_system(snapshot: &Snapshot) -> String {
    SystemTemplate { snapshot }.render().unwrap_or_default()
}

pub fn render_connection_state(snapshot: &Snapshot) -> String {
    ConnectionStateTemplate { snapshot }.render().unwrap_or_default()
}

pub fn render_acs(snapshot: &Snapshot) -> String {
    AcsTemplate { snapshot }.render().unwrap_or_default()
}

pub fn render_zones(snapshot: &Snapshot) -> String {
    render_zones_with_bulk(snapshot, snapshot.bulk_mode())
}

/// Render the zones partial with an explicit bulk-bar mode. Used by the bulk
/// control-type / preset POST handlers so the bar reflects the user's last
/// selection rather than only the live zone states.
pub fn render_zones_with_bulk(snapshot: &Snapshot, bulk_mode: BulkModeView) -> String {
    ZonesTemplate {
        snapshot,
        bulk_mode,
    }
    .render()
    .unwrap_or_default()
}
