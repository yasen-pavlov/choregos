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
    config::{Config, ConfigError},
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
    /// The config file the fake runtime reads and saves through the runtime's own
    /// `read_config` / `reload`.
    path: std::path::PathBuf,
    /// Errors the fake runtime answers the next saves with, before touching the file.
    rejections: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<ConfigError>>>,
    _directory: tempfile::TempDir,
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
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("choregos.toml");
    std::fs::write(&path, &text).expect("test config is written");
    let state = AppState::new(config.clone(), text);
    let rejections = std::sync::Arc::default();
    fake_runtime(
        &state,
        config,
        path.clone(),
        std::sync::Arc::clone(&rejections),
    );
    Harness {
        app: router(state.clone()),
        state,
        path,
        rejections,
        _directory: directory,
    }
}

/// Answers config reads and saves as the runtime does, against the harness file: a save is
/// checked, compared with the file's hash and written by `runtime::reload`, then becomes the
/// running config. Commands get no answer (the runtime is "unavailable" to them).
fn fake_runtime(
    state: &AppState,
    mut running: Config,
    path: std::path::PathBuf,
    rejections: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<ConfigError>>>,
) {
    let (sender, mut inputs) = tokio::sync::mpsc::channel(8);
    state.attach(sender);
    let state = state.clone();
    tokio::spawn(async move {
        while let Some(input) = inputs.recv().await {
            match input {
                crate::runtime::Input::Request(crate::runtime::Request::ReadConfig(reply)) => {
                    let _ = reply.send(crate::runtime::read_config(&path).await);
                }
                crate::runtime::Input::Request(crate::runtime::Request::SaveConfig(
                    text,
                    expected,
                    reply,
                )) => {
                    let rejection = rejections.lock().unwrap().pop_front();
                    let result = match rejection {
                        Some(error) => Err(error),
                        None => crate::runtime::reload(&text, expected, &running, &path)
                            .await
                            .map(|config| {
                                running = config;
                                state.replace_config_text(text);
                            }),
                    };
                    let _ = reply.send(result);
                }
                _ => {}
            }
        }
    });
}

impl Harness {
    /// The config file as it is on disk.
    fn file(&self) -> String {
        std::fs::read_to_string(&self.path).expect("config file")
    }

    /// The next save is answered with `error` and writes nothing.
    fn reject_next_save(&self, error: ConfigError) {
        self.rejections.lock().unwrap().push_back(error);
    }

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
        "/recorder",
        "/p/recorder",
        "/recorder/1",
        "/p/window/1",
        "/recorder/glossary",
        "/recorder/live",
        "/recorder/live/events",
        "/devices",
        "/p/devices",
        "/devices/streamer",
        "/devices/out/tv",
        "/devices/la/3",
        "/config",
        "/p/cockpit",
        "/p/status",
        "/p/bridge",
        "/p/config/timing",
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

fn encode(pairs: &[(&str, &str)]) -> String {
    openidconnect::url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
}

/// The value of the hidden input `name` in rendered HTML, unescaped.
fn hidden(html: &str, name: &str) -> String {
    unescape(between(html, &format!("name=\"{name}\" value=\""), "\""))
}

/// The value a browser gives the textarea `name`: its text, unescaped, without the one
/// newline the HTML parser drops after the start tag.
fn textarea(html: &str, name: &str) -> String {
    let at = html
        .find(&format!("<textarea id=\"f-{}\"", name.replace('.', "-")))
        .unwrap_or_else(|| panic!("textarea {name} is rendered"));
    let content = between(&html[at..], ">", "</textarea>");
    unescape(content.strip_prefix('\n').unwrap_or(content))
}

fn unescape(escaped: &str) -> String {
    let mut out = String::new();
    let mut rest = escaped;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let end = rest[at..].find(';').expect("entity end") + at;
        let entity = &rest[at + 1..end];
        out.push(match entity {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            numeric => char::from_u32(numeric.trim_start_matches('#').parse().expect("entity"))
                .expect("char"),
        });
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

#[tokio::test]
async fn config_lists_every_section_and_opens_the_first_editable_one() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let page = ok_page(&h, &cookie, "/config").await;
    for (section, badge) in [
        ("mqtt", "<span class=\"chip dim\">restart</span>"),
        ("home_assistant", "<span class=\"chip dim\">restart</span>"),
        ("web", "<span class=\"chip dim\">restart</span>"),
        ("tv", "<span class=\"chip dim\">partly</span>"),
        ("devices", "<span class=\"chip plain\">2</span>"),
        ("timing", "</a>"),
    ] {
        assert!(
            page.contains(&format!("<span class=\"mono\">{section}</span>{badge}")),
            "{section}: {page}"
        );
    }
    assert!(page.contains("hx-push-url=\"/config?section=bridge\" aria-current=\"page\""));
    assert!(page.contains("name=\"section\" value=\"bridge\""));
    assert!(page.contains("<h2 class=\"mono\">[bridge]</h2>"));
    assert_eq!(
        h.get("/config?section=nope", Some(&cookie)).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.get("/p/config/nope", Some(&cookie)).await.status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_section_partial_shows_defaults_bounds_and_moves_the_nav() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let html = ok_page(&h, &cookie, "/p/config/timing").await;
    assert!(html.starts_with("<form id=\"section\""), "{html}");
    assert!(html.contains(
        "name=\"timing.standby_retries\" value=\"2\" inputmode=\"numeric\" min=\"0\" max=\"10\""
    ));
    assert!(
        html.contains("Default 2 · 0–10."),
        "absent key shows its default"
    );
    assert!(html.contains(
        "<nav id=\"cfg-nav\" class=\"cfg-nav\" aria-label=\"Sections\" hx-swap-oob=\"true\">"
    ));
    assert!(html.contains("hx-push-url=\"/config?section=timing\" aria-current=\"page\""));
    // A restart-gated section is read-only: values and secret names, no inputs, no footer.
    let web = ok_page(&h, &cookie, "/p/config/web").await;
    assert!(web.contains(
        "<dt class=\"mono\">public_url</dt><dd class=\"mono\">https://choregos.example.com/</dd>"
    ));
    assert!(!web.contains("<textarea") && !web.contains("id=\"cfg-foot\""));
}

#[tokio::test]
async fn secrets_appear_only_as_variable_names() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    for path in [
        "/config?section=web",
        "/config?section=mqtt",
        "/config?section=home_assistant",
        "/config?section=tv",
        "/config?tab=raw",
    ] {
        let html = ok_page(&h, &cookie, path).await;
        assert!(
            !html.contains(PASSWORD) && !html.contains("test-only-"),
            "{path}"
        );
    }
    let web = ok_page(&h, &cookie, "/config?section=web").await;
    for name in [
        "CHOREGOS_SESSION_SECRET",
        "CHOREGOS_OIDC_CLIENT_SECRET",
        "CHOREGOS_LOCAL_PASSWORD",
    ] {
        assert!(
            web.contains(&format!("env {name} <span class=\"chip")),
            "{name}"
        );
    }
}

#[tokio::test]
async fn validate_pins_an_out_of_bounds_value_to_its_field_and_writes_nothing() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, csrf) = h.login().await;
    let saved = h.file();
    let response = h
        .post_form(
            "/config/validate",
            &cookie,
            Some(&csrf),
            &encode(&[("section", "timing"), ("timing.standby_retries", "12")]),
        )
        .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let html = body(response).await;
    assert!(
        html.contains("<p id=\"f-timing-standby_retries-e\" class=\"ferr\" hx-swap-oob=\"true\">Must be between 0 and 10.</p>"),
        "{html}"
    );
    assert!(html.contains(
        "Fix the error in <a href=\"#f-timing-standby_retries\">timing.standby_retries</a>"
    ));
    // Only the control whose validity flipped is re-rendered, now marked invalid; the others
    // are left alone (they may be mid-typing).
    assert!(
        html.contains("<input id=\"f-timing-standby_retries\" class=\"num\" type=\"number\" name=\"timing.standby_retries\" value=\"12\" inputmode=\"numeric\" min=\"0\" max=\"10\" aria-invalid=\"true\" aria-describedby=\"f-timing-standby_retries-e f-timing-standby_retries-h\" hx-swap-oob=\"true\">"),
        "{html}"
    );
    assert!(!html.contains("<input id=\"f-timing-off_delay_s\""));
    assert!(html.contains(
        "<input type=\"hidden\" id=\"cfg-invalid\" name=\"_invalid\" value=\"timing.standby_retries\" hx-swap-oob=\"true\">"
    ));
    // Still wrong and already marked: nothing to re-render. Fixed: the mark comes off.
    for (value, restated) in [("11", false), ("3", true)] {
        let html = body(
            h.post_form(
                "/config/validate",
                &cookie,
                Some(&csrf),
                &encode(&[
                    ("section", "timing"),
                    ("_invalid", "timing.standby_retries"),
                    ("timing.standby_retries", value),
                ]),
            )
            .await,
        )
        .await;
        assert_eq!(
            html.contains("<input id=\"f-timing-standby_retries\""),
            restated,
            "{value}: {html}"
        );
        assert!(!html.contains("aria-invalid") || !restated, "{html}");
    }
    assert_eq!(h.file(), saved);

    let response = h
        .post_form(
            "/config/validate",
            &cookie,
            Some(&csrf),
            &encode(&[("section", "timing"), ("timing.off_delay_s", "5")]),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert!(
        html.contains("<span class=\"chip info\">changed · was 3</span>"),
        "{html}"
    );
    assert!(html.contains("<span class=\"chip info\">1 changed</span>"));
    assert_eq!(h.file(), saved);
}

#[tokio::test]
async fn the_raw_editor_reports_lines_and_rejects_restart_gated_edits() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, csrf) = h.login().await;
    let saved = h.file();
    let gated = saved.replace("host = \"mqtt.test\"", "host = \"broker.test\"");
    let response = h
        .post_form(
            "/config/validate",
            &cookie,
            Some(&csrf),
            &encode(&[("raw", &gated)]),
        )
        .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let html = body(response).await;
    assert!(html.contains("mqtt changes require a restart"), "{html}");
    assert!(html.contains("<span class=\"chip info\">1 changed</span>"));

    let broken = saved.replace("[timing]", "[timing]\nmode = [\"<b>");
    let response = h
        .post_form(
            "/config/preview",
            &cookie,
            Some(&csrf),
            &encode(&[("raw", &broken)]),
        )
        .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let html = body(response).await;
    let line = saved.lines().position(|l| l == "[timing]").unwrap() + 2;
    assert!(html.contains(&format!("line {line}: ")), "{html}");
    assert!(
        html.contains("mode = [&#34;&#60;b&#62;"),
        "the text is escaped back"
    );
    assert_eq!(h.file(), saved);
}

