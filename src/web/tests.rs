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
    action::DriftDetail,
    config::Config,
    state::{
        AppState, BridgeTelemetry, BusEntryView, BusPower, Command, CommandError, DecisionView,
        FrameDirection, FrameView, RolePower, Snapshot, UiEvent,
    },
};

const PASSWORD: &str = "test-only-password";
const UNREACHABLE_ISSUER: &str = "http://127.0.0.1:9/";
const PUBLIC_URL: &str = "https://choregos.example.com/";

const TASMOTA_BRIDGE: &str = "kind = \"tasmota\"\ntopic = \"cec_bridge\"";
const AETHER_BRIDGE: &str = "kind = \"aether\"\nnode = \"cec-bridge\"";

fn config_text(issuer: &str, public_url: &str) -> String {
    bridge_config_text(issuer, public_url, TASMOTA_BRIDGE)
}

fn bridge_config_text(issuer: &str, public_url: &str, bridge: &str) -> String {
    format!(
        r#"
mode = "shadow"

[mqtt]
host = "mqtt.test"
port = 1883
username = "choregos"
client_id = "choregos-test"

[bridge]
{bridge}
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
media_player = "media_player.test_streamer"

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
}

fn harness(issuer: &str) -> Harness {
    harness_at(issuer, PUBLIC_URL)
}

fn harness_at(issuer: &str, public_url: &str) -> Harness {
    harness_from(config_text(issuer, public_url))
}

fn harness_from(text: String) -> Harness {
    let mut config = Config::parse(&text).expect("test config is valid");
    config
        .resolve_secrets(|name| {
            Some(match name {
                "CHOREGOS_LOCAL_PASSWORD" => PASSWORD.to_owned(),
                _ => format!("test-only-{name}-0123456789abcdef"),
            })
        })
        .expect("test secrets resolve");
    let state = AppState::new(config, text);
    Harness {
        app: router(state.clone()),
        state,
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
        "/p/cockpit",
        "/p/status",
        "/p/bridge",
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
async fn local_login_opens_a_fresh_session_showing_the_cockpit() {
    let h = harness(UNREACHABLE_ISSUER);
    let form = h.get("/login/local", None).await;
    let anonymous = session_cookie(&form).unwrap();
    let (cookie, _) = h.login().await;
    assert_ne!(cookie, anonymous, "session id is cycled on login");
    let html = body(h.get("/", Some(&cookie)).await).await;
    for expected in ["Test Streamer", "Test Console", "mode-shadow", "admin"] {
        assert!(html.contains(expected), "cockpit lacks {expected}");
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
    let saved = h.state.config_text();
    let page = body(h.get("/config", Some(&cookie)).await).await;
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
    assert_eq!(h.state.config_text(), saved);

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
    assert_eq!(h.state.config_text(), saved);
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
    let bad_forget = h
        .post_form("/actions/forget/15", &cookie, Some(&csrf), "")
        .await;
    assert_eq!(bad_forget.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let bad_raw = h
        .post_form("/actions/send-raw", &cookie, Some(&csrf), "to=4&data=xyz")
        .await;
    assert_eq!(bad_raw.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body(bad_raw).await.contains("hex bytes"));
}

/// Answers exactly one runtime command with `result` and hands the command back to the test.
fn answer_one_command(
    state: &AppState,
    result: Result<(), CommandError>,
) -> tokio::sync::oneshot::Receiver<Command> {
    let (sender, mut inputs) = tokio::sync::mpsc::channel(1);
    state.attach(sender);
    let (forward, received) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        if let Some(crate::runtime::Input::Request(crate::runtime::Request::Command(
            command,
            Some(reply),
        ))) = inputs.recv().await
        {
            let _ = reply.send(result);
            let _ = forward.send(command);
        }
    });
    received
}

#[tokio::test]
async fn raw_send_length_follows_the_configured_bridge() {
    for (bridge, limit) in [(TASMOTA_BRIDGE, 14), (AETHER_BRIDGE, 15)] {
        let h = harness_from(bridge_config_text(UNREACHABLE_ISSUER, PUBLIC_URL, bridge));
        let (cookie, csrf) = h.login().await;
        let too_long = h
            .post_form(
                "/actions/send-raw",
                &cookie,
                Some(&csrf),
                &format!("to=4&data={}", "00".repeat(limit + 1)),
            )
            .await;
        assert_eq!(too_long.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let text = body(too_long).await;
        assert!(
            text.contains(&format!("Data must be 1–{limit} bytes.")),
            "{bridge}: {text}"
        );

        let received = answer_one_command(&h.state, Ok(()));
        let longest = h
            .post_form(
                "/actions/send-raw",
                &cookie,
                Some(&csrf),
                &format!("to=4&data={}", "00".repeat(limit)),
            )
            .await;
        assert_eq!(longest.status(), StatusCode::OK, "{bridge}");
        let command = received.await.expect("runtime received the command");
        assert!(
            matches!(&command, Command::SendRaw { to: 4, data } if data.len() == limit),
            "{bridge}: {command:?}"
        );
    }
}

#[tokio::test]
async fn forget_forwards_the_logical_address_and_reloads_only_on_success() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, csrf) = h.login().await;
    let received = answer_one_command(&h.state, Ok(()));
    let response = h
        .post_form("/actions/forget/3", &cookie, Some(&csrf), "")
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["hx-refresh"], "true");
    assert!(body(response).await.contains("Forgot LA 3."));
    let command = received.await.expect("runtime received the command");
    assert!(matches!(command, Command::Forget(3)), "{command:?}");

    // A second click on the now stale row: the runtime rejects it and the flash stays.
    let _received = answer_one_command(
        &h.state,
        Err(CommandError::Invalid("unknown logical address".to_owned())),
    );
    let stale = h
        .post_form("/actions/forget/3", &cookie, Some(&csrf), "")
        .await;
    assert_eq!(stale.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!stale.headers().contains_key("hx-refresh"));
    assert!(body(stale).await.contains("unknown logical address"));

    let out_of_range = h
        .post_form("/actions/forget/15", &cookie, Some(&csrf), "")
        .await;
    assert_eq!(out_of_range.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!out_of_range.headers().contains_key("hx-refresh"));
}

fn bus_entry(la: u8) -> BusEntryView {
    BusEntryView {
        logical_address: la,
        physical_address: Some("2.5.0.0".to_owned()),
        device_type: Some(3),
        vendor: Some("000000".to_owned()),
        osd: None,
        power: BusPower::Unknown,
        last_seen: None,
        identified: false,
        verified: false,
        matched_role: None,
    }
}

#[tokio::test]
async fn devices_rows_offer_forget_except_for_the_bridge() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let mut snapshot = h.state.snapshot();
    let bridge = snapshot.bridge_logical_address;
    snapshot.bus = vec![bus_entry(bridge), bus_entry(3)];
    h.state.publish(snapshot, []);
    let html = body(h.get("/devices", Some(&cookie)).await).await;
    let row = between(&html, "<td class=\"mono\">3</td>", "</tr>");
    assert!(row.contains(r#"hx-post="/actions/forget/3""#));
    assert!(row.contains("hx-confirm=\"Forget LA 3? It will be re-learned if it talks again.\""));
    assert!(row.contains(r#"hx-post="/actions/identify/3""#));
    // Both buttons share the last cell: the row keeps the header's nine columns.
    assert_eq!(row.matches("<td").count(), 8, "plus the LA cell = 9");
    assert!(!html.contains(&format!("/actions/forget/{bridge}\"")));
    assert!(html.contains(&format!("/actions/identify/{bridge}\"")));
}

#[tokio::test]
async fn pages_render_and_reference_served_assets() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    for path in [
        "/bus?la=4&opcode=82",
        "/devices",
        "/decisions",
        "/config",
        "/p/cockpit",
        "/p/status",
        "/p/bridge",
    ] {
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
        ack: None,
    }
}

/// The `<tr>…</tr>` of the row that renders `text`.
fn row_containing<'a>(html: &'a str, text: &str) -> &'a str {
    let at = html
        .find(text)
        .unwrap_or_else(|| panic!("{text:?} is rendered"));
    let open = html[..at].rfind("<tr").expect("text sits inside a row");
    let close = at + html[at..].find("</tr>").expect("row is closed");
    &html[open..close]
}

#[tokio::test]
async fn bus_rows_mark_local_replies_and_nacked_frames() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    // A local reply that failed without a NACK (arbitration lost, line error, busy): the
    // adapter reports no ack outcome, so the row must not claim a NACK.
    let local = FrameView {
        direction: FrameDirection::Local,
        ack: None,
        ..frame(0, 15, 0x84, "Report Physical Address")
    };
    let nacked = FrameView {
        direction: FrameDirection::Sent,
        ack: Some(false),
        ..frame(0, 8, 0x8F, "Give Device Power Status")
    };
    let acked = FrameView {
        ack: Some(true),
        ..frame(4, 0, 0x90, "Report Power Status")
    };
    let mut snapshot = h.state.snapshot();
    snapshot
        .recent_frames
        .extend([local, nacked, acked, frame(4, 15, 0x82, "Active Source")]);
    h.state.publish(snapshot.clone(), []);
    let page = body(h.get("/bus", Some(&cookie)).await).await;

    let local_row = row_containing(&page, "Report Physical Address");
    assert!(
        local_row.starts_with("<tr class=\"frame local\""),
        "{local_row}"
    );
    assert!(local_row.contains(">local</td>"), "{local_row}");
    assert!(!local_row.contains("nack"), "{local_row}");

    let nacked_row = row_containing(&page, "Give Device Power Status");
    assert!(
        nacked_row.starts_with("<tr class=\"frame tx\""),
        "{nacked_row}"
    );
    assert!(
        classes_around(nacked_row, "nack</span>").contains(&"nack"),
        "{nacked_row}"
    );
    for unmarked in ["Report Power Status", "Active Source"] {
        let row = row_containing(&page, unmarked);
        assert!(row.starts_with("<tr class=\"frame rx\""), "{row}");
        assert!(!row.contains("nack"), "{row}");
    }

    // Live rows come from the same partial: a local reply the destination NACKed.
    let response = h.get("/bus/events", Some(&cookie)).await;
    let mut stream = response.into_body().into_data_stream();
    h.state.publish(
        snapshot,
        [UiEvent::Frame(FrameView {
            timestamp: chrono::Utc::now() + chrono::TimeDelta::seconds(1),
            direction: FrameDirection::Local,
            ack: Some(false),
            ..frame(0, 4, 0x47, "Set OSD Name")
        })],
    );
    let mut received = String::new();
    while !received.contains("event: row") || !received.contains("Set OSD Name") {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("SSE event arrives")
            .expect("stream open")
            .expect("chunk");
        received.push_str(std::str::from_utf8(&chunk).unwrap());
    }
    let live = &received[received.find("event: row").expect("live row")..];
    let live_row = row_containing(live, "Set OSD Name");
    assert!(
        live_row.starts_with("<tr class=\"frame local\""),
        "{live_row}"
    );
    assert!(live_row.contains("nack</span>"), "{live_row}");
}

