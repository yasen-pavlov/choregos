# Choregos — design contract

Choregos (χορηγός, the patron who ran the chorus at the Dionysia) is the
conductor of a living-room home theater: it watches the HDMI-CEC bus through a
Tasmota CEC bridge, keeps a model of who is on the bus and who is the active
source, and drives the non-CEC parts of the room (TV, receiver, Sonos, Kodi)
through Home Assistant so that every device can be used with its own remote.

This document is the contract between the implementation slices. It is
authoritative until superseded by code review decisions recorded in
`docs/decisions.md`.

## 1. The room (reference topology)

```
TV ──────────── HDMI ──── AV receiver (CEC audio system, LA 5, PA 2.0.0.0)
  CEC pin disconnected        │ in1 streaming box   (playback)
  WoL + vendor API via HA     │ in2 Kodi box        (recording device)
                              │ in3 console A       (playback; leaves the bus entirely in standby)
                              │ in4 console B       (playback)
                              │ in5 the bridge      (Tasmota HDMI-CEC device, claims the TV role, LA 0)
Sonos (line-in of a separate amp, not on HDMI)
home server: MQTT broker, Home Assistant (websocket), choregos (this)
```

The TV's CEC is physically disconnected: the bridge is the only "TV" on the
bus. Every CEC frame the bridge sees is published as
`tele/tasmota_cec/SENSOR` → `{"HdmiReceived":{"From":4,"To":0,"Data":"04"}}`
(`Data` = opcode + operands, uppercase hex, no separators; empty for polling
messages). The bridge publishes frames addressed to itself, to broadcast (15)
and (patched firmware) to the audio system (5). Choregos sends frames with
`cmnd/tasmota_cec/HdmiSend` → `{"To":<la>,"Data":"<hex>"}`; the bridge answers
`stat/tasmota_cec/RESULT` → `{"HdmiSend":"Done"|"Sending failed"}` (stock) or
`{"HdmiSend":{"Ack":bool}}` (patched).
`fixtures/capture-streamer-wake.jsonl` is a real capture (Kodi standby,
streaming box wake, standby, re-announce, remote volume presses) and is the
primary replay fixture.

## 2. Non-goals

- No receiver/TV/Sonos/Kodi protocol implementations: Home Assistant already
  has working integrations; choregos calls HA services.
- No persistence of bus state across restarts: the bus is re-interrogated on
  start (§5.6).
- No CEC frame decoding beyond what the engine needs (a name table for the UI
  is fine; no full CEC stack).
- No plugin system. The TV wake path is a two-variant enum, not a plugin.

## 3. Crate layout (single binary crate `choregos`)

```
src/main.rs            CLI (clap): run | check-config | replay <fixture>
src/config.rs          TOML config + env secrets, validation            [core]
src/cec/mod.rs         pub use                                           [core]
src/cec/frame.rs       Frame {from, to, opcode: Option<u8>, params: Vec<u8>}; parse HdmiReceived JSON; to_hdmisend() [core]
src/cec/opcode.rs      const names for the opcodes used + name lookup for the UI [core]
src/cec/bus.rs         BusModel: per-LA device table, active source, vendor/osd/physical learning [core]
src/identity.rs        role resolution (config match rules × bus table)  [core]
src/engine.rs          pure state machine: Event → Vec<Action> (+ timers) [core]
src/action.rs          Action enum + Decision log entry                  [core]
src/runtime/mod.rs     event loop: channels → engine → executor          [core]
src/runtime/mqtt.rs    rumqttc client; bridge topics; HA discovery; state topics [core]
src/runtime/ha.rs      HA websocket client: auth, call_service, state subscriptions, reconnect [core]
src/runtime/wol.rs     magic packet to directed broadcast                [core]
src/runtime/executor.rs Action → side effect (respects mode)             [core]
src/state.rs           AppState shared with the web layer (RwLock snapshot + broadcast channel of UI events) [core]
src/web/mod.rs         axum router, OIDC + local login, session cookie   [ui]
src/web/auth.rs                                                           [ui]
src/web/routes/*.rs    dashboard, bus, devices, decisions, config, actions, healthz, SSE [ui]
src/web/templates/     askama templates, htmx, one static CSS, no JS build [ui]
tests/replay.rs        fixture-driven engine tests                        [core]
fixtures/*.jsonl       captures                                           [lead]
```