#[tokio::test]
async fn preview_lists_sentences_the_diff_and_the_file_hash() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, csrf) = h.login().await;
    let response = h
        .post_form(
            "/config/preview",
            &cookie,
            Some(&csrf),
            &encode(&[("section", "timing"), ("timing.off_delay_s", "5")]),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert!(html.contains("<h2>Review 1 change</h2>"), "{html}");
    assert!(html.contains("<li>The room waits 5 s instead of 3 s before turning off.</li>"));
    assert!(
        html.contains("<ol class=\"diff mono\">"),
        "the diff is a list: {html}"
    );
    assert!(
        html.contains("<li class=\"dl hunk\"><span class=\"sign\"></span>@@ [timing] @@</li>"),
        "{html}"
    );
    assert!(
        html.contains("<li class=\"dl del\"><span class=\"sign\">-</span>off_delay_s = 3</li>")
    );
    assert!(
        html.contains("<li class=\"dl add\"><span class=\"sign\">+</span>off_delay_s = 5</li>")
    );
    let text = h.file();
    assert_eq!(
        hidden(&html, "_base"),
        crate::web::view::config::hash(&text)
    );
    assert_eq!(
        hidden(&html, "_proposed"),
        text.replace("off_delay_s = 3", "off_delay_s = 5")
    );
    assert_eq!(
        hidden(&html, "timing.off_delay_s"),
        "5",
        "the form rides along"
    );

    // Nothing to review: the section comes back as it is.
    let response = h
        .post_form(
            "/config/preview",
            &cookie,
            Some(&csrf),
            &encode(&[("section", "timing")]),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert!(
        html.contains("No changes yet.") && !html.contains("Review 0"),
        "{html}"
    );
}

#[tokio::test]
async fn save_against_a_moved_file_conflicts_with_a_fresh_review() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, csrf) = h.login().await;
    let saved = h.file();
    let response = h
        .post_form(
            "/config/save",
            &cookie,
            Some(&csrf),
            &encode(&[
                ("section", "timing"),
                ("timing.off_delay_s", "5"),
                ("_base", "0000000000000000"),
                (
                    "_proposed",
                    &saved.replace("off_delay_s = 3", "off_delay_s = 5"),
                ),
            ]),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let html = body(response).await;
    assert!(
        html.contains("The file changed since you opened the editor"),
        "{html}"
    );
    assert_eq!(
        hidden(&html, "_base"),
        crate::web::view::config::hash(&saved)
    );
    assert_eq!(h.file(), saved);
}

/// Opens the timing review that changes `off_delay_s` to 5; returns the save form.
async fn timing_review(h: &Harness, cookie: &str, csrf: &str) -> String {
    let preview = body(
        h.post_form(
            "/config/preview",
            cookie,
            Some(csrf),
            &encode(&[("section", "timing"), ("timing.off_delay_s", "5")]),
        )
        .await,
    )
    .await;
    encode(&[
        ("section", "timing"),
        ("timing.off_delay_s", "5"),
        ("_base", &hidden(&preview, "_base")),
        ("_proposed", &hidden(&preview, "_proposed")),
    ])
}

#[tokio::test]
async fn an_external_edit_after_review_conflicts_and_survives() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, csrf) = h.login().await;
    let save = timing_review(&h, &cookie, &csrf).await;
    // Someone edits the mounted file between the review and the save.
    let external = h
        .file()
        .replace("[timing]\n", "[timing]\nstandby_retries = 4\n");
    std::fs::write(&h.path, &external).unwrap();
    let response = h
        .post_form("/config/save", &cookie, Some(&csrf), &save)
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let html = body(response).await;
    assert!(
        html.contains("The file changed since you opened the editor"),
        "{html}"
    );
    assert_eq!(h.file(), external, "the external edit is kept");
    // The fresh review is against the edited file and saves on top of it.
    let proposed = hidden(&html, "_proposed");
    assert_eq!(
        proposed,
        external.replace("off_delay_s = 3", "off_delay_s = 5")
    );
    let retry = encode(&[
        ("section", "timing"),
        ("timing.off_delay_s", "5"),
        ("_base", &hidden(&html, "_base")),
        ("_proposed", &proposed),
    ]);
    let response = h
        .post_form("/config/save", &cookie, Some(&csrf), &retry)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(h.file(), proposed);
}

fn encode_owned(pairs: &[(String, String)]) -> String {
    let pairs: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    encode(&pairs)
}

/// Every `<input type="hidden" name=…>` of `html`, unescaped, as the browser posts them.
fn hidden_inputs(html: &str) -> Vec<(String, String)> {
    html.match_indices("<input type=\"hidden\" name=\"")
        .map(|(at, marker)| {
            let rest = &html[at + marker.len()..];
            let name = between(rest, "", "\"").to_owned();
            (name, unescape(between(rest, "value=\"", "\"")))
        })
        .collect()
}

/// What the browser posts from the timing page `page` (its originals included), with
/// `edits` typed over the rendered values.
fn timing_form(page: &str, edits: &[(&str, &str)]) -> String {
    let mut form: Vec<(String, String)> = hidden_inputs(page)
        .into_iter()
        .filter(|(name, _)| name == "section" || name == "_orig")
        .collect();
    for (at, _) in page.match_indices("type=\"number\" name=\"") {
        let name = between(&page[at..], "name=\"", "\"").to_owned();
        let rendered = between(&page[at..], "value=\"", "\"").to_owned();
        let value = edits
            .iter()
            .find(|(edited, _)| *edited == name)
            .map_or(rendered, |(_, typed)| (*typed).to_owned());
        form.push((name, value));
    }
    encode_owned(&form)
}

/// Whether the review's diff adds or removes a line starting with `key`.
fn diff_touches(review: &str, key: &str) -> bool {
    ["-", "+"]
        .iter()
        .any(|sign| review.contains(&format!("<span class=\"sign\">{sign}</span>{key}")))
}

#[tokio::test]
async fn a_conflicting_save_applies_only_the_users_edit_and_keeps_the_external_one() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, csrf) = h.login().await;
    let page = ok_page(&h, &cookie, "/config?section=timing").await;
    let form = timing_form(&page, &[("timing.off_delay_s", "5")]);
    let review = body(
        h.post_form("/config/preview", &cookie, Some(&csrf), &form)
            .await,
    )
    .await;
    // Between review and save, another key of the same section changes in the file.
    let external = h
        .file()
        .replace("presence_poll_s = 5", "presence_poll_s = 7");
    std::fs::write(&h.path, &external).unwrap();
    let response = h
        .post_form(
            "/config/save",
            &cookie,
            Some(&csrf),
            &encode_owned(&hidden_inputs(&review)),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let html = body(response).await;
    assert!(html.contains("<h2>Review 1 change</h2>"), "{html}");
    assert!(diff_touches(&html, "off_delay_s = 5"));
    assert!(!diff_touches(&html, "presence_poll_s"), "{html}");
    let proposed = hidden(&html, "_proposed");
    assert_eq!(
        proposed,
        external.replace("off_delay_s = 3", "off_delay_s = 5")
    );
    let response = h
        .post_form(
            "/config/save",
            &cookie,
            Some(&csrf),
            &encode_owned(&hidden_inputs(&html)),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(h.file(), proposed);
    assert!(h.file().contains("presence_poll_s = 7\n"));
}

#[tokio::test]
async fn an_edit_back_to_the_value_the_form_showed_changes_nothing() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, csrf) = h.login().await;
    let page = ok_page(&h, &cookie, "/config?section=timing").await;
    // The file's off_delay_s moves outside the form; the user types 5, then back to 3.
    let external = h.file().replace("off_delay_s = 3", "off_delay_s = 8");
    std::fs::write(&h.path, &external).unwrap();
    let typed = |value| timing_form(&page, &[("timing.off_delay_s", value)]);
    let html = body(
        h.post_form("/config/validate", &cookie, Some(&csrf), &typed("5"))
            .await,
    )
    .await;
    assert!(html.contains("1 changed"), "{html}");
    let html = body(
        h.post_form("/config/validate", &cookie, Some(&csrf), &typed("3"))
            .await,
    )
    .await;
    assert!(html.contains("No changes yet."), "{html}");
    let response = h
        .post_form("/config/preview", &cookie, Some(&csrf), &typed("3"))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert!(
        html.contains("No changes yet.") && !html.contains("<h2>Review"),
        "{html}"
    );
    assert_eq!(h.file(), external, "the file's 8 stays");
}

#[tokio::test]
async fn the_review_diff_never_lists_an_untouched_key() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, csrf) = h.login().await;
    let page = ok_page(&h, &cookie, "/config?section=timing").await;
    let external = h
        .file()
        .replace("presence_poll_s = 5", "presence_poll_s = 7")
        .replace("identify_retry_s = 10", "identify_retry_s = 20");
    std::fs::write(&h.path, &external).unwrap();
    let review = body(
        h.post_form(
            "/config/preview",
            &cookie,
            Some(&csrf),
            &timing_form(&page, &[("timing.off_delay_s", "5")]),
        )
        .await,
    )
    .await;
    assert!(review.contains("<h2>Review 1 change</h2>"), "{review}");
    for untouched in [
        "takeover_settle_ms",
        "presence_poll_s",
        "presence_misses",
        "identify_retry_s",
    ] {
        assert!(!diff_touches(&review, untouched), "{untouched}: {review}");
    }
    assert_eq!(
        hidden(&review, "_proposed"),
        external.replace("off_delay_s = 3", "off_delay_s = 5")
    );
}

#[tokio::test]
async fn a_file_change_the_runtime_sees_at_write_time_conflicts_too() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, csrf) = h.login().await;
    let before = h.file();
    let save = timing_review(&h, &cookie, &csrf).await;
    // The web check passes; the runtime's compare right before the write does not.
    h.reject_next_save(ConfigError::Changed);
    let response = h
        .post_form("/config/save", &cookie, Some(&csrf), &save)
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(
        body(response)
            .await
            .contains("The file changed since you opened the editor")
    );
    assert_eq!(h.file(), before);
}

#[tokio::test]
async fn save_goes_through_the_runtime_and_refreshes_the_status() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, csrf) = h.login().await;
    let preview = body(
        h.post_form(
            "/config/preview",
            &cookie,
            Some(&csrf),
            &encode(&[("section", "verify"), ("verify.drift_grace_s", "45")]),
        )
        .await,
    )
    .await;
    let proposed = hidden(&preview, "_proposed");
    assert!(
        proposed.contains("[verify]\ndrift_grace_s = 45\n"),
        "{proposed}"
    );
    let base = hidden(&preview, "_base");
    let save = encode(&[
        ("section", "verify"),
        ("verify.drift_grace_s", "45"),
        ("_base", &base),
        ("_proposed", &proposed),
    ]);

    let before = h.file();
    h.reject_next_save(ConfigError::Validation("nope".to_owned()));
    let response = h
        .post_form("/config/save", &cookie, Some(&csrf), &save)
        .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let html = body(response).await;
    assert!(
        html.contains("<strong>Not saved</strong>") && html.contains("invalid configuration: nope")
    );
    assert_eq!(h.file(), before);

    let response = h
        .post_form("/config/save", &cookie, Some(&csrf), &save)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert!(html.contains("Saved and applied · adapters restarted · bridge resync sent"));
    assert!(html.contains("1 change written"));
    assert!(html.contains("<div id=\"status\" class=\"status\" hx-swap-oob=\"true\">"));
    assert_eq!(h.file(), proposed);
    assert_eq!(h.state.config_text(), proposed);
}