/// A shadow-mode decision as the runtime records it: the suppressed action carries the
/// runtime's `would: ` prefix, the identification query (never suppressed) carries none.
fn shadow_decision() -> DecisionView {
    DecisionView {
        id: 7,
        trigger: crate::action::Trigger::Command {
            command: crate::state::Command::Takeover("streamer".to_owned()),
        },
        parent: None,
        timestamp: chrono::Utc::now(),
        role: Some("streamer".to_owned()),
        from: Some(RolePower::Off),
        to: Some(RolePower::On),
        reason: "test_reason".to_owned(),
        actions: vec![
            "would: media_player.turn_on media_player.test_receiver".to_owned(),
            "cec.give_osd_name 4".to_owned(),
        ],
        would: true,
        expectations: vec![],
        verdict: crate::action::Verdict::None,
        drift: None,
    }
}

fn verdict_decision() -> DecisionView {
    use crate::action::{Expectation, Field, Outcome, Verdict, Via};
    DecisionView {
        id: 9,
        would: false,
        reason: "PowerOn".to_owned(),
        actions: vec![],
        verdict: Verdict::Partial,
        expectations: vec![
            Expectation {
                subject: "tv".to_owned(),
                field: Field::Power,
                expect: "on".to_owned(),
                via: vec![Via::TvDirect, Via::Ha],
                deadline_ms: 15_000,
                attempts: 0,
                outcome: Outcome::Met {
                    via: Via::TvDirect,
                    observed: "on".to_owned(),
                    latency_ms: 3_400,
                },
            },
            Expectation {
                subject: "role:console_b".to_owned(),
                field: Field::Power,
                expect: "standby".to_owned(),
                via: vec![Via::Cec],
                deadline_ms: 6_000,
                attempts: 2,
                outcome: Outcome::Missed {
                    observed: "on".to_owned(),
                },
            },
        ],
        ..shadow_decision()
    }
}

