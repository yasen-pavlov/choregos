# Choregos — design contract

Choregos (χορηγός, the patron who ran the chorus at the Dionysia) is the
conductor of a living-room home theater: it watches the HDMI-CEC bus through a
Tasmota CEC bridge, keeps a model of who is on the bus and who is the active
source, and drives the non-CEC parts of the room (TV, receiver, Sonos, Kodi)
through Home Assistant, with native TV wake/power-off, so that every device can be used with its own remote.

This document is the contract between the implementation slices. It is
authoritative until superseded by code review decisions recorded in
`docs/decisions.md`.

## 1. The room (reference topology)

```
TV ──────────── HDMI ──── AV receiver (CEC audio system, LA 5, PA 2.0.0.0)
  CEC pin disconnected        │ in1 streaming box   (playback)
  Native WoL + webOS SSAP     │ in2 Kodi box        (recording device)
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
`{"HdmiSend":{"Ack":bool}}` (patched). Only the patched response reports a
target's ACK/NACK. Stock `"Done"` means queued; `"Sending failed"` means the
bridge refused to transmit (busy or no logical address), not a target NACK.
The bridge answers `HdmiSend` requests in publish order. The transport correlates
results through a bounded FIFO of 64 outstanding sends (oldest dropped on
overflow), expiring entries older than five seconds before consuming a result.
Every reply containing `HdmiSend` consumes an entry, including firmware error
strings without ACK information. Clear the FIFO on bridge `Offline`/`Online`,
MQTT disconnect/reconnect, and any config reload (which replaces the transport).
Results only tune labels/decisions, never gate safety. Raw send data is limited
to 14 bytes: the bridge rejects payloads of 15 bytes or more.
`fixtures/capture-streamer-wake.jsonl` is a real capture (Kodi standby,
streaming box wake, standby, re-announce, remote volume presses) and is the
primary replay fixture.

## 2. Non-goals

- No general receiver/TV/Sonos/Kodi protocol implementations: Home Assistant
  handles these, except the TV's narrow native wake and webOS power-off paths.
- No persistence of live power or active-source state: the bus is
  re-interrogated on start (§5.6); learned identities and decisions are retained.
- No CEC frame decoding beyond what the engine needs (a name table for the UI
  is fine; no full CEC stack).
- No plugin system. The TV wake and off paths are small enums, not plugins.

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
src/runtime/webos.rs   native TLS SSAP TV power-off                       [core]
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
off = { kind = "webos", host = "192.168.10.20", key_env = "CHOREGOS_TV_KEY" }
# or: off = { kind = "ha" }  # default; uses HA media_player.turn_off

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
standby_retries = 2               # bounded standby retries after each takeover_settle_ms (0–10)
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
takeover = "cec_standby"          # what to do to this device when another one takes over: cec_standby | cec_pause_standby | ha_media_stop | none
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
`{physical: Option<u16>, device_type: Option<u8>, vendor: Option<u32>, osd: Option<String>, power: Unknown|On|Standby|Transition, last_seen: Option<Instant>, verified: bool, confirmed_at: Option<DateTime<Utc>>, identified: bool}`
with transient flags tracking vendor/OSD re-confirmation since restore,
plus `active_source: Option<u16>` (physical address) and the bridge's own LA.
Learning rules (all from received frames; sender = `From`):
- `84 PP PP TT` Report Physical Address (bcast) → physical, device_type, last_seen, verified = true, confirmed_at = injected wall time.
- `87 VV VV VV` Device Vendor ID (bcast) → vendor.
- `47 <ascii>` Set OSD Name → osd.
- `82 PP PP` Active Source (bcast) → active_source = PP PP, sender power = On.
- `9D PP PP` Inactive Source (to TV) → if active_source == PP PP then None; sender power = Standby.
- `36` Standby from a device (to TV or bcast) → sender power = Standby. (From the TV/bridge to others it is a command, not a report.)
- `90 SS` Report Power Status → sender power per SS (00 On, 01 Standby, 02 →On, 03 →Standby).
- `04` Image View On / `0D` Text View On (to TV) → sender power = On.
- An entry is `identified` only when verified, physical address is known and every `osd`/`vendor` key used by any configured device match rule is known. Recompute the required key set on config reload. Query only missing required keys (`83` PA, `8C` vendor, `46` OSD). Unverified restored entries additionally need PA verification: when all keys are cached, request **only `83` on every automatic attempt**, including the first; never solicit `87`/`47` solely for re-confirmation. Missing vendor/OSD keys are still requested. Manual identify requests all three and updates its last-query instant when sent; the bridge and LA 15 are never queried.
- Retry on ticks only while the bridge is online. The interval is `identify_retry_s * 2^attempts`, capped at one hour, starting at the base interval after the first query. Each unanswered retry increases attempts. Reset attempts on a genuinely new key, restored-identity verification, or bridge-online/resync, not arbitrary traffic or unchanged identity replies. After applying a wake frame (`04`/`0D` to the bridge, broadcast `82` with exactly two operands, or broadcast `85`), re-arm a non-bridge sender below LA 15 if it is still `!identified` and its last query is absent or at least the base `identify_retry_s` interval old (also capped at one hour): clear its last-query instant and reset attempts, allowing an immediate missing-key query subject to the shared one-second gate. Compare elapsed time without adding an uncapped duration to an instant. Apply this after identity replacement too. The base-interval guard permits at most one re-arm per base interval per LA, including under broadcast `82` flapping; unanswered queries resume at the base interval and then back off normally. Other replacements retain the LA's last-query instant and attempts; re-confirming a PA retained after a vendor/OSD replacement does not reset them. With a 10 s base, intervals are 10, 20, 40, …, 2560, 3600 s: the interval reaches about ten minutes after about ten minutes, and the one-hour cap after about 85 minutes of cumulative silence. Resync clears last-query instants for verified entries and records its immediate `83` query for unverified entries, whose next retry is after the base interval.
- Except for the existing one-off resync burst (§5.6), emit at most one `Action::Identify` (one LA) per handled event, with a shared one-second minimum between sends. Other due LAs (including queued manual identify requests) wait for that slot; `next_deadline` includes this deadline bump, preventing a zero-delay tick loop. Choose the earliest due entry, then LA order for ties. Resync reserves the next second too: coincident retries wait for a later tick.
- A fully keyed restored identity may also verify opportunistically after both its stored vendor and OSD have been re-confirmed since restore, in either order. Accept matching `87` and `47` from that LA regardless of destination: devices may address these replies to the receiver rather than the bridge. A single unchanged reply does not reset attempts. This alternative never updates `confirmed_at`; only `84` does. It is unavailable after replacement or if the restored PA/vendor/OSD was missing.
- A differing PA in `84` (at least three operands) or broadcast `82` (exactly two operands) from a non-bridge LA below 15 is a PA change of the same device only when its entry is already verified, both vendor and OSD are known, and the LA is bound to a role that is currently On. Continuity protects a running device; an Off role provides no same-device evidence when its LA is reused, so it retains the replace → buffer → rebind path rather than waking the previous occupant. Keep the keys, power, buffered signals and pending standby verification; update PA and last-seen time normally, and take device type from `84`. If active_source equalled the old PA, remap it to the new PA. `84` refreshes verification and `confirmed_at`; `82` preserves them. Queue a full identify (`83`/`8C`/`46`) through the shared one-second slot, including in shadow mode: retained keys are provisional until replies confirm or replace them. Vendor/OSD-matched On roles stay bound; physical-matched roles may legitimately unbind because their match key changed.
- Otherwise, a differing PA replaces the LA entry and clears its buffered signals, including when a key is unknown or a restored entry is unverified. A PA already held by another LA takes precedence over same-LA PA continuity and is an address move: the new LA inherits that entry's keys, power and verification, and the old LA is dropped. If the destination is not being replaced and already has conflicting vendor/OSD keys, only evict the old LA; retain the destination's own identity instead of inheriting. Inherited keys are provisional: queue a full identify (`83`/`8C`/`46`) at the new LA through the shared one-second slot, including in shadow mode. An `82`-first move preserves existing verification and `confirmed_at` without refreshing either; an unverified restored device stays unverified until `84` confirms it. A differing known vendor/OSD clears the other non-PA keys and signals, retains PA only as unverified, and requires fresh `84` confirmation before role matching. Only a move discards destination-occupant standby verification; ordinary key replacement leaves verification cancellation to its deadline.
- Entries age out of `power = On` only via explicit events or presence polling; never by time alone.
- Operator `Forget(la)` removes a non-bridge LA entry, its identification retry/manual-query state, buffered power signals and pending standby verification; the removal itself sends no CEC frames. Reject the command while the bridge is warming so deferred role transitions cannot leave an On role unbound. Re-resolve roles; a role losing its binding follows the existing `identity_lost` Off transition and release timer. A newly bound role consumes its latest relevant buffered on/off signal through the normal role transition. Record `command_forget` with no actions. The next persisted state excludes the entry. This local model edit is allowed in shadow mode and does not blacklist the LA: subsequent traffic learns it afresh.

### 5.2 Identity → role
A role matches a verified LA entry when every key in its `match` table matches
(OSD exact after trimming, vendor hex, physical dotted). Restored entries
are ignored until a received `84` confirms the stored PA, or both cached
vendor and OSD are re-confirmed as specified in §5.1; power signals received
before verification remain buffered. If two roles
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
2. For every other role currently On: `takeover` action — `cec_standby` → directed `CecSend{to: la, data: "36"}` followed by `"8F"`; `cec_pause_standby` → directed `"41 25"` (Play: Still), `"36"` (Standby), then `"8F"` (Give Device Power Status), for sources that ignore Standby while playing: pause through deck control first; `ha_media_stop` → `HaCall{media_player.media_stop, entity}`; `none`.
3. If Sonos state is `playing`: `HaCall{media_player.media_stop, sonos.entity}`.
4. `HaCall{media_player.turn_on, receiver}` if receiver not `on`; then after `takeover_settle_ms`: `HaCall{media_player.select_source, receiver, {source}}` if `select_source` and current receiver source ≠ `source`; then `HaCall{script.turn_on, preset_for(now)}` — the preset only runs when the receiver transitioned off→on in this takeover or the active role changed.
Takeovers are idempotent: a repeated On for the same role with no state change does nothing.

`cec_pause_standby` uses deck-control Play: Still rather than the user-control Pause key: keys sent by the TV can make the displaced source assert Active Source and briefly steal the input. Play: Still pauses without that activation side effect, allowing directed Standby to be honoured during playback.

In active mode, both CEC stop policies (including all-off and manual standby) are verified after `takeover_settle_ms`, confirmed only by observed Standby/Transition bus power from that LA. During pending verification, `90 xx` replies update bus power but are not role signals or buffered signals, so an unchanged On reply cannot take the room back. Unless confirmed, retry the complete original stop/query sequence up to `standby_retries` times: `36`, `8F` for `cec_standby`, or `41 25`, `36`, `8F` for `cec_pause_standby`. Silence or NACK is not confirmation and does not shorten retries. Each pending verification remembers its sequence and counts every outgoing frame in it to its LA (`sent`: two or three per round), correlated NACKs (`nacked`), and whether any ACK was observed (`acked`). Observed Standby/Transition confirms the stop even when the trailing `8F` NACKs because the device is already asleep; an ACKed `36` also prevents absence classification if no power report arrives. On exhaustion without confirmation, record `standby_target_absent` if `sent > 0 && !acked && nacked >= sent` (expected for devices that leave the bus in standby), otherwise `standby_unconfirmed` (ACKed frames or missing results without a confirming power report). Extra/late NACKs cannot override an observed ACK. ACK-based classification requires patched bridge firmware; stock firmware without ACK information can only produce `standby_unconfirmed`. Send results for other LAs or frame data outside the remembered sequence do not affect verification. A real role On event (including configured `04`, `0D`, or `82`) or blocked/remapped identity cancels verification; bridge-offline or a config reload clears pending verifications. No verification is registered in shadow mode, and entering shadow clears pending checks. Residual risk: a retry deadline can land between a waking device's non-on-event frames (`84`/`87`, depending on its configured on list) and its on-event.

### 5.5 Release (role turns Off)
Start/restart `off_delay_s` timer. On expiry, if no role is On: `TvOff` and `HaCall{media_player.turn_off, receiver}`. `TvOff` uses HA by default, or native webOS SSAP over TLS (default port 3001) with an environment-only pairing key. Native off has one five-second connect/register/request/close deadline per attempt, three attempts one second apart, and no HA fallback. If a role turned On meanwhile the timer is cancelled.
Before each native power-off attempt, query `getPowerState`; `Power Off`, `Suspend`
and `Active Standby` succeed without `turnOff`, which can wake a standby TV.
An unsupported-service/error response permits `turnOff` on older webOS.
Pairing-required responses are permanent errors and are never retried.
After an acknowledged power-off, websocket close is best-effort.

### 5.6 Startup / resync
On start and on bridge LWT `Online`: send `85` (Request Active Source, bcast) and `8F` to LAs 1, 4, 8, 11 (and any LA in config `match.physical` tables); the replies populate the model within ~1 s. Until the first reply or 3 s, no power actions are taken (`Warming` state shown in UI).
Resync additionally sends `83` immediately to each unverified restored LA. This counts as its first query; without identification progress, the next `83` waits `identify_retry_s`, not merely the one-second send slot.

#### Persisted state

The runtime loads versioned `choregos.state.json` from the config file's directory.
Version 1 contains only learned identities (`la`, `physical`, `vendor`, `osd`,
`device_type`, `confirmed_at`) and the newest 2000 `DecisionView` entries, newest
last; no frames, credentials or live power state. Only identities confirmed by
an `84` are persisted; empty entries and the bridge LA are excluded. On load,
drop confirmations older than the fixed 30-day lifetime. Missing files start
empty; corrupt or unknown versions warn and start empty without failing startup.

`Engine::new(config, now, restored)` receives the seed without I/O. Restored
entries have Unknown power, no last-seen time, and verified = false; their
restore instant anchors the first retry deadline. Duplicate restored physical
addresses keep the earlier LA. A matching received `84` verifies cached keys
and enables role matching. A differing PA on an unverified restored entry
still replaces the cached identity: only verified, fully keyed entries qualify
for same-device PA continuity (§5.1).
Opportunistic matching vendor plus OSD traffic may also verify a
fully keyed restored identity (§5.1), without refreshing its persisted
`confirmed_at` or extending the 30-day lifetime.
The devices page labels unverified entries `restored` rather than inventing a
last-seen time. The runtime appends `service_started` after restored decisions.

Snapshot publication serializes only changed identity/decision state and hands
it to a single-slot watch channel. One writer task performs atomic replacement
off the event loop, with newest state winning over queued older snapshots.
Reuse config-save temp-file/fsync/rename/directory-fsync behavior: rename commits
the write; post-rename directory-fsync failure warns without reporting failure.
On write failure warn once, retry after 60 seconds or immediately on new state,
and log recovery once when writing succeeds again. Shutdown flushes the newest
state; a persistently failing final attempt warns and cannot guarantee durability.
Replay never opens the state file.

### 5.7 Presence polling (`presence = "poll"`)
While the role is On: every `presence_poll_s` send `8F` to its LA. A `90 xx` reply (or any frame from that LA) resets the miss counter; `presence_misses` consecutive polls without any frame from the LA → role Off (with decision reason `presence_timeout`) and bus entry power Unknown, retaining its learned identity. Silence is not an observed standby report. Polling stops when Off.

### 5.8 Fake TV replies (frames addressed to the bridge LA)
`fake_tv.replies` defaults to `true`. Set it to `false` when bridge firmware handles directed `8F`/`9F`/`46`/`91` requests itself. The service never answers `85` Request Active Source: only the current active source device may announce its own physical address, and a TV must not claim another device's address on its behalf. Such a reply can suppress the real source's announcement and provoke an Inactive Source release. Never reply to unregistered senders (LA 15).
- `8F` → `90 00` (when `power_status = "always_on"`).
- `9F` → `9E 05` (1.4).
- `46` → `47 <osd_name>`.
- `91` → `32 "eng"`.
Everything else: no reply (Feature Abort is a firmware concern).

### 5.9 Sonos volume forwarding
`44 41|42|43` (User Control Pressed: Volume Up/Down/Mute) from a role listed in `sonos.volume_from`, addressed to the bridge or to LA 5, while the Sonos entity state is `playing` → `HaCall{media_player.volume_up|volume_down, sonos}` / `HaCall{switch.toggle, mute_switch}`. `45` (release) is ignored. Not debounced (one step per press frame), as today.

### 5.10 Modes
`shadow`: the engine runs fully; the executor performs **only** read-only identification queries, startup resync and presence `8F` polls. Every other device action is logged as `would: …` with the full payload. `active`: everything executes. The HA `select.choregos_mode` entity and the UI switch the mode at runtime and persist it to the config file (atomic rename preserving comments/order).

### 5.11 HA exposure (MQTT discovery, retained)
Node id `choregos`, availability topic `choregos/status` (`online`/`offline` LWT). Entities: `binary_sensor.choregos_<role>` (power, per role), `sensor.choregos_active_source` (role name or `none`), `select.choregos_mode` (shadow/active), `button.choregos_all_off`, `binary_sensor.choregos_bridge` (bridge LWT), `sensor.choregos_conflicts` (count). Commands arrive on `choregos/cmd/<entity>`. State topics `choregos/state/<name>` retained.

### 5.12 Events into the engine
`Frame`, `SendResult{to, data, ack}`, `BridgeOnline/Offline`, `HaState{entity, state, attrs, changed: Option<DateTime<Utc>>}` (tv, receiver, sonos, kodi — subscribed via `subscribe_trigger`/`state_changed`), `Tick(now)`, `Command(AllOff | SetMode | Identify(la) | Forget(la) | SendRaw{to,data} | ManualTakeover(role))`, `ConfigReloaded`.
`SendResult` correlates patched `{"Ack":true|false}` bridge results with
successfully queued `HdmiSend` publishes. Stock `"Done"` / `"Sending failed"`
and other non-ACK responses consume the FIFO entry without emitting an event:
they do not establish whether the target is present. Uncorrelated results are
ignored. Correlated results, including resync NACKs for absent LAs, are
debug-level logs rather than warnings.
HA's parsed `last_changed` takes precedence; absent/invalid timestamps use injected
wall time on state-string changes, otherwise retain the cached timestamp.
Optimistic state writes preserve the last HA-observed timestamp; sending a
command does not establish when the device actually changed state.

## 6. Web UI (axum + askama + htmx + SSE; no JS build step)

Pages (all behind auth): 
- `/` Dashboard: mode switch (confirm when going active), bridge status, role cards (name, power, LA/PA, vendor/OSD, last event, "takeover" and "standby" buttons), TV/receiver/Sonos/Kodi HA states, active source, conflicts banner, "all off".
- `/bus` Live bus: SSE stream of frames (decoded: from/to names, opcode name, operands) interleaved with decisions; filters by LA/opcode; last 2000 kept in memory (ring buffer in `AppState`).
- `/devices` Bus table (every LA with learned fields, identified/unidentified, matched role) and the role → source mapping; "identify" and confirmed "forget" controls per non-bridge LA.
- `/decisions` Decision log with reasons and resulting actions (and `would:` entries in shadow mode).
- `/config` Current file TOML verbatim (comments/order preserved; secrets are environment-only), edit + validate + save (atomic), reload.
- `/healthz` (unauthenticated) JSON: mqtt connected, ha connected, bridge online, mode.
Auth: OIDC (openidconnect crate, authorization-code + PKCE) with Authentik; local username/password break-glass form at `/login/local`; signed session cookie (`tower-sessions` or axum-extra signed cookies); CSRF for POSTs via same-site cookie + hidden token. Visual design: dark, dense, monospace for bus data; mocked before implementation (see `docs/ui.md` once written).

## 7. Operations

- Docker: multi-stage build, static musl binary, `FROM gcr.io/distroless/static`, non-root, `/healthz` healthcheck; writable `/config` holds `choregos.toml` and `choregos.state.json`, secrets via env. Published as `ghcr.io/yasen-pavlov/choregos` on tags, and `:edge` on every push to `main`.
- `deploy/docker-compose.yml`: standalone example (host port, config directory mount, env secrets, health check). Integration into a specific stack (reverse proxy, log driver, networks) is left to the operator.
- Logging: `tracing` JSON to stdout; decision log lines are structured (`event=decision role=… from=… to=… reason=…`).
- CI (GitHub Actions): fmt, clippy `-D warnings`, test, `cargo llvm-cov --lcov`, SonarCloud (`sonar.projectKey=yasen-pavlov_choregos`, fork-PR guard), docker build (no push) on PR; release workflow on `v*` tags and `main` pushes: build + push multi-arch image; git-cliff notes and GitHub release only on tags. Dependabot for cargo + actions.
- Versioning: `Cargo.toml` stays `0.0.0`; tags drive versions.

## 8. Interfaces between slices

```rust
// src/state.rs  (core owns; ui reads)
pub struct AppState { inner: Arc<Inner> }
impl AppState {
    pub fn new(config: Config, config_text: String) -> Self; // config and text from the same file read
    pub fn snapshot(&self) -> Snapshot;                 // cheap clone of everything the UI renders
    pub fn subscribe(&self) -> broadcast::Receiver<UiEvent>; // Frame | Decision only
    pub async fn command(&self, cmd: Command) -> Result<(), CommandError>;
    pub fn config_text(&self) -> String;                // current file text; environment secrets never serialized
    pub async fn save_config(&self, text: &str) -> Result<(), ConfigError>; // validate + atomic write + reload
    pub fn health(&self) -> Health;
    pub fn auth(&self) -> &AuthConfig;                  // oidc + local login settings (secrets resolved)
}
pub struct Snapshot { pub mode: Mode, pub warming: bool, pub bridge_online: bool, pub bridge_logical_address: u8, pub roles: Vec<RoleView>, pub bus: Vec<BusEntryView>, pub ha: HaView, pub active_role: Option<String>, pub conflicts: Vec<Conflict>, pub recent_frames: VecDeque<FrameView>, pub decisions: VecDeque<DecisionView>, pub connections: Health }
pub enum UiEvent { Frame(FrameView), Decision(DecisionView) }   // snapshots are pulled via snapshot(), never broadcast
pub enum Command { AllOff, SetMode(Mode), Identify(u8), Forget(u8), SendRaw { to: u8, data: Vec<u8> }, Takeover(String), Standby(String) }

// src/web/mod.rs  (ui owns; core mounts)
pub fn router(state: AppState) -> axum::Router;
```

All `*View` structs are plain `Clone + Serialize` data (no `Instant`s; use
`chrono::DateTime<Utc>` and humanised strings computed in templates).

## 9. Testing contract

- `src/cec/*`, `src/identity.rs`: unit tests, no I/O. Engine behavior tests live in `tests/replay.rs`, driven by `Event`s with injected monotonic and wall times.
- `tests/replay.rs`: feeds `fixtures/*.jsonl` through `BusModel` + `Engine` with the reference config and asserts the decision sequence (Kodi off → streaming box on → takeover actions → streaming box off → off-timer → TV/receiver off → streaming box on again → volume keys forwarded only when Sonos playing).
- Runtime adapters (`mqtt`, `ha`, `wol`): thin; tested with a local broker (`rumqttd` in-process) and a fake HA websocket server in tests where cheap, otherwise exercised by the lead's smoke run against the real stack in shadow mode.
- Web: handler tests with `axum::body` + a fake `AppState` for the pages; auth middleware tests (unauthenticated → redirect, local login happy path, CSRF rejection).
- No tests of wiring/copies; coverage target on new code ≥ 80 % honestly.