/// Every text field of a rendered section as the browser would post it.
fn textareas(html: &str) -> Vec<(String, String)> {
    html.match_indices("<textarea id=\"f-")
        .map(|(at, _)| {
            let name = between(&html[at..], "name=\"", "\"").to_owned();
            let value = textarea(html, &name);
            (name, value)
        })
        .collect()
}

#[tokio::test]
async fn an_untouched_multiline_name_survives_an_unrelated_edit() {
    let text = config_text(UNREACHABLE_ISSUER, PUBLIC_URL).replacen(
        "name = \"Test Streamer\"",
        "name = \"\"\"Test\nStreamer 雪\"\"\"",
        1,
    );
    let h = harness_from(text.clone());
    let (cookie, csrf) = h.login().await;
    let page = ok_page(&h, &cookie, "/config?section=device").await;
    assert_eq!(textarea(&page, "device.0.name"), "Test\nStreamer 雪");
    for line_break in ["\n", "\r\n"] {
        let mut form = vec![
            ("section".to_owned(), "device".to_owned()),
            ("device.0.origin".to_owned(), "0".to_owned()),
            ("device.1.origin".to_owned(), "1".to_owned()),
        ];
        for (name, value) in textareas(&page) {
            let value = match name.as_str() {
                "device.0.source" => "Streamer Input 2".to_owned(),
                _ => value.replace('\n', line_break),
            };
            form.push((name, value));
        }
        let pairs: Vec<(&str, &str)> = form
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        let review = body(
            h.post_form("/config/preview", &cookie, Some(&csrf), &encode(&pairs))
                .await,
        )
        .await;
        assert_eq!(
            hidden(&review, "_proposed"),
            text.replacen(
                "source = \"Streamer Input\"",
                "source = \"Streamer Input 2\"",
                1
            ),
            "{line_break:?}: only the edited line changes"
        );
    }
}

#[tokio::test]
async fn the_phone_section_list_asks_before_dropping_pending_edits() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, csrf) = h.login().await;
    let page = ok_page(&h, &cookie, "/config?section=timing").await;
    let phone = between(
        &page,
        "<details class=\"phone-only cfg-pick\">",
        "</details>",
    );
    assert!(phone.contains("<span class=\"mono\">timing</span></summary>"));
    assert!(!phone.contains("hx-confirm"), "nothing pending yet");
    assert!(!page.contains("<select id=\"cfg-pick\""));
    let html = body(
        h.post_form(
            "/config/validate",
            &cookie,
            Some(&csrf),
            &encode(&[("section", "timing"), ("timing.off_delay_s", "9")]),
        )
        .await,
    )
    .await;
    let phone = between(
        &html,
        "<details class=\"phone-only cfg-pick\">",
        "</details>",
    );
    assert!(
        phone.contains("<a href=\"/config?section=verify\" hx-get=\"/p/config/verify\" hx-target=\"#section\" hx-swap=\"outerHTML\" hx-push-url=\"/config?section=verify\" hx-confirm=\"Leave [timing]? Its 1 unsaved change will be discarded.\">"),
        "{phone}"
    );
}

#[tokio::test]
async fn checkbox_groups_are_fieldsets_named_by_a_legend() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let page = ok_page(&h, &cookie, "/config?section=device").await;
    let group = between(
        &page,
        "<fieldset class=\"chips\" id=\"f-device-0-on\"",
        "</fieldset>",
    );
    assert!(group.contains("<legend class=\"sr-only\">"), "{group}");
    assert!(
        group.contains(
            "<input type=\"checkbox\" id=\"f-device-0-on-0\" name=\"device.0.on\" value=\""
        )
    );
    assert!(!page.contains("aria-labelledby=\"f-device-0-on-l\""));
}

#[tokio::test]
async fn roles_are_added_and_removed_through_the_form() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, csrf) = h.login().await;
    let rows = [
        ("section", "device"),
        ("device.0.origin", "0"),
        ("device.1.origin", "1"),
    ];
    let mut add = rows.to_vec();
    add.push(("op", "add_role"));
    let response = h
        .post_form("/config/validate", &cookie, Some(&csrf), &encode(&add))
        .await;
    assert_eq!(
        response.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "a blank role is incomplete"
    );
    let html = body(response).await;
    assert!(html.contains("<input type=\"hidden\" name=\"device.2.origin\" value=\"\">"));
    assert!(html.contains("<h2 class=\"mono\">[[device]] · 3 roles</h2>"));
    assert!(
        html.contains("device roles must be nonempty and unique"),
        "{html}"
    );

    let mut filled = rows.to_vec();
    filled.extend([
        ("device.2.origin", ""),
        ("device.2.role", "tuner"),
        ("device.2.name", "Tuner"),
        ("device.2.source", "Tuner Input"),
        ("device.2.kind", "recording"),
        ("device.2.match.osd", "Tuner"),
        ("device.2.on", "active_source"),
        ("device.2.off", "standby"),
        ("device.2.takeover", "none"),
        ("device.2.presence", "events"),
    ]);
    let preview = body(
        h.post_form("/config/preview", &cookie, Some(&csrf), &encode(&filled))
            .await,
    )
    .await;
    assert!(preview.contains("<li>Adds role tuner.</li>"), "{preview}");
    let proposed = hidden(&preview, "_proposed");
    let config = Config::parse(&proposed).expect("the new role validates");
    assert_eq!(config.device[2].role, "tuner");

    let removed = encode(&[
        ("section", "device"),
        ("device.0.origin", "0"),
        ("device.1.origin", "1"),
        ("op", "remove_role:1"),
    ]);
    let html = body(
        h.post_form("/config/validate", &cookie, Some(&csrf), &removed)
            .await,
    )
    .await;
    assert!(!html.contains("name=\"device.1.origin\""));
    assert!(html.contains("<h2 class=\"mono\">[[device]] · 1 role</h2>"));
    let preview = body(
        h.post_form(
            "/config/preview",
            &cookie,
            Some(&csrf),
            &encode(&[("section", "device"), ("device.0.origin", "0")]),
        )
        .await,
    )
    .await;
    assert!(
        preview.contains("<li>Removes role console.</li>"),
        "{preview}"
    );
    let roles: Vec<String> = Config::parse(&hidden(&preview, "_proposed"))
        .unwrap()
        .device
        .into_iter()
        .map(|device| device.role)
        .collect();
    assert_eq!(roles, ["streamer"]);
}

#[tokio::test]
async fn fill_from_this_entry_posts_the_bus_keys_into_the_rule() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, csrf) = h.login().await;
    let mut snapshot = h.state.snapshot();
    snapshot.bus.push(BusEntryView {
        osd: Some("Console B".to_owned()),
        physical_address: Some("2.4.0.0".to_owned()),
        vendor: None,
        ..bus_entry(8)
    });
    h.state.publish(snapshot, []);
    let page = ok_page(&h, &cookie, "/config?section=device").await;
    assert!(page.contains("Unbound entries on the bus:"));
    assert!(page.contains("hx-vals='{\"op\":\"fill:1:8\"}'>Fill from this entry</button>"));

    let html = body(
        h.post_form(
            "/config/validate",
            &cookie,
            Some(&csrf),
            &encode(&[
                ("section", "device"),
                ("device.0.origin", "0"),
                ("device.1.origin", "1"),
                ("op", "fill:1:8"),
            ]),
        )
        .await,
    )
    .await;
    assert_eq!(textarea(&html, "device.1.match.osd"), "Console B", "{html}");
    assert_eq!(textarea(&html, "device.1.match.physical"), "2.4.0.0");
    assert!(html.contains(
        "On the bus now: <span class=\"mono\">LA 8 · 2.4.0.0 · — · &#34;Console B&#34;</span>"
    ));
}

#[tokio::test]
async fn a_role_removed_outside_the_form_refuses_the_edit_and_reopens_the_section() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, csrf) = h.login().await;
    let page = ok_page(&h, &cookie, "/config?section=device").await;
    // The user edits the console (form row 1); the browser posts the page's rows.
    let edit = |page: &str, row: &str| {
        let mut form: Vec<(String, String)> = hidden_inputs(page)
            .into_iter()
            .filter(|(name, _)| name == "section" || name == "_orig" || name.ends_with(".origin"))
            .collect();
        form.push((format!("device.{row}.source"), "Console 2".to_owned()));
        encode_owned(&form)
    };
    let form = edit(&page, "1");
    // Before Review, the streamer (file row 0) is removed outside the form.
    let file = h.file();
    let first = file.find("[[device]]").unwrap();
    let second = first + 1 + file[first + 1..].find("[[device]]").unwrap();
    let external = format!("{}{}", &file[..first], &file[second..]);
    std::fs::write(&h.path, &external).unwrap();
    for path in ["/config/validate", "/config/preview"] {
        let response = h.post_form(path, &cookie, Some(&csrf), &form).await;
        assert_eq!(response.status(), StatusCode::CONFLICT, "{path}");
        // Swapped over the whole section, even from a plain change (`hx-swap="none"`).
        assert_eq!(response.headers()["HX-Retarget"], "#section");
        assert_eq!(response.headers()["HX-Reswap"], "outerHTML");
        let html = body(response).await;
        assert!(
            html.contains("The roles in the file changed since you opened this form"),
            "{html}"
        );
        assert!(!html.contains("<h2>Review"), "{html}");
        // The section as the file has it now; the edit is gone.
        assert!(html.contains("<h2 class=\"mono\">[[device]] · 1 role</h2>"));
        assert!(!html.contains("Console 2"), "{html}");
        assert_eq!(h.file(), external, "nothing is written");
    }
    // The reopened section edits the console where it now is.
    let reopened = body(
        h.post_form("/config/preview", &cookie, Some(&csrf), &form)
            .await,
    )
    .await;
    let review = body(
        h.post_form(
            "/config/preview",
            &cookie,
            Some(&csrf),
            &edit(&reopened, "0"),
        )
        .await,
    )
    .await;
    assert!(review.contains("<h2>Review 1 change</h2>"), "{review}");
    assert_eq!(
        hidden(&review, "_proposed"),
        external.replace("Console Input", "Console 2")
    );
}

#[tokio::test]
async fn the_old_single_post_editor_is_gone() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, csrf) = h.login().await;
    let response = h
        .post_form("/config", &cookie, Some(&csrf), "action=save&config=x")
        .await;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
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
        confirmed_at: None,
        next_identify_at: None,
    }
}

/// A bus entry bound to `role`, confirmed live a minute ago.
fn bound_entry(la: u8, physical: &str, osd: &str, role: &str) -> BusEntryView {
    BusEntryView {
        physical_address: Some(physical.to_owned()),
        device_type: Some(4),
        osd: Some(osd.to_owned()),
        power: BusPower::Standby,
        last_seen: Some(chrono::Utc::now() - chrono::TimeDelta::seconds(26)),
        identified: true,
        verified: true,
        matched_role: Some(role.to_owned()),
        confirmed_at: Some(chrono::Utc::now() - chrono::TimeDelta::minutes(1)),
        ..bus_entry(la)
    }
}