#[tokio::test]
async fn decisions_page_renders_verdict_chip_and_expectations() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let mut snapshot = h.state.snapshot();
    snapshot
        .decisions
        .extend([shadow_decision(), verdict_decision()]);
    h.state.publish(snapshot, []);
    let page = body(h.get("/decisions", Some(&cookie)).await).await;
    assert_eq!(
        page.matches("class=\"pill verdict-").count(),
        1,
        "the `none` verdict renders no chip"
    );
    assert!(page.contains("pill verdict-partial\">partial<"));
    assert!(page.contains("tv.power"));
    assert!(page.contains("via tv_direct in 3.4 s"));
    assert!(page.contains("standby → on"));
    assert!(page.contains("2 retries"));
}

/// Class tokens of the element whose text starts at the first occurrence of `text`.
fn classes_around<'a>(html: &'a str, text: &str) -> Vec<&'a str> {
    let at = html
        .find(text)
        .unwrap_or_else(|| panic!("{text:?} is rendered"));
    let open = html[..at].rfind('<').expect("text sits inside an element");
    html[open..at]
        .split_once("class=\"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .map_or_else(Vec::new, |(classes, _)| {
            classes.split_whitespace().collect()
        })
}

/// The suppressed action reads `would: ` exactly once and is marked `would`; the identify
/// action, which runs even in shadow mode, has neither.
fn assert_shadow_actions(html: &str) {
    let suppressed = "would: media_player.turn_on media_player.test_receiver";
    let identify = "cec.give_osd_name 4";
    assert!(html.contains(suppressed));
    assert!(!html.contains("would: would:"));
    assert!(classes_around(html, suppressed).contains(&"would"));
    let before_identify = &html[..html.find(identify).expect("identify action is rendered")];
    assert!(!before_identify.ends_with("would: "));
    assert!(!classes_around(html, identify).contains(&"would"));
}

#[tokio::test]
async fn decisions_page_shows_the_runtime_would_prefix_once() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let mut snapshot = h.state.snapshot();
    snapshot.decisions.extend([shadow_decision()]);
    h.state.publish(snapshot, []);
    let page = body(h.get("/decisions", Some(&cookie)).await).await;
    assert_shadow_actions(&page);
}

/// A drift as the engine records it: Home Assistant reports the receiver `observed` while
/// the room intends it on.
fn drift_detail(observed: &str, since: chrono::DateTime<chrono::Utc>) -> DriftDetail {
    DriftDetail {
        subject: "receiver".to_owned(),
        field: crate::action::Field::Power,
        intended: "on".to_owned(),
        observed: observed.to_owned(),
        via: crate::action::Via::Ha,
        since,
    }
}

/// An actionless drift row: no role, transition, actions, expectations or verdict.
fn drift_decision(
    id: u64,
    reason: &str,
    at: chrono::TimeDelta,
    detail: DriftDetail,
) -> DecisionView {
    DecisionView {
        id,
        trigger: crate::action::Trigger::Observation {
            source: crate::action::Via::Ha,
        },
        timestamp: detail.since + at,
        role: None,
        from: None,
        to: None,
        reason: reason.to_owned(),
        actions: vec![],
        would: false,
        drift: Some(detail),
        ..shadow_decision()
    }
}