Ownership: `[core]` = core implementer, `[ui]` = UI implementer. The boundary
is `src/state.rs` (owned by core, consumed by ui) and `src/web/mod.rs::router(state: AppState) -> axum::Router` (owned by ui, mounted by core's `main.rs`). Both are
written against the interfaces in §8; whoever needs a change to those
interfaces messages the other implementer first.

## 4. Configuration (`choregos.toml`)

```toml
mode = "shadow"            # shadow | active   (also switchable at runtime; persisted back to this file)

[mqtt]
host = "mqtt.lan"
port = 1883
username = "choregos"
# password: env CHOREGOS_MQTT_PASSWORD
client_id = "choregos"

[bridge]                  # the Tasmota CEC device
topic = "tasmota_cec"  # tele/<topic>/SENSOR, cmnd/<topic>/HdmiSend, stat/<topic>/RESULT, tele/<topic>/LWT
logical_address = 0       # what the bridge claims; frames To:<this> are "to the TV"

[home_assistant]
url = "http://homeassistant.lan:8123"
# token: env CHOREGOS_HA_TOKEN  (long-lived access token)

[tv]
entity = "media_player.tv"
wake = { kind = "wol", mac = "AA:BB:CC:DD:EE:FF", broadcast = "192.168.10.255", port = 9 }
# or: wake = { kind = "http", url = "http://wol-service.lan:8090/api/wake/<device-id>", header = "Authorization", token_env = "CHOREGOS_WAKE_TOKEN" }

[receiver]
entity = "media_player.receiver"
select_source = true      # call media_player.select_source on takeover (the receiver may also follow <Active Source> itself)

[presets]                 # HA scripts, chosen by local time
timezone = "Europe/Berlin"
[[presets.slot]]
from = "09:00"
to = "21:00"
script = "script.listening_day"
[[presets.slot]]
from = "21:00"
to = "24:00"
script = "script.listening_evening"
[[presets.slot]]
from = "00:00"
to = "09:00"
script = "script.listening_night"

[sonos]
entity = "media_player.sonos"
mute_switch = "switch.sonos_mute"
volume_from = ["streamer"]        # roles whose remote volume keys are forwarded while Sonos is playing

[timing]
off_delay_s = 3                   # last device off → TV/receiver off
takeover_settle_ms = 2000         # receiver turn_on → select_source → preset
presence_poll_s = 5               # for roles with presence = "poll"
presence_misses = 2
identify_retry_s = 10

[fake_tv]
power_status = "always_on"        # reply <Report Power Status: On> to <Give Device Power Status>
cec_version = "1.4"
osd_name = "Choregos"

[[device]]
role = "streamer"                 # unique id, also HA entity suffix
name = "Streaming box"
kind = "playback"
match = { vendor = "0010FA" }     # any listed key must match (osd > vendor > physical priority for conflicts)
source = "Streamer"               # receiver source name for select_source
on = ["image_view_on", "text_view_on", "active_source", "power_on"]
off = ["standby", "inactive_source", "power_standby"]
takeover = "cec_standby"          # what to do to this device when another one takes over: cec_standby | ha_media_stop | none
presence = "events"               # events | poll  (poll: send <Give Device Power Status> every presence_poll_s while on)

[[device]]
role = "kodi"
name = "Kodi box"
kind = "recording"
match = { osd = "Kodi" }
source = "Kodi"
on = ["image_view_on", "active_source"]
off = ["standby", "inactive_source"]
takeover = "ha_media_stop"
media_player = "media_player.kodi"
presence = "events"

[[device]]
role = "console_b"
name = "Console B"
kind = "playback"
match = { vendor = "080046" }      # Sony (unverified — confirm in shadow mode; fallback physical = "2.4.0.0")
source = "Console B"
on = ["image_view_on", "text_view_on", "active_source", "power_on"]
off = ["standby", "inactive_source", "power_standby"]
takeover = "cec_standby"
presence = "events"

[[device]]
role = "console_a"
name = "Console A"
kind = "playback"
match = { physical = "2.3.0.0" }   # vendor unknown until seen in shadow mode (receiver input 3); switch to vendor/osd once learned
source = "Console A"
on = ["report_physical_address", "image_view_on", "active_source"]
off = ["standby", "inactive_source"]
takeover = "cec_standby"
presence = "poll"

[web]
bind = "0.0.0.0:8095"
public_url = "https://choregos.example.com"
# session secret: env CHOREGOS_SESSION_SECRET (≥32 bytes)
[web.oidc]
issuer = "https://auth.example.com/application/o/choregos/"
client_id = "choregos"
# client secret: env CHOREGOS_OIDC_CLIENT_SECRET
[web.local_login]
username = "admin"
# password: env CHOREGOS_LOCAL_PASSWORD (break-glass; argon2-hashed in memory at startup)
```

Validation: unique roles, unique sources, at least one `match` key per device,
`presence = "poll"` requires `kind = "playback"`, time slots must cover 24 h
without overlap, `broadcast` is an IPv4, MAC parses. `choregos check-config`
prints the resolved config with secrets redacted.

## 5. Behaviour

### 5.1 Bus model
`BusModel` keeps one entry per logical address 0–14:
`{physical: Option<u16>, device_type: Option<u8>, vendor: Option<u32>, osd: Option<String>, power: Unknown|On|Standby|Transition, last_seen: Instant, identified: bool}`
plus `active_source: Option<u16>` (physical address) and the bridge's own LA.
Learning rules (all from received frames; sender = `From`):
- `84 PP PP TT` Report Physical Address (bcast) → physical, device_type, last_seen.
- `87 VV VV VV` Device Vendor ID (bcast) → vendor.
- `47 <ascii>` Set OSD Name → osd.
- `82 PP PP` Active Source (bcast) → active_source = PP PP, sender power = On.
- `9D PP PP` Inactive Source (to TV) → if active_source == PP PP then None; sender power = Standby.
- `36` Standby from a device (to TV or bcast) → sender power = Standby. (From the TV/bridge to others it is a command, not a report.)
- `90 SS` Report Power Status → sender power per SS (00 On, 01 Standby, 02 →On, 03 →Standby).
- `04` Image View On / `0D` Text View On (to TV) → sender power = On.
- Any frame from an LA whose entry is not `identified` and is not the bridge → emit `Action::Identify(la)` (rate-limited by `identify_retry_s`), which sends `83`, `8C`, `46` to that LA. `identified` = physical known and (osd or vendor known).
- A new `84` from an LA with a different physical address replaces the entry (the device moved or a different device took the LA).
- Entries age out of `power = On` only via explicit events or presence polling; never by time alone.

### 5.2 Identity → role
A role matches an LA entry when every key in its `match` table matches
(`osd` exact after trimming, `vendor` hex, `physical` dotted). If two roles
match one LA, or one role matches two LAs, the engine records a `Conflict`
decision, exposes it in the UI/HA, and takes **no power actions** for the
roles involved until resolved. Unmatched LAs appear as "unidentified" devices.

### 5.3 Role power state
Per role: `Off | On`, derived from the bus entry of the LA it currently maps
to, using the role's `on`/`off` event lists. State changes produce decisions;
duplicates (same state) are ignored (replaces today's `rbe` nodes).

### 5.4 Takeover (role turns On)
In order, as `Action`s:
1. `WakeTv` if `tv.entity` state is not `on` (HA state cache; if unknown, send anyway).
2. For every other role currently On: `takeover` action — `cec_standby` → `CecSend{to: la, data: "36"}`; `ha_media_stop` → `HaCall{media_player.media_stop, entity}`; `none`.
3. If Sonos state is `playing`: `HaCall{media_player.media_stop, sonos.entity}`.
4. `HaCall{media_player.turn_on, receiver}` if receiver not `on`; then after `takeover_settle_ms`: `HaCall{media_player.select_source, receiver, {source}}` if `select_source` and current receiver source ≠ `source`; then `HaCall{script.turn_on, preset_for(now)}` — the preset only runs when the receiver transitioned off→on in this takeover or the active role changed.
Takeovers are idempotent: a repeated On for the same role with no state change does nothing.

### 5.5 Release (role turns Off)
Start/restart `off_delay_s` timer. On expiry, if no role is On: `HaCall{media_player.turn_off, tv}` and `HaCall{media_player.turn_off, receiver}`. If a role turned On meanwhile the timer is cancelled.

### 5.6 Startup / resync
On start and on bridge LWT `Online`: send `85` (Request Active Source, bcast) and `8F` to LAs 1, 4, 8, 11 (and any LA in config `match.physical` tables); the replies populate the model within ~1 s. Until the first reply or 3 s, no power actions are taken (`Warming` state shown in UI).

### 5.7 Presence polling (`presence = "poll"`)
While the role is On: every `presence_poll_s` send `8F` to its LA. A `90 xx` reply (or any frame from that LA) resets the miss counter; `presence_misses` consecutive polls without any frame from the LA → role Off (with decision reason `presence_timeout`). Polling stops when Off.

### 5.8 Fake TV replies (frames addressed to the bridge LA)
`fake_tv.replies` defaults to `true`. Set it to `false` when bridge firmware handles directed `8F`/`9F`/`46`/`91` requests itself; the service still handles broadcast `85` replies. Never reply to unregistered senders (LA 15).
- `8F` → `90 00` (when `power_status = "always_on"`).
- `9F` → `9E 05` (1.4).
- `46` → `47 <osd_name>`.
- `85` (bcast) → `82 <active_source>` if known.
- `91` → `32 "eng"`.
Everything else: no reply (Feature Abort is a firmware concern).

### 5.9 Sonos volume forwarding
`44 41|42|43` (User Control Pressed: Volume Up/Down/Mute) from a role listed in `sonos.volume_from`, addressed to the bridge or to LA 5, while the Sonos entity state is `playing` → `HaCall{media_player.volume_up|volume_down, sonos}` / `HaCall{switch.toggle, mute_switch}`. `45` (release) is ignored. Not debounced (one step per press frame), as today.

### 5.10 Modes
`shadow`: the engine runs fully; the executor performs **only** read-only identification queries, startup resync and presence `8F` polls. Every other device action is logged as `would: …` with the full payload. `active`: everything executes. The HA `select.choregos_mode` entity and the UI switch the mode at runtime and persist it to the config file (atomic rename preserving comments/order).

### 5.11 HA exposure (MQTT discovery, retained)
Node id `choregos`, availability topic `choregos/status` (`online`/`offline` LWT). Entities: `binary_sensor.choregos_<role>` (power, per role), `sensor.choregos_active_source` (role name or `none`), `select.choregos_mode` (shadow/active), `button.choregos_all_off`, `binary_sensor.choregos_bridge` (bridge LWT), `sensor.choregos_conflicts` (count). Commands arrive on `choregos/cmd/<entity>`. State topics `choregos/state/<name>` retained.

### 5.12 Events into the engine
`Frame`, `BridgeOnline/Offline`, `HaState{entity, state, attrs}` (tv, receiver, sonos, kodi — subscribed via `subscribe_trigger`/`state_changed`), `Tick(now)`, `Command(AllOff | SetMode | ForceIdentify(la) | SendRaw{to,data} | ManualTakeover(role))`, `ConfigReloaded`.

## 6. Web UI (axum + askama + htmx + SSE; no JS build step)

Pages (all behind auth): 
- `/` Dashboard: mode switch (confirm when going active), bridge status, role cards (name, power, LA/PA, vendor/OSD, last event, "takeover" and "standby" buttons), TV/receiver/Sonos/Kodi HA states, active source, conflicts banner, "all off".
- `/bus` Live bus: SSE stream of frames (decoded: from/to names, opcode name, operands) interleaved with decisions; filters by LA/opcode; last 2000 kept in memory (ring buffer in `AppState`).
- `/devices` Bus table (every LA with learned fields, identified/unidentified, matched role) and the role → source mapping; "send identify" per LA.
- `/decisions` Decision log with reasons and resulting actions (and `would:` entries in shadow mode).
- `/config` Current TOML (secrets redacted), edit + validate + save (atomic), reload.
- `/healthz` (unauthenticated) JSON: mqtt connected, ha connected, bridge online, mode.
Auth: OIDC (openidconnect crate, authorization-code + PKCE) with Authentik; local username/password break-glass form at `/login/local`; signed session cookie (`tower-sessions` or axum-extra signed cookies); CSRF for POSTs via same-site cookie + hidden token. Visual design: dark, dense, monospace for bus data; mocked before implementation (see `docs/ui.md` once written).

## 7. Operations

- Docker: multi-stage build, static musl binary, `FROM gcr.io/distroless/static`, non-root, `/healthz` healthcheck, config mounted at `/config/choregos.toml`, secrets via env. Published as `ghcr.io/yasen-pavlov/choregos` on tags.
- `deploy/docker-compose.yml`: standalone example (host port, config directory mount, env secrets, health check). Integration into a specific stack (reverse proxy, log driver, networks) is left to the operator.
- Logging: `tracing` JSON to stdout; decision log lines are structured (`event=decision role=… from=… to=… reason=…`).
- CI (GitHub Actions): fmt, clippy `-D warnings`, test, `cargo llvm-cov --lcov`, SonarCloud (`sonar.projectKey=yasen-pavlov_choregos`, fork-PR guard), docker build (no push) on PR; release workflow on `v*` tags: build + push multi-arch image, git-cliff notes, GitHub release. Dependabot for cargo + actions.
- Versioning: `Cargo.toml` stays `0.0.0`; tags drive versions.

## 8. Interfaces between slices

```rust
// src/state.rs  (core owns; ui reads)
pub struct AppState { inner: Arc<Inner> }
impl AppState {
    pub fn snapshot(&self) -> Snapshot;                 // cheap clone of everything the UI renders
    pub fn subscribe(&self) -> broadcast::Receiver<UiEvent>; // Frame | Decision only
    pub async fn command(&self, cmd: Command) -> Result<(), CommandError>;
    pub fn config_text(&self) -> String;                // redacted TOML
    pub async fn save_config(&self, text: &str) -> Result<(), ConfigError>; // validate + atomic write + reload
    pub fn health(&self) -> Health;
    pub fn auth(&self) -> &AuthConfig;                  // oidc + local login settings (secrets resolved)
}
pub struct Snapshot { pub mode: Mode, pub warming: bool, pub bridge_online: bool, pub roles: Vec<RoleView>, pub bus: Vec<BusEntryView>, pub ha: HaView, pub active_role: Option<String>, pub conflicts: Vec<Conflict>, pub recent_frames: VecDeque<FrameView>, pub decisions: VecDeque<DecisionView>, pub connections: Health }
pub enum UiEvent { Frame(FrameView), Decision(DecisionView) }   // snapshots are pulled via snapshot(), never broadcast
pub enum Command { AllOff, SetMode(Mode), Identify(u8), SendRaw { to: u8, data: Vec<u8> }, Takeover(String), Standby(String) }

// src/web/mod.rs  (ui owns; core mounts)
pub fn router(state: AppState) -> axum::Router;
```

All `*View` structs are plain `Clone + Serialize` data (no `Instant`s; use
`chrono::DateTime<Utc>` and humanised strings computed in templates).

## 9. Testing contract

- `src/cec/*`, `src/identity.rs`, `src/engine.rs`: unit tests, no I/O. The engine is driven by `Event`s with an injected `now`.
- `tests/replay.rs`: feeds `fixtures/*.jsonl` through `BusModel` + `Engine` with the reference config and asserts the decision sequence (Kodi off → streaming box on → takeover actions → streaming box off → off-timer → TV/receiver off → streaming box on again → volume keys forwarded only when Sonos playing).
- Runtime adapters (`mqtt`, `ha`, `wol`): thin; tested with a local broker (`rumqttd` in-process) and a fake HA websocket server in tests where cheap, otherwise exercised by the lead's smoke run against the real stack in shadow mode.
- Web: handler tests with `axum::body` + a fake `AppState` for the pages; auth middleware tests (unauthenticated → redirect, local login happy path, CSRF rejection).
- No tests of wiring/copies; coverage target on new code ≥ 80 % honestly.