/// The bridge, the streamer bound at LA 4, the receiver's audio system at LA 5, an
/// unmatched tuner at LA 3 and an unidentified LA 9 with its next query due in 4½ min.
fn registry_bus(snapshot: &mut Snapshot) {
    let now = chrono::Utc::now();
    let mut audio = bus_entry(5);
    audio.device_type = Some(5);
    audio.physical_address = Some("2.0.0.0".to_owned());
    let mut tuner = bus_entry(3);
    tuner.identified = true;
    tuner.verified = true;
    tuner.osd = Some("Tuner".to_owned());
    let mut unknown = bus_entry(9);
    unknown.verified = true;
    unknown.next_identify_at = Some(now + chrono::TimeDelta::seconds(270));
    snapshot.bus = vec![
        bus_entry(snapshot.bridge_logical_address),
        tuner,
        bound_entry(4, "2.1.0.0", "Streamer", "streamer"),
        audio,
        unknown,
    ];
    snapshot.roles[0].logical_address = Some(4);
    snapshot.roles[0].physical_address = Some("2.1.0.0".to_owned());
    snapshot.roles[0].osd = Some("Streamer".to_owned());
    snapshot.roles[0].last_seen = Some(now - chrono::TimeDelta::seconds(26));
}

#[tokio::test]
async fn registry_lists_roles_outputs_bridge_then_unbound_entries() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let mut snapshot = h.state.snapshot();
    registry_bus(&mut snapshot);
    h.state.publish(snapshot, []);
    let html = ok_page(&h, &cookie, "/devices").await;
    let at = |text: &str| {
        html.find(text)
            .unwrap_or_else(|| panic!("{text:?} is rendered"))
    };
    let order = [
        "Roles · 2",
        "href=\"/devices/streamer\"",
        "href=\"/devices/console\"",
        "Outputs · 3",
        "href=\"/devices/out/tv\"",
        "href=\"/devices/out/receiver\"",
        "href=\"/devices/out/speakers\"",
        "href=\"/devices/out/bridge\"",
        "Unbound bus entries · 2",
        "href=\"/devices/la/3\"",
        "href=\"/devices/la/9\"",
    ];
    for pair in order.windows(2) {
        assert!(at(pair[0]) < at(pair[1]), "{} before {}", pair[0], pair[1]);
    }
    // The audio system belongs to the receiver row, the bridge to its own section.
    assert!(!html.contains("href=\"/devices/la/5\""));
    assert!(!html.contains("href=\"/devices/la/0\""));
    assert!(row_containing(&html, "href=\"/devices/out/receiver\"").contains("5 · 2.0.0.0"));
    let streamer = row_containing(&html, "href=\"/devices/streamer\"");
    assert!(streamer.contains(">bound<") && streamer.contains("4 · 2.1.0.0"));
    assert!(
        streamer.contains("cec · ha"),
        "the streamer has a media player"
    );
    assert!(row_containing(&html, "href=\"/devices/console\"").contains(">unbound<"));
    let unknown = row_containing(&html, "href=\"/devices/la/9\"");
    assert!(unknown.contains(">unidentified<"));
    assert!(unknown.contains("next query in 4 m"), "{unknown}");
    assert!(row_containing(&html, "href=\"/devices/la/3\"").contains(">unmatched<"));
    // Footer notes only for states present: no restored role here.
    assert!(html.contains("LA 9 has talked but not yet answered every identification query."));
    assert!(html.contains("LA 3 is fully identified but no role"));
    assert!(!html.contains("came from the state file"));
    // The page polls the rows block.
    assert!(html.contains("hx-get=\"/p/devices\" hx-trigger=\"every 5s\""));
}

#[tokio::test]
async fn registry_poll_returns_the_rows_block_only() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let mut snapshot = h.state.snapshot();
    registry_bus(&mut snapshot);
    h.state.publish(snapshot, []);
    let partial = ok_page(&h, &cookie, "/p/devices").await;
    assert!(partial.contains("id=\"registry\""));
    assert!(partial.contains("href=\"/devices/la/9\""));
    for chrome in [
        "<html",
        "class=\"topbar\"",
        "id=\"status\"",
        "<h1>Devices</h1>",
    ] {
        assert!(!partial.contains(chrome), "{chrome}");
    }
}

#[tokio::test]
async fn role_page_shows_sources_identity_stats_quirks_and_decisions() {
    use crate::action::{Expectation, Field, Outcome, Verdict, Via};
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    assert_eq!(
        h.get("/devices/nope", Some(&cookie)).await.status(),
        StatusCode::NOT_FOUND
    );
    let mut snapshot = h.state.snapshot();
    registry_bus(&mut snapshot);
    snapshot.ha.media_players[0].state = "playing".to_owned();
    snapshot.recent_frames = vec![frame(4, 0, 0x90, "Report Power Status")].into();
    let tv_on = Expectation {
        subject: "tv".to_owned(),
        field: Field::Power,
        expect: "on".to_owned(),
        via: vec![Via::TvDirect],
        deadline_ms: 45_000,
        attempts: 0,
        outcome: Outcome::Met {
            via: Via::TvDirect,
            observed: "on".to_owned(),
            latency_ms: 7_400,
        },
    };
    snapshot.decisions = vec![DecisionView {
        id: 40,
        role: Some("streamer".to_owned()),
        from: Some(RolePower::Off),
        to: Some(RolePower::On),
        reason: "ActiveSource".to_owned(),
        would: false,
        expectations: vec![tv_on],
        verdict: Verdict::Achieved,
        ..shadow_decision()
    }]
    .into();
    h.state.publish(snapshot, []);
    let html = ok_page(&h, &cookie, "/devices/streamer").await;
    for expected in [
        // Power, by source: CEC standby is used; Home Assistant says playing.
        "CEC · bus · used",
        "last frame 4 → 0 · 90 21 00 Report Power Status",
        "media_player.test_streamer",
        ">disagree<",
        // Identity against the rule.
        "4 · playback device 1",
        ">matches rule<",
        "match = { osd = &#34;Streamer&#34; }",
        "live Report Physical Address matched the stored 2.1.0.0",
        ">verified<",
        // How it behaves.
        "1 / 1 achieved",
        "p50 7.4 s · p95 hidden",
        // Quirks from the profile.
        "<code>takeover = cec_standby</code>",
        "Edit in Config →",
        // Decisions involving it.
        "triggered: off → on",
        "href=\"/recorder?role=streamer\"",
        // The same commands as before.
        "hx-post=\"/actions/identify/4\"",
        "hx-post=\"/actions/takeover/streamer\"",
        "hx-post=\"/actions/forget/4\"",
        "hx-confirm=\"Forget Test Streamer at LA 4? It will be re-learned if it talks again.\"",
    ] {
        assert!(html.contains(expected), "role page lacks {expected}");
    }
    // Shadow (the test config): a would take-over without confirmation.
    assert!(html.contains("Would take over…"));
    assert!(!html.contains("hx-confirm=\"Take over the room"));
}

#[tokio::test]
async fn restored_role_waits_for_its_device_with_identify_now() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let mut snapshot = h.state.snapshot();
    // The console's rule is `physical = "2.3.0.0"`: the state file brought LA 11 back.
    let mut stored = bound_entry(11, "2.3.0.0", "Console", "console");
    stored.verified = false;
    stored.matched_role = None;
    stored.power = BusPower::Unknown;
    snapshot.bus = vec![stored];
    snapshot.roles[1].restored = true;
    h.state.publish(snapshot, []);

    let registry = ok_page(&h, &cookie, "/devices").await;
    let row = row_containing(&registry, "href=\"/devices/console\"");
    assert!(row.contains(">restored<") && row.contains("before restart"));
    assert!(
        row.contains("11 · 2.3.0.0"),
        "keys of the entry it waits on"
    );
    assert!(
        !registry.contains("href=\"/devices/la/11\""),
        "not an unbound entry"
    );
    assert!(registry.contains("identity came from the state file after the"));

    let html = ok_page(&h, &cookie, "/devices/console").await;
    for expected in [
        "Restored, waiting for the device to talk",
        "id=\"identify-now\" class=\"btn sm\" hx-post=\"/actions/identify/11\"",
        "LA 11 · 2.3.0.0",
        "nothing on the bus",
        "forgotten after 30 days unheard",
    ] {
        assert!(html.contains(expected), "restored page lacks {expected}");
    }
    assert!(
        !html.contains("hx-post=\"/actions/forget/"),
        "forget is for bound roles"
    );
    let entry = h.get("/devices/la/11", Some(&cookie)).await;
    assert_eq!(entry.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&entry), "/devices/console");
}

#[tokio::test]
async fn unbound_entry_page_offers_identify_and_forget_and_others_redirect() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let mut snapshot = h.state.snapshot();
    registry_bus(&mut snapshot);
    snapshot.recent_frames = vec![frame(3, 0, 0x87, "Device Vendor ID")].into();
    h.state.publish(snapshot, []);
    let html = ok_page(&h, &cookie, "/devices/la/3").await;
    assert!(html.contains("hx-post=\"/actions/identify/3\""));
    assert!(html.contains(
        "hx-post=\"/actions/forget/3\" hx-swap=\"none\" hx-confirm=\"Forget LA 3? It will be re-learned if it talks again.\""
    ));
    assert!(html.contains("Device Vendor ID"), "its recent frames");
    let unidentified = ok_page(&h, &cookie, "/devices/la/9").await;
    assert!(unidentified.contains("next query in 4 m"));
    for (path, to) in [
        ("/devices/la/4", "/devices/streamer"),
        ("/devices/la/0", "/devices/out/bridge"),
        ("/devices/la/5", "/devices/out/receiver"),
    ] {
        let response = h.get(path, Some(&cookie)).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "{path}");
        assert_eq!(location(&response), to, "{path}");
    }
    for missing in ["/devices/la/12", "/devices/la/x", "/devices/out/nope"] {
        assert_eq!(
            h.get(missing, Some(&cookie)).await.status(),
            StatusCode::NOT_FOUND,
            "{missing}"
        );
    }
    let bridge = ok_page(&h, &cookie, "/devices/out/bridge").await;
    assert!(
        bridge.contains(
            "id=\"bridge-page-info\" class=\"pb-flow\" hx-get=\"/devices/out/bridge\" hx-trigger=\"every 3s\" hx-select=\"#bridge-page-info\""
        ),
        "the page polls its own wrapper"
    );
    // The drawer's ids stay free, so opening it here never duplicates one.
    assert!(!bridge.contains("id=\"bridge-info\""));
    assert!(
        !bridge.contains("/actions/send-raw"),
        "raw send stays in the drawer"
    );
}

#[tokio::test]
async fn receiver_page_identifies_and_forgets_its_audio_system_entry() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let mut snapshot = h.state.snapshot();
    registry_bus(&mut snapshot);
    snapshot.warming = false;
    h.state.publish(snapshot.clone(), []);
    let html = ok_page(&h, &cookie, "/devices/out/receiver").await;
    assert!(html.contains("hx-post=\"/actions/identify/5\" hx-swap=\"none\">Identify<"));
    assert!(html.contains(
        "hx-post=\"/actions/forget/5\" hx-swap=\"none\" hx-confirm=\"Forget the receiver's CEC entry at LA 5? It will be re-learned if it talks again.\""
    ));
    snapshot.warming = true;
    h.state.publish(snapshot.clone(), []);
    let html = ok_page(&h, &cookie, "/devices/out/receiver").await;
    assert!(html.contains(
        "hx-post=\"/actions/identify/5\" hx-swap=\"none\" disabled title=\"warming: actions are refused until the bus model settles\""
    ));
    // No audio system on the bus: nothing to identify or forget.
    snapshot.bus.retain(|entry| entry.logical_address != 5);
    h.state.publish(snapshot, []);
    let html = ok_page(&h, &cookie, "/devices/out/receiver").await;
    assert!(!html.contains("hx-post=\"/actions/identify/"));
    assert!(!html.contains("hx-post=\"/actions/forget/"));
}

