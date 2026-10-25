//! Page, action, SSE, health, and asset handlers.

pub(crate) mod actions;
pub(crate) mod bus;
pub(crate) mod config;
pub(crate) mod dashboard;
pub(crate) mod decisions;
pub(crate) mod devices;

use axum::{
    Json,
    extract::{Path, State},
    http::header,
    response::{IntoResponse, Response},
};

use super::{HTMX_JS, HTMX_SSE_JS, STYLESHEET, WebError};
use crate::state::{AppState, Health};

pub(crate) async fn healthz(State(state): State<AppState>) -> Json<Health> {
    Json(state.health())
}

/// Assets are compiled in so the static binary needs no files beside it.
pub(crate) async fn asset(Path(file): Path<String>) -> Result<Response, WebError> {
    const IMMUTABLE: &str = "public, max-age=31536000, immutable";
    let (body, content_type, cache): (&'static str, _, _) = match file.as_str() {
        HTMX_JS => (
            include_str!("../static/htmx-2.0.11.min.js"),
            "text/javascript; charset=utf-8",
            IMMUTABLE,
        ),
        HTMX_SSE_JS => (
            include_str!("../static/htmx-ext-sse-2.2.4.min.js"),
            "text/javascript; charset=utf-8",
            IMMUTABLE,
        ),
        STYLESHEET => (
            include_str!("../static/choregos.css"),
            "text/css; charset=utf-8",
            "no-cache",
        ),
        _ => return Err(WebError::NotFound),
    };
    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, cache),
        ],
        body,
    )
        .into_response())
}