#[tokio::test]
async fn decisions_page_renders_drift_detail_and_resolved_duration_without_a_verdict() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let since = chrono::Utc::now() - chrono::TimeDelta::minutes(5);
    let mut snapshot = h.state.snapshot();
    snapshot.decisions.extend([
        verdict_decision(),
        drift_decision(
            11,
            "drift",
            chrono::TimeDelta::seconds(30),
            drift_detail("off", since),
        ),
        drift_decision(
            12,
            "drift_resolved",
            chrono::TimeDelta::milliseconds(95_500),
            drift_detail("on", since),
        ),
        // A detail on a row whose reason is not drift does not take over the row.
        DecisionView {
            id: 13,
            drift: Some(drift_detail("off", since)),
            ..verdict_decision()
        },
    ]);
    h.state.publish(snapshot, []);
    let page = body(h.get("/decisions", Some(&cookie)).await).await;

    let onset = row_containing(&page, "id=\"d-11\"");
    assert!(classes_around(onset, "drift</span>").contains(&"drift-open"));
    assert!(onset.contains("<span class=\"mono\">receiver.power</span> on → off"));
    assert!(onset.contains("via ha</span>"), "{onset}");
    assert!(!onset.contains("lasted"), "an open onset has no duration");
    assert!(!onset.contains("pill verdict-"), "{onset}");

    let resolved = row_containing(&page, "id=\"d-12\"");
    assert!(classes_around(resolved, "drift_resolved</span>").contains(&"drift-resolved"));
    assert!(resolved.contains("receiver.power</span> on → on"));
    assert!(
        resolved.contains("via ha · lasted 1m 35s</span>"),
        "{resolved}"
    );
    assert!(!resolved.contains("pill verdict-"), "{resolved}");

    // Ordinary rows keep their plain reason and verdict chip.
    let ordinary = row_containing(&page, "id=\"d-9\"");
    assert!(ordinary.contains("<td>PowerOn</td>"), "{ordinary}");
    assert!(ordinary.contains("pill verdict-partial"));
    let mismatched = row_containing(&page, "id=\"d-13\"");
    assert!(mismatched.contains("<td>PowerOn</td>"), "{mismatched}");
    assert!(mismatched.contains("pill verdict-partial"), "{mismatched}");
    assert!(!mismatched.contains("drift"), "{mismatched}");
}

/// Publishes the current snapshot after `edit`; returns the polled top-bar status block.
async fn status_after(h: &Harness, cookie: &str, edit: impl FnOnce(&mut Snapshot)) -> String {
    let mut snapshot = h.state.snapshot();
    edit(&mut snapshot);
    h.state.publish(snapshot, []);
    body(h.get("/p/status", Some(cookie)).await).await
}

/// The consistency badge's classes, text and tooltip.
fn badge(status: &str) -> (&str, &str, &str) {
    let tag = &status[status
        .find("id=\"consistency\"")
        .expect("badge is rendered")..];
    (
        between(tag, "class=\"", "\""),
        between(tag, ">", "</span>"),
        between(tag, "title=\"", "\""),
    )
}

#[tokio::test]
async fn consistency_badge_follows_warming_shadow_drift_and_tv_reachability() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    // A fresh service is warming; warming outranks shadow.
    let status = status_after(&h, &cookie, |_| {}).await;
    assert_eq!(badge(&status).1, "warming");
    assert_eq!(badge(&status).0, "chip lg pend");

    let status = status_after(&h, &cookie, |snapshot| snapshot.warming = false).await;
    assert_eq!(badge(&status).1, "not checked (shadow)");
    assert_eq!(badge(&status).0, "chip lg shadow");

    let since = chrono::Utc::now() - chrono::TimeDelta::seconds(125);
    let status = status_after(&h, &cookie, |snapshot| {
        snapshot.mode = crate::state::Mode::Active;
        snapshot.intent.power = crate::state::RoomPower::On;
        snapshot.drift = vec![
            drift_detail("off", since),
            DriftDetail {
                subject: "tv".to_owned(),
                intended: "off".to_owned(),
                observed: "on".to_owned(),
                via: crate::action::Via::TvDirect,
                ..drift_detail("on", since)
            },
        ];
        // An open drift outranks an unreachable TV.
        snapshot.tv_direct = Some((crate::state::DirectPower::Unreachable, chrono::Utc::now()));
    })
    .await;
    let (classes, label, title) = badge(&status);
    assert_eq!((classes, label), ("chip lg warn", "drift: receiver.power"));
    assert!(
        title.contains("receiver.power: intended on, observed off via ha"),
        "{title}"
    );
    assert!(
        title.contains("tv.power: intended off, observed on via tv_direct"),
        "{title}"
    );

    let status = status_after(&h, &cookie, |snapshot| snapshot.drift.clear()).await;
    assert_eq!(badge(&status).1, "unknown: TV unreachable");

    let status = status_after(&h, &cookie, |snapshot| {
        snapshot.tv_direct = Some((crate::state::DirectPower::On, chrono::Utc::now()));
    })
    .await;
    assert_eq!(badge(&status).1, "consistent");
    assert_eq!(badge(&status).0, "chip lg ok");
    // The room strip carries the same verdict with its sentence.
    let cockpit = body(h.get("/p/cockpit", Some(&cookie)).await).await;
    assert!(cockpit.contains("TV and receiver agree with the intent."));
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
    h.state.publish(
        snapshot,
        [
            // Already in the history the stream resyncs from: must not appear twice.
            UiEvent::Frame(history),
            UiEvent::Frame(frame(1, 0, 0x36, "Standby")),
            UiEvent::Frame(frame(4, 0, 0x04, "Image View On")),
            UiEvent::Decision(shadow_decision()),
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
    assert_shadow_actions(&received);
}

#[tokio::test]
async fn bus_stream_re_renders_the_ring_when_a_verdict_settles() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let response = h.get("/bus/events", Some(&cookie)).await;
    let mut stream = response.into_body().into_data_stream();
    let mut received = String::new();
    macro_rules! pump {
        ($needle:expr) => {
            while !received.contains($needle) {
                let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
                    .await
                    .expect("SSE event arrives")
                    .expect("stream open")
                    .expect("chunk");
                received.push_str(std::str::from_utf8(&chunk).unwrap());
            }
        };
    }
    pump!("event: reset");

    let mut pending = verdict_decision();
    pending.verdict = crate::action::Verdict::Pending;
    let mut snapshot = h.state.snapshot();
    snapshot.decisions.push_back(pending.clone());
    h.state
        .publish(snapshot.clone(), [UiEvent::Decision(pending)]);
    pump!("verdict-pending");
    let before = received.len();

    let settled = verdict_decision();
    snapshot.decisions.back_mut().unwrap().verdict = settled.verdict;
    snapshot.decisions.back_mut().unwrap().expectations = settled.expectations.clone();
    h.state.publish(
        snapshot,
        [UiEvent::Verdict {
            id: settled.id,
            verdict: settled.verdict,
            expectations: settled.expectations,
        }],
    );
    pump!("verdict-partial");
    assert!(
        received[before..].contains("event: reset"),
        "a settled verdict re-renders the ring rather than streaming a duplicate row"
    );
}