#[tokio::test]
async fn role_page_renders_each_section_once() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let mut snapshot = h.state.snapshot();
    registry_bus(&mut snapshot);
    h.state.publish(snapshot, []);
    let html = ok_page(&h, &cookie, "/devices/streamer").await;
    for once in [
        "<h2 class=\"grow\">Identity</h2>",
        "<h2 class=\"grow\">Identity events</h2>",
        "class=\"tbl events\"",
        "<h2 class=\"grow\">Quirks</h2>",
        "class=\"pb quirks\"",
        "<h2 class=\"grow\">Decisions involving Test Streamer</h2>",
        "class=\"tbl involved\"",
        "href=\"/config\">Edit in Config →",
    ] {
        assert_eq!(html.matches(once).count(), 1, "{once}");
    }
    assert!(!html.contains("phone-only"), "no copy for the other width");
}

/// A drift on `tv.power` that opened at `since` and resolved 2 min 9 s later.
fn tv_drift_pair(since: chrono::DateTime<chrono::Utc>) -> [DecisionView; 2] {
    let detail = |observed: &str, via| DriftDetail {
        subject: "tv".to_owned(),
        observed: observed.to_owned(),
        via,
        ..drift_detail("off", since)
    };
    [
        drift_decision(
            52,
            "drift",
            chrono::TimeDelta::seconds(30),
            detail("off", crate::action::Via::Ha),
        ),
        drift_decision(
            53,
            "drift_resolved",
            chrono::TimeDelta::seconds(129),
            detail("on", crate::action::Via::TvDirect),
        ),
    ]
}

#[tokio::test]
async fn tv_page_compares_webos_with_home_assistant_and_pairs_drift_rows() {
    let text = config_text(UNREACHABLE_ISSUER, PUBLIC_URL).replace(
        "port = 9 }\n",
        "port = 9 }\noff = { kind = \"webos\", host = \"tv.test\", key_env = \"CHOREGOS_TV_KEY\" }\n",
    );
    let h = harness_from(text);
    let (cookie, _) = h.login().await;
    let now = chrono::Utc::now();
    let mut snapshot = h.state.snapshot();
    snapshot.mode = crate::state::Mode::Active;
    snapshot.tv_direct = Some((crate::state::DirectPower::On, now));
    snapshot.ha.tv.state = "unavailable".to_owned();
    snapshot.decisions = tv_drift_pair(now - chrono::TimeDelta::hours(1)).into();
    h.state.publish(snapshot.clone(), []);
    let html = ok_page(&h, &cookie, "/devices/out/tv").await;
    for expected in [
        "webOS at tv.test",
        "webOS · direct · used",
        ">disagree<",
        "Home Assistant reconnects to the TV about 30 s after a wake.",
        "<code>CHOREGOS_TV_KEY</code>",
        "Wake → picture, Test Streamer",
        "href=\"/recorder/52\"",
        "href=\"/recorder/53\"",
        "on via tv_direct",
        "2m 9s",
    ] {
        assert!(html.contains(expected), "TV page lacks {expected}");
    }
    snapshot.ha.tv.state = "on".to_owned();
    h.state.publish(snapshot, []);
    let html = ok_page(&h, &cookie, "/devices/out/tv").await;
    assert!(html.contains(">agree<"));
    assert!(!html.contains("Home Assistant reconnects"));
}

#[tokio::test]
async fn receiver_page_times_power_and_source_expectations() {
    use crate::action::{Expectation, Field, Outcome, Verdict, Via};
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let receiver = |field, expect: &str, latency_ms| Expectation {
        subject: "receiver".to_owned(),
        field,
        expect: expect.to_owned(),
        via: vec![Via::Ha],
        deadline_ms: 10_000,
        attempts: 0,
        outcome: Outcome::Met {
            via: Via::Ha,
            observed: expect.to_owned(),
            latency_ms,
        },
    };
    let row = |id, expectations| DecisionView {
        id,
        would: false,
        expectations,
        verdict: Verdict::Achieved,
        ..shadow_decision()
    };
    let mut snapshot = h.state.snapshot();
    snapshot.decisions = vec![
        row(60, vec![receiver(Field::Power, "on", 2_000)]),
        row(61, vec![receiver(Field::Power, "off", 5_000)]),
        row(62, vec![receiver(Field::Source, "Streamer Input", 52)]),
    ]
    .into();
    h.state.publish(snapshot, []);
    let html = ok_page(&h, &cookie, "/devices/out/receiver").await;
    let measure = |name: &str| row_containing(&html, &format!("<td>{name}</td>")).to_owned();
    assert!(measure("Turn on → on").contains("p50 2.0 s"));
    assert!(measure("Turn off → off").contains("p50 5.0 s · p95 hidden"));
    assert!(measure("Source switch").contains("p50 52 ms"));
    assert!(html.contains("No audio-system device has talked on the bus."));
    assert!(html.contains("<code>select_source = true</code>"));
}

#[tokio::test]
async fn pages_render_and_reference_served_assets() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    for path in [
        "/recorder/live?la=4&opcode=82",
        "/devices",
        "/p/devices",
        "/devices/streamer",
        "/devices/console",
        "/devices/out/tv",
        "/devices/out/receiver",
        "/devices/out/speakers",
        "/devices/out/bridge",
        "/recorder",
        "/recorder/glossary",
        "/recorder/timeline",
        "/p/timeline",
        "/config",
        "/config?tab=raw",
        "/p/cockpit",
        "/p/status",
        "/p/bridge",
    ] {
        let response = h.get(path, Some(&cookie)).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
    }
    for (section, _) in crate::web::view::config::SECTIONS {
        for path in [
            format!("/config?section={section}"),
            format!("/p/config/{section}"),
        ] {
            let response = h.get(&path, Some(&cookie)).await;
            assert_eq!(response.status(), StatusCode::OK, "{path}");
        }
    }
    for bad in [
        "/recorder/live?la=99",
        "/recorder?group=nap",
        "/recorder?before=x",
    ] {
        assert_eq!(
            h.get(bad, Some(&cookie)).await.status(),
            StatusCode::BAD_REQUEST,
            "{bad}"
        );
    }
    // Every page loads the stylesheet and htmx; Recorder pages add the recorder script.
    for (page, count) in [("/", 3), ("/recorder", 4)] {
        let html = body(h.get(page, Some(&cookie)).await).await;
        let assets: Vec<&str> = html
            .match_indices("/static/")
            .map(|(start, _)| between(&html[start..], "", "\""))
            .collect();
        assert_eq!(assets.len(), count, "{page}");
        for asset in assets {
            assert_eq!(h.get(asset, None).await.status(), StatusCode::OK, "{asset}");
        }
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
    let page = body(h.get("/recorder/live", Some(&cookie)).await).await;

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
    let response = h.get("/recorder/live/events", Some(&cookie)).await;
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

/// The body of a page that must render.
async fn ok_page(h: &Harness, cookie: &str, path: &str) -> String {
    let response = h.get(path, Some(cookie)).await;
    assert_eq!(response.status(), StatusCode::OK, "{path}");
    body(response).await
}

/// Ids of the list rows in page order.
fn row_ids(html: &str) -> Vec<u64> {
    html.match_indices("<tr id=\"row-")
        .map(|(at, marker)| {
            between(&html[at + marker.len()..], "", "\"")
                .parse()
                .expect("numeric row id")
        })
        .collect()
}

/// A ring covering the list's filters: migrated history, a drift onset and resolution,
/// a shadow `would` row and a settled row that follows it up.
fn recorder_ring() -> Vec<DecisionView> {
    let since = chrono::Utc::now() - chrono::TimeDelta::minutes(30);
    let migrated = DecisionView {
        id: 20,
        trigger: crate::action::Trigger::Service {
            name: crate::action::ServiceKind::Migrated,
        },
        timestamp: since - chrono::TimeDelta::hours(3),
        ..verdict_decision()
    };
    let would = DecisionView {
        id: 22,
        timestamp: since + chrono::TimeDelta::minutes(1),
        ..shadow_decision()
    };
    let settled = DecisionView {
        id: 23,
        parent: Some(22),
        timestamp: since + chrono::TimeDelta::minutes(2),
        ..verdict_decision()
    };
    vec![
        migrated,
        drift_decision(
            21,
            "drift",
            chrono::TimeDelta::seconds(30),
            drift_detail("off", since),
        ),
        would,
        settled,
        drift_decision(
            24,
            "drift_resolved",
            chrono::TimeDelta::milliseconds(195_500),
            drift_detail("on", since),
        ),
    ]
}

#[tokio::test]
async fn recorder_lists_newest_first_hides_migrated_and_applies_each_filter() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let mut snapshot = h.state.snapshot();
    snapshot.decisions.extend(recorder_ring());
    h.state.publish(snapshot, []);
    let page = |path: &'static str| ok_page(&h, &cookie, path);

    let html = page("/recorder").await;
    assert_eq!(
        row_ids(&html),
        [24, 23, 22, 21],
        "newest first, migrated hidden"
    );
    // First page polls its results (rows, count and pager) so verdicts settle in place.
    assert!(html.contains(r##"<div id="recorder-results" hx-get="/p/recorder" hx-trigger="every 5s" hx-select="#recorder-results" hx-swap="outerHTML">"##));
    let would = row_containing(&html, "id=\"row-22\"");
    assert!(
        would.starts_with("<tr id=\"row-22\" class=\"would\""),
        "{would}"
    );
    assert!(
        would.contains("<span class=\"chip would\">would</span>"),
        "{would}"
    );
    let settled = row_containing(&html, "id=\"row-23\"");
    assert!(
        settled.contains("<span class=\"chip warn\">partial</span>"),
        "{settled}"
    );
    assert!(
        settled.contains("href=\"/recorder/22\""),
        "parent link: {settled}"
    );
    assert!(settled.contains("↳ 22"), "{settled}");
    assert!(
        settled.contains("command takeover streamer"),
        "trigger: {settled}"
    );
    assert!(settled.contains(">3.4 s<"), "latency: {settled}");
    let resolved = row_containing(&html, "id=\"row-24\"");
    assert!(resolved.contains("lasted 3m 15s"), "{resolved}");
    assert!(
        resolved.contains("<span class=\"grp\">drift</span>"),
        "{resolved}"
    );
    assert!(html.contains("Showing 4 of 5 decisions in memory"));

    assert_eq!(
        row_ids(&page("/recorder?migrated=show").await),
        [24, 23, 22, 21, 20]
    );
    assert_eq!(row_ids(&page("/recorder?role=room").await), [24, 21]);
    assert_eq!(row_ids(&page("/recorder?group=drift").await), [24, 21]);
    assert_eq!(row_ids(&page("/recorder?would=hide").await), [24, 23, 21]);
    assert_eq!(row_ids(&page("/recorder?reason=drift").await), [21]);
    let empty = page("/recorder?role=room&verdict=partial").await;
    assert!(row_ids(&empty).is_empty());
    assert!(empty.contains("No decisions match these filters"));
    // The poll keeps the same query.
    assert!(
        page("/recorder?role=room")
            .await
            .contains("hx-get=\"/p/recorder?role=room\"")
    );
    let rows = page("/p/recorder?role=room").await;
    assert!(rows.starts_with("<div id=\"recorder-results\""), "{rows}");
    assert_eq!(row_ids(&rows), [24, 21]);
    assert!(
        rows.contains("Showing 2 of 5 decisions in memory"),
        "{rows}"
    );
}

#[tokio::test]
async fn recorder_pages_by_cursor_and_only_the_first_page_polls() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let mut snapshot = h.state.snapshot();
    let start = chrono::Utc::now() - chrono::TimeDelta::hours(1);
    snapshot.decisions.extend((1..=130).map(|id| DecisionView {
        id,
        timestamp: start + chrono::TimeDelta::seconds(i64::try_from(id).unwrap()),
        ..verdict_decision()
    }));
    h.state.publish(snapshot, []);
    let first = body(h.get("/recorder", Some(&cookie)).await).await;
    let ids = row_ids(&first);
    assert_eq!((ids.len(), ids[0], ids[99]), (100, 130, 31));
    assert!(first.contains("href=\"/recorder?before=31\">Older →</a>"));
    assert!(!first.contains("← Newest"));
    let second = body(h.get("/recorder?before=31", Some(&cookie)).await).await;
    let ids = row_ids(&second);
    assert_eq!((ids.len(), ids[0], ids[29]), (30, 30, 1));
    assert!(!second.contains("Older →"));
    assert!(second.contains("href=\"/recorder\">← Newest</a>"));
    assert!(
        !second.contains("hx-get=\"/p/recorder"),
        "older pages do not poll"
    );
}

/// The `Older →` cursor of a list page, if any.
fn older_cursor(html: &str) -> Option<u64> {
    let at = html.find("\">Older →</a>")?;
    let link = &html[..at];
    link[link.rfind("before=")? + "before=".len()..]
        .parse()
        .ok()
}

#[tokio::test]
async fn recorder_poll_refreshes_rows_and_pager_from_one_snapshot() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let start = chrono::Utc::now() - chrono::TimeDelta::hours(1);
    let decision = |id: u64| DecisionView {
        id,
        timestamp: start + chrono::TimeDelta::seconds(i64::try_from(id).unwrap()),
        verdict: crate::action::Verdict::Pending,
        ..verdict_decision()
    };
    let mut snapshot = h.state.snapshot();
    snapshot.decisions.extend((1..=130).map(decision));
    h.state.publish(snapshot, []);
    let first = body(h.get("/recorder", Some(&cookie)).await).await;
    assert_eq!(older_cursor(&first), Some(31));

    // A decision arrives before the next poll: the polled block's Older cursor follows
    // its own last row, so decision 31 is still reachable.
    let mut snapshot = h.state.snapshot();
    snapshot.decisions.push_back(decision(131));
    h.state.publish(snapshot, []);
    let polled = body(h.get("/p/recorder", Some(&cookie)).await).await;
    assert!(
        polled.starts_with("<div id=\"recorder-results\""),
        "{polled}"
    );
    let ids = row_ids(&polled);
    assert_eq!((ids.len(), ids[0], ids[99]), (100, 131, 32));
    assert_eq!(older_cursor(&polled), Some(32));
    assert!(
        polled.contains("Showing 131 of 131 decisions"),
        "count refreshed"
    );
    let older = body(h.get("/recorder?before=32", Some(&cookie)).await).await;
    let mut seen = ids.clone();
    seen.extend(row_ids(&older));
    assert_eq!(
        seen,
        (1..=131).rev().collect::<Vec<_>>(),
        "no gap, no repeat"
    );

    // With a verdict filter, settlement removes a row from page one: the poll pulls the
    // next one up and moves the cursor with it, so page two does not repeat it.
    let pending = "/recorder?verdict=pending";
    assert_eq!(
        older_cursor(&body(h.get(pending, Some(&cookie)).await).await),
        Some(32)
    );
    let mut snapshot = h.state.snapshot();
    for row in snapshot.decisions.iter_mut().filter(|row| row.id == 100) {
        row.verdict = crate::action::Verdict::Achieved;
    }
    h.state.publish(snapshot, []);
    let polled = body(h.get("/p/recorder?verdict=pending", Some(&cookie)).await).await;
    let ids = row_ids(&polled);
    assert_eq!((ids.len(), ids[0], ids[99]), (100, 131, 31));
    assert!(!ids.contains(&100));
    assert_eq!(older_cursor(&polled), Some(31));
    assert!(polled.contains("Showing 130 of 131 decisions"), "{polled}");
    let older = body(
        h.get("/recorder?verdict=pending&before=31", Some(&cookie))
            .await,
    )
    .await;
    let mut seen = ids.clone();
    seen.extend(row_ids(&older));
    let expected: Vec<u64> = (1..=131).rev().filter(|id| *id != 100).collect();
    assert_eq!(seen, expected, "no gap, no repeat");
}

