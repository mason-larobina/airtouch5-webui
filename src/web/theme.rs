//! Color theme registry and cookie helpers.
//!
//! The palettes themselves live in `static/css/app.css` as
//! `:root[data-theme="<name>"]` custom-property overrides. This module is the
//! server-side source of truth for which themes exist: it drives the theme
//! selector markup, validates the `theme` cookie, and provides each theme's
//! background color for `<meta name="theme-color">`.

use axum::http::HeaderMap;
use axum::http::header::COOKIE;

/// A selectable color theme.
pub struct Theme {
    /// Cookie value, `data-theme` attribute value, and CSS selector suffix.
    pub name: &'static str,
    /// Human-readable label shown on the selector button.
    pub label: &'static str,
    /// The theme's `--bg` value, duplicated from app.css so the server can
    /// render a matching `<meta name="theme-color">`.
    pub bg: &'static str,
}

/// Every theme the UI offers. The first entry is the default; its palette is
/// the plain `:root` block in app.css (no attribute selector needed).
pub const THEMES: &[Theme] = &[
    Theme {
        name: "midnight",
        label: "Midnight",
        bg: "#0f1115",
    },
    Theme {
        name: "daylight",
        label: "Daylight",
        bg: "#eef1f5",
    },
    Theme {
        name: "terminal",
        label: "Terminal",
        bg: "#050a06",
    },
    Theme {
        name: "ember",
        label: "Ember",
        bg: "#171210",
    },
    Theme {
        name: "contrast",
        label: "Contrast",
        bg: "#ffffff",
    },
];

/// Look up a theme by name, falling back to the default for missing or
/// unknown values (e.g. a stale cookie after a theme is removed).
pub fn lookup(name: &str) -> &'static Theme {
    THEMES.iter().find(|t| t.name == name).unwrap_or(&THEMES[0])
}

/// Read and validate the `theme` cookie from request headers.
pub fn from_headers(headers: &HeaderMap) -> &'static Theme {
    let raw = headers
        .get(COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    for pair in raw.split(';') {
        if let Some(("theme", value)) = pair.trim().split_once('=') {
            return lookup(value);
        }
    }
    &THEMES[0]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_theme_falls_back_to_default() {
        assert_eq!(lookup("terminal").name, "terminal");
        assert_eq!(lookup("bogus").name, "midnight");
        assert_eq!(lookup("").name, "midnight");
    }

    #[test]
    fn parses_theme_cookie_from_header() {
        let mut headers = HeaderMap::new();
        headers.insert(COOKIE, "foo=1; theme=ember; bar=2".parse().unwrap());
        assert_eq!(from_headers(&headers).name, "ember");
        assert_eq!(from_headers(&HeaderMap::new()).name, "midnight");
    }
}