#[tokio::test]
async fn cockpit_poll_carries_the_status_block_and_stable_control_ids() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let mut snapshot = h.state.snapshot();
    snapshot.mode = crate::state::Mode::Active;
    snapshot.warming = false;
    snapshot.connections.mode = crate::state::Mode::Active;
    snapshot.connections.mqtt_connected = true;
    snapshot.active_role = Some("console".into());
    snapshot.intent.power = crate::state::RoomPower::On;
    snapshot.intent.active_role = Some("console".into());
    h.state.publish(snapshot, []);
    let partial = body(h.get("/p/cockpit", Some(&cookie)).await).await;
    // One response: the cockpit body plus the chrome blocks the poll swaps out of band.
    assert!(partial.contains("<main id=\"cockpit\""));
    let status = between(&partial, "id=\"status\"", "<div class=\"conns\"");
    assert!(status.contains("id=\"mode-active\""));
    assert!(partial.contains("<div id=\"bands\""));
    assert!(partial.contains("conn up\" title=\"MQTT connected"));
    for id in [
        "mode-shadow",
        "mode-active",
        "all-off",
        "takeover-streamer",
        "standby-console",
    ] {
        assert!(
            partial.contains(&format!("id=\"{id}\"")),
            "{id} has a stable id"
        );
    }
    // The active role offers standby instead of take over.
    assert!(!partial.contains("id=\"takeover-console\""));
    // The cockpit page polls itself; every other page polls the status block.
    let page = body(h.get("/", Some(&cookie)).await).await;
    assert!(page.contains("hx-get=\"/p/cockpit\""));
    assert!(!page.contains("hx-get=\"/p/status\""));
    let devices = body(h.get("/devices", Some(&cookie)).await).await;
    assert!(devices.contains("hx-get=\"/p/status\""));
}

#[tokio::test]
async fn role_path_segments_cannot_escape_their_action() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let mut snapshot = h.state.snapshot();
    snapshot.roles[0].role = "../all-off".to_owned();
    snapshot.roles[1].role = "a/b".to_owned();
    snapshot.roles[1].power = RolePower::On;
    h.state.publish(snapshot, []);
    let html = body(h.get("/", Some(&cookie)).await).await;
    assert!(html.contains(r#"hx-post="/actions/takeover/..%2Fall-off""#));
    assert!(html.contains(r#"hx-post="/actions/standby/a%2Fb""#));
    assert!(!html.contains("/actions/takeover/../"));
    assert!(!html.contains("/actions/standby/a/b"));
}

/// The `<article>` of the cockpit card titled `title`.
fn card<'a>(html: &'a str, title: &str) -> &'a str {
    between(
        html,
        &format!("<h2 class=\"grow\">{title}</h2>"),
        "</article>",
    )
}

#[tokio::test]
async fn unreachable_tv_reads_as_off_while_the_receiver_keeps_its_error_state() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let mut snapshot = h.state.snapshot();
    snapshot.ha.tv.state = "unavailable".to_owned();
    snapshot.ha.receiver.state = "unavailable".to_owned();
    h.state.publish(snapshot, []);
    let html = body(h.get("/p/cockpit", Some(&cookie)).await).await;
    let tv = card(&html, "TV");
    assert!(
        tv.contains("<span class=\"src\">ha</span> off (unreachable)"),
        "{tv}"
    );
    assert!(tv.contains("<span class=\"chip dim\">off</span>"), "{tv}");
    let receiver = card(&html, "Receiver");
    assert!(!receiver.contains("off (unreachable)"), "{receiver}");
    assert!(
        receiver.contains("<span class=\"src\">ha</span> unavailable"),
        "{receiver}"
    );
    assert!(
        receiver.contains("<span class=\"chip none\">unknown</span>"),
        "{receiver}"
    );
}