#[tokio::test]
async fn recorder_poll_keeps_fractional_custom_bounds() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let at: chrono::DateTime<chrono::Utc> = "2026-11-07T23:54:03.500Z".parse().unwrap();
    let mut snapshot = h.state.snapshot();
    snapshot.decisions.push_back(DecisionView {
        id: 844,
        timestamp: at,
        ..verdict_decision()
    });
    h.state.publish(snapshot, []);
    let page = body(
        h.get(
            "/recorder?range=custom&from=2026-11-07T23%3A54%3A02.000Z&to=2026-11-07T23%3A54%3A03.999Z",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(row_ids(&page), [844]);
    let poll = between(&page, "<div id=\"recorder-results\" hx-get=\"", "\"").replace("&#38;", "&");
    assert_eq!(
        poll,
        "/p/recorder?range=custom&from=2026-11-07T23%3A54%3A02Z&to=2026-11-07T23%3A54%3A03.999Z"
    );
    let polled = body(h.get(&poll, Some(&cookie)).await).await;
    assert_eq!(row_ids(&polled), [844], "the poll matches the same rows");
}

/// A wake by a frame, with one met and one missed expectation, actions, and a child.
fn frame_wake() -> Vec<DecisionView> {
    let wake = DecisionView {
        id: 30,
        trigger: crate::action::Trigger::Frame {
            from: 4,
            to: 15,
            data: "822100".to_owned(),
        },
        actions: vec![
            r#"{"kind":"wake_tv"}"#.to_owned(),
            r#"{"kind":"cec_send","to":8,"data":[54]}"#.to_owned(),
        ],
        ..verdict_decision()
    };
    let settled = DecisionView {
        id: 31,
        parent: Some(30),
        reason: "takeover_settled".to_owned(),
        verdict: crate::action::Verdict::Achieved,
        expectations: vec![],
        timestamp: wake.timestamp + chrono::TimeDelta::seconds(2),
        ..verdict_decision()
    };
    vec![wake, settled]
}

#[tokio::test]
async fn decision_detail_renders_trigger_actions_expectations_and_family() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    for missing in ["/recorder/30", "/recorder/abc", "/p/window/30"] {
        assert_eq!(
            h.get(missing, Some(&cookie)).await.status(),
            StatusCode::NOT_FOUND,
            "{missing}"
        );
    }
    let mut snapshot = h.state.snapshot();
    snapshot.decisions.extend(frame_wake());
    snapshot.decisions.push_back(shadow_decision());
    h.state.publish(snapshot, []);
    let response = h.get("/recorder/30", Some(&cookie)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert!(
        html.contains("<h1>Test Streamer woke; console_b taken over — partial</h1>"),
        "{html}"
    );
    assert!(html.contains("Decision #30 · wake"));
    let trigger = between(&html, "id=\"trigger\"", "</section>");
    assert!(
        trigger.contains("<span class=\"chip info\">frame</span>"),
        "{trigger}"
    );
    assert!(
        trigger.contains("<dd>4 · LA 4</dd>") && trigger.contains("<dd>15 · broadcast</dd>"),
        "{trigger}"
    );
    assert!(
        trigger.contains("Active Source") && trigger.contains("PA 2.1.0.0"),
        "{trigger}"
    );
    let actions = between(&html, "id=\"actions\"", "</section>");
    assert!(actions.contains("via configured tv.wake"), "{actions}");
    assert!(
        actions.contains("LA 8 · LA 8") && actions.contains("36 · Standby"),
        "{actions}"
    );
    assert_eq!(actions.matches(">sent</span>").count(), 2, "{actions}");
    let expectations = between(&html, "id=\"expectations\"", "</section>");
    assert!(
        expectations.contains("<strong>tv_direct</strong> <span class=\"dim\">ha</span>"),
        "{expectations}"
    );
    assert!(
        expectations.contains("<span class=\"chip ok\">met via tv_direct</span>"),
        "{expectations}"
    );
    assert!(expectations.contains(">3.4 s<"), "{expectations}");
    assert!(
        expectations.contains("role:console_b.power"),
        "{expectations}"
    );
    assert!(
        expectations.contains("<span class=\"chip bad\">missed · read on</span>"),
        "{expectations}"
    );
    let family = between(&html, "id=\"family\"", "</section>");
    assert!(family.contains("href=\"/recorder/31\""), "{family}");
    assert!(family.contains("takeover_settled"), "{family}");
    let neighbours = between(&html, "id=\"neighbours\"", "</section>");
    assert!(
        neighbours.contains("href=\"/recorder?role=streamer\">Other decisions for Test Streamer →"),
        "{neighbours}"
    );
    assert!(
        neighbours.contains("href=\"/recorder?reason=PowerOn\">Other PowerOn rows →"),
        "{neighbours}"
    );
    // The window loads after the page; next exists, previous does not.
    assert!(html.contains("hx-get=\"/p/window/30?from=-5"), "{html}");
    assert!(html.contains("href=\"/recorder/31\" rel=\"next\""));
    assert!(!html.contains("rel=\"prev\""));
    // The child names its parent.
    let child = body(h.get("/recorder/31", Some(&cookie)).await).await;
    assert!(between(&child, "id=\"family\"", "</section>").contains("href=\"/recorder/30\""));

    // A shadow row's suppressed action shows `would`, the identify query that ran does not.
    let shadow = body(h.get("/recorder/7", Some(&cookie)).await).await;
    let actions = between(&shadow, "id=\"actions\"", "</section>");
    assert!(
        !actions.contains("would: "),
        "the prefix becomes the status: {actions}"
    );
    let suppressed = row_containing(actions, "media_player.turn_on media_player.test_receiver");
    assert!(
        suppressed.contains("<span class=\"chip would\">would</span>"),
        "{suppressed}"
    );
    let identify = row_containing(actions, "cec.give_osd_name 4");
    assert!(identify.contains(">sent</span>"), "{identify}");
    assert!(shadow.contains("recorded as would"));
}

/// A frame in the memory ring, `offset_ms` after `at`.
fn wire(
    at: chrono::DateTime<chrono::Utc>,
    offset_ms: i64,
    direction: FrameDirection,
    (from, to): (u8, u8),
    bytes: &[u8],
) -> FrameView {
    FrameView {
        timestamp: at + chrono::TimeDelta::milliseconds(offset_ms),
        direction,
        opcode: bytes.first().copied(),
        params: bytes.get(1..).unwrap_or_default().to_vec(),
        data: bytes.iter().map(|byte| format!("{byte:02X}")).collect(),
        ack: Some(true),
        ..frame(from, to, 0, "")
    }
}

#[tokio::test]
async fn bus_window_from_memory_tags_trigger_ours_and_replies() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let mut snapshot = h.state.snapshot();
    let ring = frame_wake();
    let at = ring[0].timestamp;
    snapshot.recent_frames.extend([
        wire(
            at,
            -60_000,
            FrameDirection::Received,
            (4, 15),
            &[0x87, 0x00, 0x10, 0xFA],
        ),
        wire(at, -1_000, FrameDirection::Received, (4, 4), &[]),
        wire(
            at,
            5,
            FrameDirection::Received,
            (4, 15),
            &[0x82, 0x21, 0x00],
        ),
        wire(at, 500, FrameDirection::Received, (5, 4), &[0x46]),
        wire(at, 1_000, FrameDirection::Sent, (0, 8), &[0x8F]),
        wire(at, 1_300, FrameDirection::Received, (8, 0), &[0x90, 0x01]),
    ]);
    snapshot.decisions.extend(ring);
    h.state.publish(snapshot, []);
    for bad in ["?from=-301", "?to=601", "?from=5", "?to=-1"] {
        assert_eq!(
            h.get(&format!("/p/window/30{bad}"), Some(&cookie))
                .await
                .status(),
            StatusCode::BAD_REQUEST,
            "{bad}"
        );
    }
    let html = body(h.get("/p/window/30", Some(&cookie)).await).await;
    let tag = |text: &str| {
        let row = row_containing(&html, text);
        between(row, "<span class=\"chip ", "</span>").to_owned()
    };
    assert_eq!(tag("82 21 00"), "info\">trigger");
    assert_eq!(tag("Give OSD Name"), "dim\">other");
    assert_eq!(tag("Give Device Power Status"), "ours\">ours");
    assert_eq!(tag("Report Power Status: standby"), "ok\">reply");
    assert!(!html.contains("87 00 10 FA"), "outside the window");
    // The decision and its settle row sit in the window as decision rows.
    assert!(html.contains("<a class=\"mono\" href=\"/recorder/30\">#30</a>"));
    assert!(html.contains("<tr class=\"wdec own\""));
    assert!(html.contains("href=\"/recorder/31\">#31</a> <span class=\"dim\">↳ 30</span>"));
    assert!(html.contains("Frames from memory (last ~6 h)"));
    assert!(html.contains("Widen to −30 s / +2 m"));
    assert!(html.contains("−5 s → +45 s"));
    assert!(html.contains(">poll<"), "polls are shown by default");
    let without_polls = body(
        h.get("/p/window/30?from=-5&to=45&set=1&obs=on", Some(&cookie))
            .await,
    )
    .await;
    assert!(
        !without_polls.contains(">poll<"),
        "unticked polls are hidden"
    );
    assert!(without_polls.contains("82 21 00"));
    let wide = body(h.get("/p/window/30?from=-120&to=45", Some(&cookie)).await).await;
    assert!(wide.contains("87 00 10 FA"));
    assert!(wide.contains("Back to −5 s / +45 s"));
}

