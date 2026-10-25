//! Handler tests against a real `AppState` built from a small config; no network except an
//! in-process fake identity provider on localhost.

use axum::{
    Json, Router,
    body::Body,
    http::{Request, StatusCode, header},
    response::Response,
    routing::get,
};
use futures_util::StreamExt;
use tower::ServiceExt;

use super::router;
use crate::{
    config::Config,
    state::{AppState, DecisionView, FrameDirection, FrameView, RolePower, UiEvent},
};

const PASSWORD: &str = "test-only-password";
const UNREACHABLE_ISSUER: &str = "http://127.0.0.1:9/";
const PUBLIC_URL: &str = "https://choregos.example.com/";

fn config_text(issuer: &str, public_url: &str) -> String {
    format!(
        r#"
mode = "shadow"

[mqtt]
host = "mqtt.test"
port = 1883
username = "choregos"
client_id = "choregos-test"

[bridge]
topic = "cec_bridge"
logical_address = 0

[home_assistant]
url = "http://ha.test:8123"

[tv]
entity = "media_player.test_tv"
wake = {{ kind = "wol", mac = "02:00:00:00:00:01", broadcast = "192.0.2.255", port = 9 }}

[receiver]
entity = "media_player.test_receiver"
select_source = true

[presets]
timezone = "UTC"
[[presets.slot]]
from = "00:00"
to = "24:00"
script = "script.test_preset"

[sonos]
entity = "media_player.test_speakers"
mute_switch = "switch.test_mute"
volume_from = []

[timing]
off_delay_s = 3
takeover_settle_ms = 2000
presence_poll_s = 5
presence_misses = 2
identify_retry_s = 10

[fake_tv]
power_status = "always_on"
cec_version = "1.4"
osd_name = "Choregos"

[[device]]
role = "streamer"
name = "Test Streamer"
kind = "playback"
match = {{ osd = "Streamer" }}
source = "Streamer Input"
on = ["active_source"]
off = ["standby"]
takeover = "cec_standby"
presence = "events"

[[device]]
role = "console"
name = "Test Console"
kind = "playback"
match = {{ physical = "2.3.0.0" }}
source = "Console Input"
on = ["active_source"]
off = ["standby"]
takeover = "none"
presence = "poll"

[web]
bind = "127.0.0.1:0"
public_url = "{public_url}"

[web.oidc]
issuer = "{issuer}"
client_id = "choregos-test"

[web.local_login]
username = "admin"
"#
    )
}

struct Harness {
    app: Router,
    state: AppState,
    dir: tempfile::TempDir,
}

fn harness(issuer: &str) -> Harness {
    harness_at(issuer, PUBLIC_URL)
}

fn harness_at(issuer: &str, public_url: &str) -> Harness {
    let mut config = Config::parse(&config_text(issuer, public_url)).expect("test config is valid");
    config
        .resolve_secrets(|name| {
            Some(match name {
                "CHOREGOS_LOCAL_PASSWORD" => PASSWORD.to_owned(),
                _ => format!("test-only-{name}-0123456789abcdef"),
            })
        })
        .expect("test secrets resolve");
    let dir = tempfile::tempdir().expect("temp dir");
    let state = AppState::new(config, dir.path().join("choregos.toml")).expect("state");
    Harness {
        app: router(state.clone()),
        state,
        dir,
    }
}

impl Harness {
    async fn send(&self, request: Request<Body>) -> Response {
        self.app.clone().oneshot(request).await.expect("infallible")
    }

    async fn get(&self, path: &str, cookie: Option<&str>) -> Response {
        let mut request = Request::get(path);
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        self.send(request.body(Body::empty()).unwrap()).await
    }