#[tokio::test]
async fn role_cards_read_home_assistant_only_for_roles_with_a_media_player() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let mut snapshot = h.state.snapshot();
    let streamer = snapshot
        .ha
        .media_players
        .iter_mut()
        .find(|entity| entity.entity == "media_player.test_streamer")
        .expect("the streamer's media player is tracked");
    streamer.state = "playing".to_owned();
    h.state.publish(snapshot, []);
    let html = body(h.get("/p/cockpit", Some(&cookie)).await).await;
    let streamer = card(&html, "Test Streamer");
    assert!(
        streamer.contains("<span class=\"src\">ha</span> playing"),
        "{streamer}"
    );
    let console = card(&html, "Test Console");
    assert!(
        console.contains("<span class=\"src\">ha</span> not configured"),
        "{console}"
    );
}

#[tokio::test]
async fn off_timer_shows_only_an_exposed_deadline_never_a_guess() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    // On with no active role used to be read as a running timer; it is not evidence of one.
    let mut snapshot = h.state.snapshot();
    snapshot.intent.power = crate::state::RoomPower::On;
    snapshot.intent.active_role = None;
    snapshot.intent.off_at = Some(chrono::Utc::now() + chrono::TimeDelta::seconds(60));
    h.state.publish(snapshot.clone(), []);
    let html = body(h.get("/p/cockpit", Some(&cookie)).await).await;
    let timer = between(&html, "<span class=\"k\">Off timer</span>", "</div>");
    assert!(!html.contains("running"), "{timer}");
    assert!(!timer.contains("off in"), "{timer}");
    assert!(
        timer.contains("<span class=\"dim small\">Starts when the last role turns off.</span>"),
        "{timer}"
    );

    let deadline = chrono::Utc::now() + chrono::TimeDelta::seconds(90);
    snapshot.off_deadline = Some(deadline);
    h.state.publish(snapshot, []);
    let html = body(h.get("/p/cockpit", Some(&cookie)).await).await;
    let timer = between(&html, "<span class=\"k\">Off timer</span>", "</div>");
    assert!(
        timer.contains(&format!("off at {} (in ", crate::web::view::hms(&deadline))),
        "{timer}"
    );
    assert!(timer.contains("<span class=\"value\">off in "), "{timer}");
    assert!(!html.contains("running"), "{timer}");
}

#[tokio::test]
async fn restored_roles_wait_for_the_bus_while_unmatched_roles_read_unbound() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let mut snapshot = h.state.snapshot();
    snapshot.warming = false;
    // The runtime flags the streamer: unbound, and a restored unverified entry matches it.
    snapshot.bus = vec![bus_entry(4)];
    let streamer = snapshot
        .roles
        .iter_mut()
        .find(|role| role.role == "streamer")
        .unwrap();
    streamer.restored = true;
    h.state.publish(snapshot, []);
    let html = body(h.get("/p/cockpit", Some(&cookie)).await).await;
    let streamer = card(&html, "Test Streamer");
    assert!(
        streamer.contains("<span class=\"chip pend\">restored</span>"),
        "{streamer}"
    );
    assert!(
        streamer.contains("Restored from the state file"),
        "{streamer}"
    );
    assert!(
        streamer.contains("<span class=\"src\">cec</span> silent since restore"),
        "{streamer}"
    );
    let takeover = between(&html, "id=\"takeover-streamer\"", "</button>");
    assert!(takeover.contains("disabled"), "{takeover}");
    let console = card(&html, "Test Console");
    assert!(
        console.contains("<span class=\"chip dim\">unbound</span>"),
        "{console}"
    );
    assert!(
        !console.contains("Restored from the state file"),
        "{console}"
    );
}

#[tokio::test]
async fn shadow_offers_would_actions_without_confirmation() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let mut snapshot = h.state.snapshot();
    snapshot.warming = false;
    snapshot.roles[0].logical_address = Some(4);
    snapshot.bus = vec![BusEntryView {
        identified: true,
        verified: true,
        matched_role: Some("streamer".to_owned()),
        ..bus_entry(4)
    }];
    h.state.publish(snapshot.clone(), []);
    let html = body(h.get("/p/cockpit", Some(&cookie)).await).await;
    let takeover = between(&html, "id=\"takeover-streamer\"", "</button>");
    assert!(takeover.ends_with(">Would take over…"), "{takeover}");
    assert!(!takeover.contains("hx-confirm"), "{takeover}");
    assert!(!takeover.contains("disabled"), "{takeover}");
    let all_off = between(&html, "id=\"all-off\"", "</button>");
    assert!(all_off.ends_with(">Would turn all off…"), "{all_off}");
    assert!(!all_off.contains("hx-confirm"), "{all_off}");
    // A role that is not bound to a verified device cannot act yet.
    let unbound = between(&html, "id=\"takeover-console\"", "</button>");
    assert!(
        unbound.contains("disabled title=\"not verified yet\""),
        "{unbound}"
    );

    snapshot.mode = crate::state::Mode::Active;
    h.state.publish(snapshot, []);
    let html = body(h.get("/p/cockpit", Some(&cookie)).await).await;
    let takeover = between(&html, "id=\"takeover-streamer\"", "</button>");
    assert!(takeover.ends_with(">Take over…"), "{takeover}");
    assert!(
        takeover.contains("hx-confirm=\"Take over the room with Test Streamer?\""),
        "{takeover}"
    );
}