#[tokio::test]
async fn bus_window_reads_the_trace_day_file_with_observations() {
    use crate::runtime::trace::{TraceEntry, day_file, line};
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let directory = tempfile::tempdir().unwrap();
    h.state.set_trace_directory(directory.path().to_path_buf());
    let mut snapshot = h.state.snapshot();
    let ring = frame_wake();
    let at = ring[0].timestamp;
    let ms = |offset| at + chrono::TimeDelta::milliseconds(offset);
    let mut text = String::new();
    for (ts, entry) in [
        (
            ms(-10),
            TraceEntry::Decision(
                serde_json::json!({"id": 30, "timestamp": at, "reason": "PowerOn", "role": "streamer", "verdict": "pending"}),
            ),
        ),
        (
            ms(5),
            TraceEntry::Received {
                bridge: "cec_bridge".to_owned(),
                frame: crate::cec::Frame::from_hex(4, 15, "822100").unwrap(),
            },
        ),
        (
            ms(1_000),
            TraceEntry::TvPower {
                state: crate::state::DirectPower::Unreachable,
                at: ms(1_000),
            },
        ),
        (
            ms(3_400),
            TraceEntry::TvPower {
                state: crate::state::DirectPower::On,
                at: ms(3_400),
            },
        ),
    ] {
        text.push_str(&line(ts, &entry));
    }
    let path = day_file(directory.path(), at.date_naive());
    std::fs::write(&path, text).unwrap();
    snapshot.decisions.extend(ring);
    h.state.publish(snapshot, []);
    let html = body(h.get("/p/window/30", Some(&cookie)).await).await;
    let file = path.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        html.contains(&format!("Frames from the trace file {file}.")),
        "{html}"
    );
    assert!(html.contains("tv/state unreachable · tv_direct"), "{html}");
    let settled = row_containing(&html, "tv/state on · tv_direct");
    assert!(
        settled.contains("<strong>— tv.power met (3.4 s)</strong>"),
        "{settled}"
    );
    assert!(row_containing(&html, "82 21 00").contains("trigger"));
    // The ring's verdict wins over the trace's creation-time one.
    assert!(row_containing(&html, "wdec own\"").contains("chip warn\">partial"));
    // Observations can be hidden.
    let quiet = body(h.get("/p/window/30?set=1&polls=on", Some(&cookie)).await).await;
    assert!(!quiet.contains("tv/state"));
}

#[tokio::test]
async fn bus_window_shows_its_decision_when_the_trace_lacks_the_creation_line() {
    use crate::runtime::trace::{TraceEntry, day_file, line};
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let directory = tempfile::tempdir().unwrap();
    h.state.set_trace_directory(directory.path().to_path_buf());
    let ring = frame_wake();
    let at = ring[0].timestamp;
    // Capture began after the decision was made: the day file has only a later sample.
    let path = day_file(directory.path(), at.date_naive());
    std::fs::write(
        &path,
        line(
            at + chrono::TimeDelta::seconds(1),
            &TraceEntry::TvPower {
                state: crate::state::DirectPower::On,
                at: at + chrono::TimeDelta::seconds(1),
            },
        ),
    )
    .unwrap();
    let mut snapshot = h.state.snapshot();
    // Memory holds a frame for the window; a valid trace window must not fall back to it.
    snapshot.recent_frames.push_back(wire(
        at,
        5,
        FrameDirection::Received,
        (4, 15),
        &[0x82, 0x21, 0x00],
    ));
    snapshot.decisions.extend(ring);
    h.state.publish(snapshot, []);
    let html = body(h.get("/p/window/30", Some(&cookie)).await).await;
    assert_eq!(html.matches("<tr class=\"wdec own\"").count(), 1, "{html}");
    assert!(
        row_containing(&html, "wdec own\"").contains("href=\"/recorder/30\">#30</a>"),
        "{html}"
    );
    assert!(
        !html.contains("82 21 00"),
        "no memory frames in a trace window"
    );
    let file = path.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        html.contains(&format!(
            "No frames for this window in the trace file {file}."
        )),
        "{html}"
    );
    assert!(html.contains("tv/state on · tv_direct"));
}

#[tokio::test]
async fn old_routes_are_gone_and_navigation_points_at_the_recorder() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    for gone in ["/bus", "/bus/events", "/decisions"] {
        assert_eq!(
            h.get(gone, Some(&cookie)).await.status(),
            StatusCode::NOT_FOUND,
            "{gone}"
        );
    }
    let mut snapshot = h.state.snapshot();
    snapshot.decisions.extend(frame_wake());
    h.state.publish(snapshot, []);
    let cockpit = body(h.get("/", Some(&cookie)).await).await;
    assert!(!cockpit.contains("/decisions") && !cockpit.contains("/bus"));
    assert!(
        cockpit.contains("<a href=\"/recorder\">Recorder</a>"),
        "{cockpit}"
    );
    assert!(cockpit.contains("href=\"/recorder\">Open Recorder →</a>"));
    assert!(cockpit.contains("<a href=\"/recorder/31\">31</a>"));
    let live = body(h.get("/recorder/live", Some(&cookie)).await).await;
    assert!(
        live.contains("href=\"/recorder/30\">#30</a>"),
        "decision rows open the detail"
    );
    let glossary = body(h.get("/recorder/glossary", Some(&cookie)).await).await;
    assert!(glossary.contains("<dt id=\"r-off_timer_expired\"><a class=\"mono\" href=\"/recorder?reason=off_timer_expired\">off_timer_expired</a></dt>"));
    assert_eq!(glossary.matches("<dt id=\"r-").count(), 38);
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
async fn live_stream_resyncs_from_history_then_streams_filtered_rows() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let mut snapshot = h.state.snapshot();
    let history = frame(4, 15, 0x82, "<Active Source>");
    snapshot.recent_frames.extend([history.clone()]);
    h.state.publish(snapshot.clone(), []);
    let page = body(h.get("/recorder/live", Some(&cookie)).await).await;
    assert!(page.contains("&#60;Active Source&#62;") || page.contains("&lt;Active Source&gt;"));
    assert!(page.contains("21 00"));
    assert!(page.contains("sse-connect=\"/recorder/live/events\""));
    // Follow off: the same rows, no stream.
    let frozen = body(h.get("/recorder/live?follow=off", Some(&cookie)).await).await;
    assert!(frozen.contains("21 00"));
    assert!(
        !frozen.contains("sse-connect"),
        "follow off renders no stream"
    );
    assert!(
        frozen.contains("href=\"/recorder/live\""),
        "follow on is a link back to now"
    );
    assert!(page.contains("href=\"/recorder/live?follow=off\""));

    let response = h.get("/recorder/live/events?la=4", Some(&cookie)).await;
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
async fn live_stream_re_renders_the_ring_when_a_verdict_settles() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let response = h.get("/recorder/live/events", Some(&cookie)).await;
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