    async fn post_form(
        &self,
        path: &str,
        cookie: &str,
        csrf_header: Option<&str>,
        form: &str,
    ) -> Response {
        let mut request = Request::post(path)
            .header(header::COOKIE, cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(token) = csrf_header {
            request = request.header("x-csrf-token", token);
        }
        self.send(request.body(Body::from(form.to_owned())).unwrap())
            .await
    }

    /// Returns the session cookie and CSRF token of a freshly logged-in local user.
    async fn login(&self) -> (String, String) {
        let response = self.local_login("admin", PASSWORD).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(location(&response), "/");
        let cookie = session_cookie(&response).expect("login sets a fresh session cookie");
        let page = self.get("/", Some(&cookie)).await;
        assert_eq!(page.status(), StatusCode::OK);
        let html = body(page).await;
        let csrf = between(&html, "\"X-CSRF-Token\": \"", "\"").to_owned();
        (cookie, csrf)
    }

    async fn local_login(&self, username: &str, password: &str) -> Response {
        let form = self.get("/login/local", None).await;
        assert_eq!(form.status(), StatusCode::OK);
        let cookie = session_cookie(&form).expect("login form starts a session");
        let csrf = between(&body(form).await, "name=\"_csrf\" value=\"", "\"").to_owned();
        self.post_form(
            "/login/local",
            &cookie,
            None,
            &format!("_csrf={csrf}&username={username}&password={password}"),
        )
        .await
    }
}

fn location(response: &Response) -> &str {
    response
        .headers()
        .get(header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

fn session_cookie(response: &Response) -> Option<String> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with("choregos_session="))
        .and_then(|value| value.split(';').next())
        .map(str::to_owned)
}

async fn body(response: Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    String::from_utf8(bytes.to_vec()).expect("utf-8 body")
}

fn between<'a>(text: &'a str, start: &str, end: &str) -> &'a str {
    let from = text.find(start).expect("start marker") + start.len();
    let len = text[from..].find(end).expect("end marker");
    &text[from..from + len]
}

#[tokio::test]
async fn unauthenticated_requests_are_sent_to_login() {
    let h = harness(UNREACHABLE_ISSUER);
    for path in [
        "/",
        "/bus",
        "/bus/events",
        "/devices",
        "/decisions",
        "/config",
    ] {
        let response = h.get(path, None).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "{path}");
        assert_eq!(location(&response), "/login", "{path}");
    }
    let htmx = h
        .send(
            Request::get("/")
                .header("hx-request", "true")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(htmx.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(htmx.headers()["hx-redirect"], "/login");
    assert_eq!(h.get("/login", None).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn healthz_is_public_json() {
    let h = harness(UNREACHABLE_ISSUER);
    let response = h.get("/healthz", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    let health: serde_json::Value = serde_json::from_str(&body(response).await).unwrap();
    assert_eq!(health["mode"], "shadow");
    assert_eq!(health["mqtt_connected"], false);
    assert_eq!(health["bridge_online"], false);
}

#[tokio::test]
async fn local_login_opens_a_fresh_session_showing_the_dashboard() {
    let h = harness(UNREACHABLE_ISSUER);
    let form = h.get("/login/local", None).await;
    let anonymous = session_cookie(&form).unwrap();
    let (cookie, _) = h.login().await;
    assert_ne!(cookie, anonymous, "session id is cycled on login");
    let html = body(h.get("/", Some(&cookie)).await).await;
    for expected in [
        "Test Streamer",
        "Test Console",
        "Streamer Input",
        "media_player.test_tv",
        "mode-shadow",
        "admin",
    ] {
        assert!(html.contains(expected), "dashboard lacks {expected}");
    }
    // The login form cookie never becomes authenticated.
    assert_eq!(
        h.get("/", Some(&anonymous)).await.status(),
        StatusCode::SEE_OTHER
    );
    // Authenticated users skip the chooser.
    assert_eq!(location(&h.get("/login", Some(&cookie)).await), "/");
}

#[tokio::test]
async fn wrong_credentials_are_rejected_without_a_session() {
    let h = harness(UNREACHABLE_ISSUER);
    for (username, password) in [("admin", "wrong-password"), ("root", PASSWORD)] {
        let response = h.local_login(username, password).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let cookie = session_cookie(&response);
        let html = body(response).await;
        assert!(html.contains("Invalid username or password."));
        assert!(!html.contains(PASSWORD));
        if let Some(cookie) = cookie {
            assert_eq!(
                h.get("/", Some(&cookie)).await.status(),
                StatusCode::SEE_OTHER
            );
        }
    }
}

#[tokio::test]
async fn posts_require_the_session_csrf_token() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, csrf) = h.login().await;
    let missing = h.post_form("/actions/all-off", &cookie, None, "").await;
    assert_eq!(missing.status(), StatusCode::FORBIDDEN);
    let wrong = h
        .post_form("/actions/all-off", &cookie, Some("0000"), "")
        .await;
    assert_eq!(wrong.status(), StatusCode::FORBIDDEN);
    let wrong_field = h
        .post_form("/actions/all-off", &cookie, None, "_csrf=0000")
        .await;
    assert_eq!(wrong_field.status(), StatusCode::FORBIDDEN);
    let accepted = h
        .post_form("/actions/all-off", &cookie, Some(&csrf), "")
        .await;
    assert_ne!(accepted.status(), StatusCode::FORBIDDEN);
    // Login itself is protected against cross-site form posts.
    let form = h.get("/login/local", None).await;
    let anonymous = session_cookie(&form).unwrap();
    let forged = h
        .post_form(
            "/login/local",
            &anonymous,
            None,
            &format!("username=admin&password={PASSWORD}"),
        )
        .await;
    assert_eq!(forged.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn logout_ends_the_session() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, csrf) = h.login().await;
    let response = h
        .post_form("/logout", &cookie, None, &format!("_csrf={csrf}"))
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/login");
    assert_eq!(
        h.get("/", Some(&cookie)).await.status(),
        StatusCode::SEE_OTHER
    );
}

#[tokio::test]
async fn invalid_config_is_reported_inline_and_not_saved() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, csrf) = h.login().await;
    let page = body(h.get("/config", Some(&cookie)).await).await;
    assert!(page.contains("[REDACTED]"));
    assert!(!page.contains(PASSWORD));

    let response = h
        .post_form(
            "/config",
            &cookie,
            Some(&csrf),
            "action=save&config=mode+%3D+%5B%22%3Cb%3E",
        )
        .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let html = body(response).await;
    assert!(html.contains("invalid TOML configuration"));
    assert!(html.contains("mode = [&#34;&#60;b&#62;") || html.contains("mode = [&quot;&lt;b&gt;"));
    assert!(!h.dir.path().join("choregos.toml").exists());

    let valid = config_text(UNREACHABLE_ISSUER, PUBLIC_URL);
    let encoded: String = openidconnect::url::form_urlencoded::Serializer::new(String::new())
        .append_pair("action", "validate")
        .append_pair("config", &valid)
        .finish();
    let response = h.post_form("/config", &cookie, Some(&csrf), &encoded).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        body(response)
            .await
            .contains("Configuration is valid (not saved).")
    );
    assert!(!h.dir.path().join("choregos.toml").exists());
}

