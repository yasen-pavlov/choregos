//! Authenticated web UI: axum router, sessions, CSRF, and htmx-driven pages.

mod auth;
mod routes;
mod session_store;
mod view;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use axum::{
    Router,
    extract::Request,
    http::{HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use tower_http::trace::TraceLayer;
use tower_sessions::{
    Expiry, SessionManagerLayer,
    cookie::{SameSite, time::Duration},
};

use self::session_store::BoundedStore;
use crate::state::AppState;

pub(crate) const HTMX_JS: &str = "htmx-2.0.11.min.js";
pub(crate) const HTMX_SSE_JS: &str = "htmx-ext-sse-2.2.4.min.js";
/// The one hand-written script, loaded on Recorder pages only; bump the suffix on change.
pub(crate) const RECORDER_JS: &str = "recorder-2.js";
pub(crate) const STYLESHEET: &str = "choregos.css";

const SESSION_COOKIE: &str = "choregos_session";
const SESSION_IDLE_DAYS: i64 = 7;
const ANONYMOUS_IDLE_MINUTES: i64 = 15;
/// Upper bound on stored sessions; anonymous ones are evicted first when it is reached.
const MAX_SESSIONS: usize = 1024;
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; style-src 'self' 'unsafe-inline'; \
     img-src 'self' data:; frame-ancestors 'none'; form-action 'self'; base-uri 'none'";

/// Shared handler state: the core-owned application state plus resolved login settings.
#[derive(Clone)]
pub(crate) struct WebState {
    pub(crate) app: AppState,
    pub(crate) auth: Arc<auth::Auth>,
}

pub fn router(state: AppState) -> Router {
    let auth = Arc::new(auth::Auth::new(state.auth()));
    // Saving on every request makes the expiry a true inactivity timeout and refreshes the
    // cookie; the store keeps anonymous sessions (login form, pending OIDC) much shorter.
    let store = BoundedStore::new(
        MAX_SESSIONS,
        auth::USER_KEY,
        Duration::minutes(ANONYMOUS_IDLE_MINUTES),
    );
    let sessions = SessionManagerLayer::new(store)
        .with_name(SESSION_COOKIE)
        .with_secure(auth.secure_cookies())
        .with_http_only(true)
        .with_same_site(SameSite::Lax)
        .with_always_save(true)
        .with_expiry(Expiry::OnInactivity(Duration::days(SESSION_IDLE_DAYS)))
        .with_signed(auth::session_key(state.auth().session_secret.expose()));
    let web = WebState {
        app: state.clone(),
        auth,
    };

    let protected = Router::new()
        .route("/", get(routes::cockpit::page))
        .route("/p/cockpit", get(routes::cockpit::partial))
        .route("/p/status", get(routes::chrome::status))
        .route("/p/bridge", get(routes::chrome::bridge))
        .route("/recorder", get(routes::recorder::page))
        .route("/p/recorder", get(routes::recorder::rows))
        .route("/recorder/glossary", get(routes::recorder::glossary))
        .route("/recorder/live", get(routes::live::page))
        .route("/recorder/live/events", get(routes::live::events))
        .route("/recorder/timeline", get(routes::timeline::page))
        .route("/p/timeline", get(routes::timeline::block))
        .route(
            "/p/timeline/window.json",
            get(routes::timeline::window_json),
        )
        .route("/p/timeline/select/{id}", get(routes::timeline::selected))
        .route("/recorder/{id}", get(routes::recorder::detail))
        .route("/p/window/{id}", get(routes::recorder::window))
        .route("/devices", get(routes::devices::page))
        .route("/p/devices", get(routes::devices::partial))
        .route("/devices/out/{name}", get(routes::devices::output))
        .route("/devices/la/{la}", get(routes::devices::entry))
        .route("/devices/{role}", get(routes::devices::role))
        .route("/config", get(routes::config::page))
        .route("/p/config/{section}", get(routes::config::partial))
        .route("/config/validate", post(routes::config::validate))
        .route("/config/preview", post(routes::config::preview))
        .route("/config/save", post(routes::config::save))
        .route("/actions/mode", post(routes::actions::mode))
        .route("/actions/all-off", post(routes::actions::all_off))
        .route("/actions/identify/{la}", post(routes::actions::identify))
        .route("/actions/forget/{la}", post(routes::actions::forget))
        .route("/actions/takeover/{role}", post(routes::actions::takeover))
        .route("/actions/standby/{role}", post(routes::actions::standby))
        .route("/actions/send-raw", post(routes::actions::send_raw))
        .route_layer(middleware::from_fn(auth::require_login));
    let public = Router::new()
        .route("/login", get(auth::login_page))
        .route(
            "/login/local",
            get(auth::local_form).post(auth::local_submit),
        )
        .route("/login/oidc", get(auth::oidc_start))
        .route("/auth/callback", get(auth::oidc_callback))
        .route("/logout", post(auth::logout));
    let session_routes = protected
        .merge(public)
        .layer(middleware::from_fn(auth::require_csrf))
        .layer(sessions)
        .with_state(web);

    Router::new()
        .route("/healthz", get(routes::healthz))
        .route("/static/{file}", get(routes::asset))
        .with_state(state)
        .merge(session_routes)
        .layer(middleware::from_fn(security_headers))
        .layer(
            // Path only: query strings carry OIDC authorization codes.
            TraceLayer::new_for_http().make_span_with(|request: &Request| {
                tracing::info_span!(
                    "http",
                    method = %request.method(),
                    path = %request.uri().path(),
                )
            }),
        )
}

async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    response
}

/// Handler failure. Server-side causes are logged; clients only see a generic message.
#[derive(Debug, thiserror::Error)]
pub(crate) enum WebError {
    #[error("template rendering failed: {0}")]
    Render(#[from] askama::Error),
    #[error("session store failed: {0}")]
    Session(#[from] tower_sessions::session::Error),
    #[error("not authenticated")]
    Unauthenticated,
    #[error("{0}")]
    BadRequest(String),
    #[error("not found")]
    NotFound,
    /// The configuration file could not be read through the runtime.
    #[error("configuration file unavailable: {0}")]
    ConfigFile(#[from] crate::config::ConfigError),
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::ConfigFile(crate::config::ConfigError::Unavailable) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            Self::Render(_) | Self::Session(_) | Self::ConfigFile(_) => {
                tracing::error!(error = %self, "web request failed");
                return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
            }
            Self::Unauthenticated => StatusCode::UNAUTHORIZED,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::NotFound => StatusCode::NOT_FOUND,
        };
        (status, self.to_string()).into_response()
    }
}

pub(crate) fn render(template: &impl askama::Template) -> Result<Html<String>, WebError> {
    Ok(Html(template.render()?))
}