fn iso(at: chrono::DateTime<chrono::Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// The bus-window memory fixture (frame_wake plus frames around it), published.
fn timeline_snapshot(h: &Harness) -> chrono::DateTime<chrono::Utc> {
    let mut snapshot = h.state.snapshot();
    let ring = frame_wake();
    let at = ring[0].timestamp;
    snapshot.recent_frames.extend([
        wire(at, -1_000, FrameDirection::Sent, (0, 4), &[]),
        wire(
            at,
            5,
            FrameDirection::Received,
            (4, 15),
            &[0x82, 0x21, 0x00],
        ),
        wire(at, 500, FrameDirection::Received, (5, 4), &[0x46]),
        wire(at, 1_000, FrameDirection::Sent, (0, 8), &[0x8F]),
        // Outside a one-minute window around the wake.
        wire(at, -120_000, FrameDirection::Received, (5, 15), &[0x87]),
    ]);
    snapshot.decisions.extend(ring);
    h.state.publish(snapshot, []);
    at
}

#[tokio::test]
async fn timeline_renders_lanes_at_percent_positions_from_memory() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let at = timeline_snapshot(&h);
    // The TV's current sample, taken before the window: memory's only observation.
    let mut snapshot = h.state.snapshot();
    snapshot.tv_direct = Some((
        crate::state::DirectPower::On,
        at - chrono::TimeDelta::seconds(10),
    ));
    h.state.publish(snapshot, []);
    let html = ok_page(
        &h,
        &cookie,
        &format!("/recorder/timeline?at={}&zoom=1m", iso(at)),
    )
    .await;
    let lanes = between(&html, "<div class=\"lanes\">", "<p class=\"legend");
    // Lanes per address: the bridge's own sends, the poll in its target's lane, senders.
    for la in ["0", "4", "5"] {
        assert!(
            lanes.contains(&format!("data-la=\"{la}\"")),
            "lane {la}: {lanes}"
        );
    }
    assert!(!lanes.contains("data-la=\"15\""), "broadcast is no lane");
    // The wake sits in the middle, its trigger tick too; the minute-old frame is outside.
    assert!(
        lanes.contains("<rect class=\"tk trigger\" x=\"50.01%\""),
        "{lanes}"
    );
    assert_eq!(lanes.matches("data-seq=").count(), 4, "{lanes}");
    assert!(lanes.contains("data-id=\"30\"") && lanes.contains("data-id=\"31\""));
    assert!(lanes.contains("aria-label=\"Test Streamer woke; console_b taken over — partial\""));
    assert!(html.contains("frames from memory · <span id=\"tl-n-frames\">4</span> frames"));
    // Each address and observation track is an image named by its summary, with its
    // frames or runs as a visually hidden text list.
    let lane = between(lanes, "data-la=\"4\">", "</div>");
    assert!(
        lane.contains("role=\"img\" aria-label=\"LA 4 · 2 frames · trigger at :"),
        "{lane}"
    );
    let list = between(lane, "<ul class=\"sr-only\">", "</ul>");
    assert_eq!(list.matches("<li>").count(), 2, "{list}");
    assert!(list.contains(" 0→4 poll</li>"), "{list}");
    assert!(
        list.contains(" 4→15 82 Active Source · trigger of #30</li>"),
        "{list}"
    );
    let tv = between(lanes, "data-obs=\"tv\">", "</div>");
    assert!(
        tv.contains("role=\"img\" aria-label=\"tv · webos: on\""),
        "{tv}"
    );
    assert!(tv.contains("<ul class=\"sr-only\"><li>on · "), "{tv}");
    // Every x is a percentage in [0, 100].
    for (start, _) in lanes.match_indices(" x=\"") {
        let value = between(&lanes[start..], " x=\"", "\"");
        if let Some(percent) = value.strip_suffix('%') {
            let percent: f64 = percent.parse().expect("numeric percentage");
            assert!((0.0..=100.0).contains(&percent), "{value}");
        }
    }
    // Nothing selected: the panel is an empty element. The phone note is rendered too.
    assert!(html.contains("aria-label=\"Selected decision\"></aside>"));
    assert!(html.contains("<h1>The timeline is desk only</h1>"));
    assert!(!html.contains("Open decision #"));
    // The Timeline tab is live and current; no inline styles anywhere.
    assert!(html.contains("<a href=\"/recorder/timeline\" aria-current=\"page\">Timeline</a>"));
    assert!(!html.contains("style="));
    // The live-tail toggle needs the script.
    assert!(html.contains(
        "title=\"needs the recorder script\"><input type=\"checkbox\" id=\"tl-tail\" disabled>"
    ));
}

#[tokio::test]
async fn timeline_select_opens_the_panel_and_centres_on_the_decision() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let at = timeline_snapshot(&h);
    let html = ok_page(&h, &cookie, "/recorder/timeline?select=30").await;
    let half = chrono::TimeDelta::seconds(150);
    assert!(
        html.contains(&format!(
            "data-from=\"{}\" data-to=\"{}\" data-zoom=\"5m\" data-at=\"{}\"",
            iso(at - half),
            iso(at + half),
            iso(at)
        )),
        "{html}"
    );
    let panel = between(&html, "<aside id=\"selected\"", "</aside>");
    assert!(
        panel.contains("#30") && panel.contains("chip warn\">partial"),
        "{panel}"
    );
    assert!(panel.contains("href=\"/recorder/30\">Open decision</a>"));
    assert!(panel.contains("child <a class=\"mono\" href=\"/recorder/31\">#31</a>"));
    assert!(panel.contains("tv.power on"), "{panel}");
    assert!(html.contains("<a class=\"mk warn selected\" href=\"/recorder/30\""));
    // The trigger's address lane is highlighted; zoom and pan keep the selection.
    assert!(html.contains("<div class=\"lane addr focus\" data-la=\"4\">"));
    let zoom = between(&html, "data-zoom=\"1m\" href=\"", "\"").replace("&#38;", "&");
    assert!(zoom.ends_with("zoom=1m&select=30"), "{zoom}");
    // `at=now` keeps the live edge with the selection, and its zoom links keep it too.
    let edge = ok_page(&h, &cookie, "/p/timeline?at=now&zoom=1m&select=30").await;
    assert!(edge.contains("data-zoom=\"1m\" data-at=\"\""), "{edge}");
    assert!(edge.contains("<a class=\"mk warn selected\" href=\"/recorder/30\""));
    let zoom = between(&edge, "data-zoom=\"5m\" href=\"", "\"").replace("&#38;", "&");
    assert_eq!(zoom, "/recorder/timeline?at=now&zoom=5m&select=30");
    assert!(
        html.contains("Open decision #30</a>"),
        "the phone note links the decision"
    );
    // The panel partial alone, for the [ and ] keys.
    let partial = ok_page(&h, &cookie, "/p/timeline/select/31?zoom=1m").await;
    assert!(partial.starts_with("<aside id=\"selected\""));
    assert!(partial.contains("href=\"/recorder/31\">Open decision</a>"));
    let close = between(&partial, "<a class=\"btn sm ghost\" href=\"", "\"");
    assert_eq!(close, "/recorder/timeline?zoom=1m");
    // The decision page links here.
    let detail = ok_page(&h, &cookie, "/recorder/30").await;
    assert!(detail.contains("href=\"/recorder/timeline?select=30\">Open in timeline →</a>"));
}

#[tokio::test]
async fn timeline_query_is_validated_and_bounded() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let at = timeline_snapshot(&h);
    for (path, status) in [
        ("/recorder/timeline?zoom=2h", StatusCode::BAD_REQUEST),
        ("/recorder/timeline?zoom=90s", StatusCode::BAD_REQUEST),
        ("/p/timeline?at=yesterday", StatusCode::BAD_REQUEST),
        ("/recorder/timeline?select=x", StatusCode::BAD_REQUEST),
        ("/recorder/timeline?select=999", StatusCode::NOT_FOUND),
        ("/p/timeline/select/999", StatusCode::NOT_FOUND),
        ("/p/timeline/window.json", StatusCode::BAD_REQUEST),
        (
            "/p/timeline/window.json?from=x&to=y",
            StatusCode::BAD_REQUEST,
        ),
    ] {
        assert_eq!(h.get(path, Some(&cookie)).await.status(), status, "{path}");
    }
    let json = |from, to| format!("/p/timeline/window.json?from={}&to={}", iso(from), iso(to));
    let hour = chrono::TimeDelta::hours(1);
    for bad in [
        json(at, at),
        json(at, at - hour),
        json(at, at + hour + chrono::TimeDelta::milliseconds(1)),
    ] {
        assert_eq!(
            h.get(&bad, Some(&cookie)).await.status(),
            StatusCode::BAD_REQUEST,
            "{bad}"
        );
    }
    assert_eq!(
        h.get(&json(at, at + hour), Some(&cookie)).await.status(),
        StatusCode::OK
    );
    // Every zoom renders; the window never runs past now on the live edge.
    for zoom in ["1m", "5m", "15m", "1h"] {
        let html = ok_page(&h, &cookie, &format!("/p/timeline?zoom={zoom}")).await;
        assert!(html.starts_with("<div id=\"timeline\""), "{zoom}");
        assert!(
            html.contains("title=\"The window already ends now\""),
            "{zoom}"
        );
    }
    assert_eq!(
        h.get("/p/timeline/window.json?from=x", None).await.status(),
        StatusCode::SEE_OTHER,
        "the JSON is behind the login"
    );
}

#[tokio::test]
async fn timeline_json_returns_the_frames_the_partial_draws() {
    let h = harness(UNREACHABLE_ISSUER);
    let (cookie, _) = h.login().await;
    let at = timeline_snapshot(&h);
    let half = chrono::TimeDelta::seconds(30);
    let html = ok_page(&h, &cookie, &format!("/p/timeline?at={}&zoom=1m", iso(at))).await;
    let response = h
        .get(
            &format!(
                "/p/timeline/window.json?from={}&to={}",
                iso(at - half),
                iso(at + half)
            ),
            Some(&cookie),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
    let json: serde_json::Value = serde_json::from_str(&body(response).await).unwrap();
    assert_eq!(json["source"], "memory");
    let mut drawn: Vec<u64> = html
        .match_indices("data-seq=\"")
        .map(|(start, _)| {
            between(&html[start..], "data-seq=\"", "\"")
                .parse()
                .unwrap()
        })
        .collect();
    drawn.sort_unstable();
    let entries = json["entries"].as_array().unwrap();
    let frames: Vec<u64> = (0u64..)
        .zip(entries)
        .filter(|(_, entry)| entry["kind"] == "frame")
        .map(|(seq, _)| seq)
        .collect();
    assert_eq!(drawn, frames);
    let mut ticks: Vec<u64> = json["lanes"]["addresses"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|lane| lane["ticks"].as_array().unwrap().clone())
        .map(|tick| tick["seq"].as_u64().unwrap())
        .collect();
    ticks.sort_unstable();
    assert_eq!(ticks, frames);
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry["kind"] == "decision")
            .count(),
        2
    );
}

#[tokio::test]
async fn versioned_scripts_are_cached_immutably_and_old_versions_are_gone() {
    let h = harness(UNREACHABLE_ISSUER);
    for asset in ["/static/recorder-2.js", "/static/htmx-2.0.11.min.js"] {
        let response = h.get(asset, None).await;
        assert_eq!(response.status(), StatusCode::OK, "{asset}");
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable",
            "{asset}"
        );
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/javascript; charset=utf-8"
        );
    }
    // A cached copy never goes stale: a changed script is a new name, the old one is gone.
    for gone in ["/static/recorder-1.js", "/static/recorder.js"] {
        assert_eq!(
            h.get(gone, None).await.status(),
            StatusCode::NOT_FOUND,
            "{gone}"
        );
    }
}