#[tokio::test]
async fn action_inputs_are_validated_before_reaching_the_runtime() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, csrf) = h.login().await;
    let unknown = h
        .post_form("/actions/takeover/nope", &cookie, Some(&csrf), "")
        .await;
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    let bad_la = h
        .post_form("/actions/identify/15", &cookie, Some(&csrf), "")
        .await;
    assert_eq!(bad_la.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let bad_raw = h
        .post_form("/actions/send-raw", &cookie, Some(&csrf), "to=4&data=xyz")
        .await;
    assert_eq!(bad_raw.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body(bad_raw).await.contains("hex bytes"));
}

#[tokio::test]
async fn pages_render_and_reference_served_assets() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    for path in ["/bus?la=4&opcode=82", "/devices", "/decisions", "/config"] {
        let response = h.get(path, Some(&cookie)).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
    }
    assert_eq!(
        h.get("/bus?la=99", Some(&cookie)).await.status(),
        StatusCode::BAD_REQUEST
    );
    let html = body(h.get("/", Some(&cookie)).await).await;
    let assets: Vec<&str> = html
        .match_indices("/static/")
        .map(|(start, _)| between(&html[start..], "", "\""))
        .collect();
    assert_eq!(assets.len(), 3);
    for asset in assets {
        assert_eq!(h.get(asset, None).await.status(), StatusCode::OK, "{asset}");
    }
    assert_eq!(
        h.get("/static/missing.js", None).await.status(),
        StatusCode::NOT_FOUND
    );
}

/// Serves only discovery + an empty JWKS: enough to exercise the redirect leg of the flow.
async fn fake_identity_provider() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/", listener.local_addr().unwrap());
    let issuer = base.clone();
    let discovery = Json(serde_json::json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}authorize"),
        "token_endpoint": format!("{issuer}token"),
        "jwks_uri": format!("{issuer}jwks"),
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["RS256"],
    }));
    let app = Router::new()
        .route(
            "/.well-known/openid-configuration",
            get(move || async move { discovery }),
        )
        .route(
            "/jwks",
            get(|| async { Json(serde_json::json!({ "keys": [] })) }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await });
    base
}