#[tokio::test]
async fn last_decisions_skip_migrated_history_and_mark_the_restart() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let row = |id: u64, reason: &str, trigger: crate::action::Trigger| DecisionView {
        id,
        reason: reason.to_owned(),
        trigger,
        ..verdict_decision()
    };
    let frame = || crate::action::Trigger::Frame {
        from: 4,
        to: 0,
        data: "82".to_owned(),
    };
    let service = |name| crate::action::Trigger::Service { name };
    let mut snapshot = h.state.snapshot();
    snapshot.decisions.extend([
        row(9, "PowerOn", frame()),
        row(10, "PowerOn", frame()),
        row(11, "PowerOn", service(crate::action::ServiceKind::Migrated)),
        row(
            12,
            "service_started",
            service(crate::action::ServiceKind::Started),
        ),
        row(13, "Standby", frame()),
        row(14, "Standby", frame()),
        row(15, "Standby", frame()),
        row(16, "Standby", frame()),
    ]);
    h.state.publish(snapshot, []);
    let html = body(h.get("/p/cockpit", Some(&cookie)).await).await;
    let table = between(&html, "Last 5 decisions", "</table>");
    // Five rows: the separator and the migrated row take no slot, so #10 is the fifth.
    for id in [16, 15, 14, 13, 10] {
        assert!(table.contains(&format!("id=\"recent-{id}\"")), "#{id}");
    }
    for id in [11, 9] {
        assert!(!table.contains(&format!("id=\"recent-{id}\"")), "#{id}");
    }
    let separator = table
        .find("service started — rows below were made by the previous run")
        .expect("the restart is marked");
    assert!(table.find("id=\"recent-13\"").unwrap() < separator);
    assert!(separator < table.find("id=\"recent-10\"").unwrap());
}

#[tokio::test]
async fn action_responses_toast_out_of_band_and_refresh_the_status() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, csrf) = h.login().await;
    let page = body(h.get("/", Some(&cookie)).await).await;
    assert!(page.contains("<div id=\"toasts\" class=\"toasts\" aria-live=\"polite\"></div>"));

    let _received = answer_one_command(&h.state, Ok(()));
    let response = h
        .post_form("/actions/all-off", &cookie, Some(&csrf), "")
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert!(
        html.contains("hx-swap-oob=\"afterbegin:#toasts\""),
        "{html}"
    );
    assert!(html.contains("All off requested."), "{html}");
    // The harness runs in shadow: the toast says nothing was sent.
    assert!(html.contains("toast would"), "{html}");
    assert!(html.contains("id=\"status\" class=\"status\" hx-swap-oob=\"true\""));
    assert!(html.contains("id=\"bands\" hx-swap-oob=\"true\""));

    let rejected = h
        .post_form("/actions/identify/15", &cookie, Some(&csrf), "")
        .await;
    assert_eq!(rejected.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let html = body(rejected).await;
    assert!(html.contains("toast bad"), "{html}");
    assert!(html.contains("Logical address must be 0–14."), "{html}");
}

/// Publishes the current snapshot after `edit`; returns the bridge drawer.
async fn drawer_after(h: &Harness, cookie: &str, edit: impl FnOnce(&mut Snapshot)) -> String {
    let mut snapshot = h.state.snapshot();
    edit(&mut snapshot);
    h.state.publish(snapshot, []);
    body(h.get("/p/bridge", Some(cookie)).await).await
}

fn online(snapshot: &mut Snapshot) {
    snapshot.bridge_online = true;
    snapshot.connections.bridge_online = true;
}

/// Telemetry as an adapter reports it, `age` old, stale after the adapter's `stale_after_s`.
fn telemetry(signal_dbm: i32, age: chrono::TimeDelta, stale_after_s: u64) -> BridgeTelemetry {
    BridgeTelemetry {
        signal_dbm,
        link_count: 2,
        uptime_s: 282_348,
        at: chrono::Utc::now() - age,
        stale_after_s,
    }
}

#[tokio::test]
async fn bridge_drawer_hosts_raw_send_and_devices_no_longer_does() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let drawer = drawer_after(&h, &cookie, |_| {}).await;
    let form = between(&drawer, "<form id=\"raw-send\"", "</form>");
    assert!(form.contains("hx-post=\"/actions/send-raw\""), "{form}");
    assert!(
        form.contains("hx-confirm=\"Send this raw CEC frame?\""),
        "{form}"
    );
    assert!(form.contains("name=\"to\"") && form.contains("name=\"data\""));
    let devices = body(h.get("/devices", Some(&cookie)).await).await;
    assert!(!devices.contains("/actions/send-raw"));
    // The close button asks for nothing, which empties the drawer and stops its polling.
    let closed = h.get("/p/bridge?close=true", Some(&cookie)).await;
    assert_eq!(closed.status(), StatusCode::OK);
    assert!(body(closed).await.is_empty());
}

