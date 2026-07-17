//! Application error type that renders as a small HTML error fragment.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// A user-facing control error. Rendered as a 422 with a tiny HTML fragment so
/// htmx (which only swaps on 2xx) can surface it via the `htmx:responseError`
/// event, and curl clients see the message.
#[derive(Debug)]
pub struct AppError(pub String);

impl AppError {
    pub fn msg(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AppError {}

impl From<String> for AppError {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = format!("<div class=\"err-line\">{}</div>", html_escape(&self.0));
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
            body,
        )
            .into_response()
    }
}

/// Escape `&`, `<`, `>`, `'`, `"` for safe HTML interpolation.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&#39;"),
            '"' => out.push_str("&#34;"),
            other => out.push(other),
        }
    }
    out
}