#[tokio::test]
async fn oidc_login_redirects_with_pkce_and_rejects_forged_callbacks() {
    let issuer = fake_identity_provider().await;
    let h = harness(&issuer);
    let start = h.get("/login/oidc", None).await;
    assert_eq!(start.status(), StatusCode::SEE_OTHER);
    let cookie = session_cookie(&start).expect("pending login is stored in the session");
    let target = openidconnect::url::Url::parse(location(&start)).unwrap();
    assert_eq!(
        target.as_str().split('?').next(),
        Some(&*format!("{issuer}authorize"))
    );
    let query: std::collections::HashMap<_, _> = target.query_pairs().into_owned().collect();
    assert_eq!(query["response_type"], "code");
    assert_eq!(query["client_id"], "choregos-test");
    assert_eq!(
        query["redirect_uri"],
        "https://choregos.example.com/auth/callback"
    );
    assert_eq!(query["code_challenge_method"], "S256");
    assert!(query["scope"].contains("openid"));
    assert!(!query["state"].is_empty() && !query["nonce"].is_empty());

    // Uncorrelated callbacks (forged state, or a bare provider error) are rejected without
    // touching the pending login.
    for forged in [
        "/auth/callback?code=abc&state=forged",
        "/auth/callback?error=access_denied",
        "/auth/callback?error=access_denied&state=forged",
    ] {
        let response = h.get(forged, Some(&cookie)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{forged}");
        let html = body(response).await;
        assert!(html.contains("state parameter does not match"), "{forged}");
        assert!(!html.contains("access_denied"), "{forged}");
    }
    // The genuine callback still finds its transaction; the exchange then fails only
    // because the fake provider has no token endpoint.
    let genuine = format!("/auth/callback?code=abc&state={}", query["state"]);
    let response = h.get(&genuine, Some(&cookie)).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(body(response).await.contains("token exchange failed"));
    // Consumed: a replay finds nothing pending.
    let replay = h.get(&genuine, Some(&cookie)).await;
    assert!(body(replay).await.contains("no login is pending"));

    // A correlated provider error is reported.
    let restart = h.get("/login/oidc", Some(&cookie)).await;
    let target = openidconnect::url::Url::parse(location(&restart)).unwrap();
    let state = target
        .query_pairs()
        .find(|(name, _)| name == "state")
        .map(|(_, value)| value.into_owned())
        .unwrap();
    let denied = h
        .get(
            &format!("/auth/callback?error=access_denied&state={state}"),
            Some(&cookie),
        )
        .await;
    assert!(body(denied).await.contains("access_denied"));
    assert_eq!(
        h.get("/", Some(&cookie)).await.status(),
        StatusCode::SEE_OTHER
    );
}

#[tokio::test]
async fn unreachable_identity_provider_is_reported_on_the_login_page() {
    let h = harness(UNREACHABLE_ISSUER);
    let response = h.get("/login/oidc", None).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let html = body(response).await;
    assert!(html.contains("Single sign-on failed"));
    assert!(html.contains("/login/local"));
}

fn frame(from: u8, to: u8, opcode: u8, opcode_name: &str) -> FrameView {
    FrameView {
        timestamp: chrono::Utc::now(),
        direction: FrameDirection::Received,
        from,
        to,
        from_name: format!("Device {from}"),
        to_name: format!("Device {to}"),
        opcode: Some(opcode),
        opcode_name: opcode_name.to_owned(),
        params: vec![0x21, 0x00],
        data: format!("{opcode:02X}2100"),
    }
}

#[tokio::test]
async fn bus_stream_resyncs_from_history_then_streams_filtered_rows() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let mut snapshot = h.state.snapshot();
    let history = frame(4, 15, 0x82, "<Active Source>");
    snapshot.recent_frames.extend([history.clone()]);
    h.state.publish(snapshot.clone(), []);
    let page = body(h.get("/bus", Some(&cookie)).await).await;
    assert!(page.contains("&#60;Active Source&#62;") || page.contains("&lt;Active Source&gt;"));
    assert!(page.contains("21 00"));
    assert!(page.contains("sse-connect=\"/bus/events\""));

    let response = h.get("/bus/events?la=4", Some(&cookie)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    let mut stream = response.into_body().into_data_stream();
    let decision = DecisionView {
        timestamp: chrono::Utc::now(),
        role: Some("streamer".to_owned()),
        from: Some(RolePower::Off),
        to: Some(RolePower::On),
        reason: "test_reason".to_owned(),
        actions: vec!["media_player.turn_on media_player.test_receiver".to_owned()],
        would: true,
    };
    h.state.publish(
        snapshot,
        [
            // Already in the history the stream resyncs from: must not appear twice.
            UiEvent::Frame(history),
            UiEvent::Frame(frame(1, 0, 0x36, "Standby")),
            UiEvent::Frame(frame(4, 0, 0x04, "Image View On")),
            UiEvent::Decision(decision),
        ],
    );
    let mut received = String::new();
    while !received.contains("test_reason") {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("SSE event arrives")
            .expect("stream open")
            .expect("chunk");
        received.push_str(std::str::from_utf8(&chunk).unwrap());
    }
    let reset = received
        .find("event: reset")
        .expect("stream starts with a resync");
    let first_row = received.find("event: row").expect("live rows follow");
    assert!(reset < first_row);
    assert_eq!(received.matches("Active Source").count(), 1);
    assert!(received.contains("Image View On"));
    assert!(
        !received.contains("Standby"),
        "LA filter applies to the stream"
    );
    assert!(received.contains("off → on"));
    assert!(received.contains("would: media_player.turn_on"));
}

#[tokio::test]
async fn dashboard_refresh_carries_header_status_and_stable_control_ids() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let mut snapshot = h.state.snapshot();
    snapshot.mode = crate::state::Mode::Active;
    snapshot.warming = false;
    snapshot.connections.mode = crate::state::Mode::Active;
    snapshot.connections.mqtt_connected = true;
    h.state.publish(snapshot, []);
    let html = body(h.get("/", Some(&cookie)).await).await;
    // The poll swaps `#dashboard` and, out of band, `#status` from the same response.
    assert!(html.contains(r##"hx-select-oob="#status""##));
    let status = between(&html, "id=\"status\"", "</div>");
    assert!(status.contains("mode-active"));
    assert!(!status.contains("warming"));
    assert!(status.contains("conn up\" title=\"MQTT connected"));
    for id in [
        "mode-shadow",
        "mode-active",
        "all-off",
        "takeover-streamer",
        "standby-console",
    ] {
        assert!(
            html.contains(&format!("id=\"{id}\"")),
            "{id} has a stable id"
        );
    }
}

#[tokio::test]
async fn role_path_segments_cannot_escape_their_action() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let mut snapshot = h.state.snapshot();
    snapshot.roles[0].role = "../all-off".to_owned();
    snapshot.roles[1].role = "a/b".to_owned();
    h.state.publish(snapshot, []);
    let html = body(h.get("/", Some(&cookie)).await).await;
    assert!(html.contains(r#"hx-post="/actions/takeover/..%2Fall-off""#));
    assert!(html.contains(r#"hx-post="/actions/standby/a%2Fb""#));
    assert!(!html.contains("/actions/takeover/../"));
    assert!(!html.contains("/actions/standby/a/b"));
}

#[tokio::test]
async fn secure_cookie_follows_the_normalised_public_url_scheme() {
    for (public_url, secure) in [
        ("https://choregos.example.com/", true),
        ("HTTPS://Choregos.Example.com", true),
        ("http://choregos.lan:8095", false),
    ] {
        let h = harness_at(UNREACHABLE_ISSUER, public_url);
        let form = h.get("/login/local", None).await;
        let cookie = form
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|value| value.to_str().ok())
            .unwrap()
            .to_owned();
        assert_eq!(
            cookie.contains("; Secure"),
            secure,
            "{public_url}: {cookie}"
        );
    }
}

#[tokio::test]
async fn local_login_attempts_are_throttled() {
    let h = harness(UNREACHABLE_ISSUER);
    for _ in 0..10 {
        let response = h.local_login("admin", "wrong-password").await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let throttled = h.local_login("admin", PASSWORD).await;
    assert_eq!(throttled.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(throttled.headers()[header::RETRY_AFTER], "60");
    assert!(body(throttled).await.contains("Too many login attempts"));
}

#[tokio::test]
async fn anonymous_session_flood_does_not_displace_a_logged_in_user() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    for _ in 0..1100 {
        assert_eq!(h.get("/login/local", None).await.status(), StatusCode::OK);
    }
    assert_eq!(h.get("/", Some(&cookie)).await.status(), StatusCode::OK);
}