#[tokio::test]
async fn bridge_drawer_shows_boot_telemetry_and_last_frame_while_online() {
    let h = harness_from(bridge_config_text(
        UNREACHABLE_ISSUER,
        PUBLIC_URL,
        AETHER_BRIDGE,
    ));
    let (cookie, _) = h.login().await;
    let drawer = drawer_after(&h, &cookie, |snapshot| {
        online(snapshot);
        snapshot.bridge_node = Some("cec-bridge".to_owned());
        snapshot.bridge_boot = Some(7);
        snapshot.bridge_telemetry = Some(telemetry(-50, chrono::TimeDelta::minutes(2), 900));
        snapshot.last_frame_at = Some(chrono::Utc::now() - chrono::TimeDelta::seconds(12));
    })
    .await;
    assert!(drawer.contains("id=\"bridge-boot\">7</dd>"), "{drawer}");
    assert!(
        drawer.contains("<dd class=\"mono\">cec-bridge</dd>"),
        "{drawer}"
    );
    assert!(drawer.contains("−50 dBm · 2 links</dd>"), "{drawer}");
    assert!(!classes_around(&drawer, "−50 dBm").contains(&"warn-text"));
    assert!(drawer.contains(">3d 6h</dd>"), "{drawer}");
    // Exact ages are covered by the fixed-clock `view::ago_at` test; here only their presence.
    assert!(between(&drawer, "id=\"bridge-telemetry\" class=\"mono\">", "</dd>").ends_with(" ago"));
    assert!(
        between(&drawer, "id=\"bridge-last-frame\" class=\"mono\">", "</dd>").ends_with(" ago")
    );
    let status = body(h.get("/p/status", Some(&cookie)).await).await;
    assert!(status.contains("title=\"CEC bridge online · −50 dBm\""));

    // A status without a boot counter (Tasmota, or none received yet) shows none.
    let drawer = drawer_after(&h, &cookie, |snapshot| {
        snapshot.bridge_boot = None;
        snapshot.bridge_node = None;
    })
    .await;
    assert!(!drawer.contains("Boot count"), "{drawer}");
    assert!(!drawer.contains("cec-bridge"), "{drawer}");
}

#[tokio::test]
async fn weak_signal_or_telemetry_past_its_stale_after_is_flagged_while_online() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    // The fixed-clock `view` tests pin the boundaries; here the drawer follows the adapter's
    // threshold, and a zero threshold (telemetry disabled) only judges the signal.
    for (signal, age_s, stale_after_s, flagged) in [
        (-80, 10, 600, true),
        (-50, 120, 60, true),
        (-50, 120, 600, false),
        (-50, 86_400, 0, false),
        (-80, 86_400, 0, true),
    ] {
        let age = chrono::TimeDelta::seconds(age_s);
        let drawer = drawer_after(&h, &cookie, |snapshot| {
            online(snapshot);
            snapshot.bridge_telemetry = Some(telemetry(signal, age, stale_after_s));
        })
        .await;
        assert_eq!(
            classes_around(&drawer, " dBm").contains(&"warn-text"),
            flagged,
            "{signal} dBm, {age}, stale after {stale_after_s} s: {drawer}"
        );
    }
}

#[tokio::test]
async fn bus_line_fault_is_flagged_only_when_not_ok() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    for (bus, fault) in [
        (None, None),
        (Some("ok"), None),
        (Some("stuck_low"), Some("stuck_low")),
        (Some("no_pullup"), Some("no_pullup")),
    ] {
        let drawer = drawer_after(&h, &cookie, |snapshot| {
            online(snapshot);
            snapshot.bridge_bus = bus.map(str::to_owned);
        })
        .await;
        let cockpit = body(h.get("/p/cockpit", Some(&cookie)).await).await;
        let alerts = between(
            &cockpit,
            "<div id=\"alerts\" class=\"alerts\">",
            "<div class=\"section-h\">",
        );
        match fault {
            Some(fault) => {
                assert!(
                    drawer.contains(&format!("<span class=\"chip bad\">{fault}</span>")),
                    "{bus:?}: {drawer}"
                );
                assert!(alerts.contains(&format!("Bus fault: {fault}")), "{alerts}");
            }
            None => {
                assert!(!drawer.contains("chip bad"), "{bus:?}: {drawer}");
                assert!(!alerts.contains("Bus fault"), "{alerts}");
            }
        }
    }
}

#[tokio::test]
async fn bridge_without_telemetry_says_so_and_offline_shows_none() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let drawer = drawer_after(&h, &cookie, online).await;
    assert!(drawer.contains("<span class=\"chip ok\">online</span>"));
    assert!(drawer.contains("no telemetry yet"));

    let offline = drawer_after(&h, &cookie, |snapshot| {
        snapshot.bridge_online = false;
        snapshot.connections.bridge_online = false;
        snapshot.bridge_telemetry = Some(telemetry(-80, chrono::TimeDelta::minutes(30), 900));
        // Last known line state from before the bridge dropped: stale, like the telemetry.
        snapshot.bridge_bus = Some("stuck_low".to_owned());
    })
    .await;
    assert!(offline.contains("<span class=\"chip bad\">offline</span>"));
    for absent in ["dBm", "Telemetry", "links", "warn-text", "stuck_low"] {
        assert!(
            !offline.contains(absent),
            "offline shows {absent}: {offline}"
        );
    }
    let status = body(h.get("/p/status", Some(&cookie)).await).await;
    assert!(status.contains("title=\"CEC bridge offline\""));
    assert!(status.contains("Bridge offline — no frame has arrived yet."));
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
