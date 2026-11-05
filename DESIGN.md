# Choregos — design contract

Choregos (χορηγός, the patron who ran the chorus at the Dionysia) is the
conductor of a living-room home theater: it watches the HDMI-CEC bus through a
CEC bridge (patched Tasmota or the native aether firmware), keeps a model of
who is on the bus and who is the active source, and drives the non-CEC parts
of the room (TV, receiver, Sonos, Kodi) through Home Assistant, with native TV
wake/power-off, so that every device can be used with its own remote.

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
                              │ in5 the bridge      (HDMI-CEC device claiming the TV role, LA 0; Tasmota or aether firmware)
Sonos (line-in of a separate amp, not on HDMI)
home server: MQTT broker, Home Assistant (websocket), choregos (this)
```

The TV's CEC is physically disconnected: the bridge is the only "TV" on the
bus. Two bridge firmwares are supported; `[bridge].kind` selects the adapter
in `src/runtime/bridge/`. Everything downstream (engine, snapshot, UI, HA
discovery, trace replay) sees only `Event`s, `BridgeTelemetry` and
`TraceEntry`s, never bridge topics.

`kind = "tasmota"` (patched Tasmota HDMI-CEC driver): every CEC frame the
bridge sees is published as
`tele/<topic>/SENSOR` → `{"HdmiReceived":{"From":4,"To":0,"Data":"04"}}`
(`Data` = opcode + operands, uppercase hex, no separators; empty for polling
messages). The bridge publishes frames addressed to itself, to broadcast (15)
and (patched firmware) to the audio system (5). Choregos sends frames with
`cmnd/<topic>/HdmiSend` → `{"To":<la>,"Data":"<hex>"}`; the bridge answers
`stat/<topic>/RESULT` → `{"HdmiSend":"Done"|"Sending failed"}` (stock) or
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
`tele/<topic>/LWT` `Online`/`Offline` → `BridgeOnline`/`BridgeOffline`;
`tele/<topic>/STATE` Wi-Fi fields → `BridgeTelemetry`, stale after 15 min.

`kind = "aether"` (aether MQTT protocol v1, `aether-cec/docs/protocol.md`;
topics `aether/<node>/{status,bus,tx,telemetry,cfg}`, all JSON, unknown keys
ignored):
- `status` (retained; LWT `{"online":false}`): `online:true` → `BridgeOnline`;
  `online:false` → `BridgeOffline` (pending sends dropped; `boot`/`seq` kept so
  a same-boot reconnect still dedupes); a changed `boot` while online (retained
  LWT lost or missed) → `BridgeOffline` then `BridgeOnline`; the same `boot`
  again with a newer `status.t_us` is a fresh publish from the board (it
  publishes `status` on every broker connect, so this covers a broker restart
  that left a stale retained `online:true`, and `la`/bus-state changes) →
  `BridgeOnline` (resync); the same `boot` and `t_us` (retained redelivery) →
  no event. `v` ≠ 1 → one error log per connection, no `BridgeOnline`, and
  `bus` events are ignored until a supported `status` arrives. `status.bus` →
  `Snapshot.bridge_bus`. On every MQTT (re)connect of its own the adapter
  drops pending sends and availability (the redelivered retained `status` is
  an `Online` again) but keeps the board's `boot`/`seq` history, so a QoS 1
  redelivery around a broker restart still dedupes.
  Before any `status` has been seen since the last clear the state is
  *unknown*: `bus` events are decoded without a `BridgeOnline` (this is what
  a trace replay window or a daily trace file without a `status` line sees;
  live, the retained `status` always precedes the first `bus` event).
- `bus` (QoS 1; one ordered stream with a per-boot `seq`): `rx` →
  `Event::Frame` carrying `ack` and `t_us`; a directed `rx` with `ack:false`
  was cut short by a NACK (protocol §5.1 publishes the blocks received so
  far), so the engine receives it header-only (`opcode: None`, no params) as
  presence evidence while the trace and UI keep the raw frame — a truncated
  `47`/`84` must never replace a learned identity; `tx` with `origin:"host"` →
  `Event::SendResult` for the pending send with that `id` (unknown or expired
  ids are debug-logged and ignored, so manual `mosquitto_pub` sends never reach
  the engine; `status:"rejected"` → warn with `reason`, no event); `tx` with
  `origin:"local"` → the UI frame ring only (`FrameDirection::Local`, `ack`
  `Some(true)` for `ack`, `Some(false)` for `nack`, `None` otherwise), never an
  engine event; `line` → `Snapshot.bridge_bus`; `err` → debug log. An event
  whose `seq` is not greater than the last `seq` seen for the current `boot` is
  a QoS 1 duplicate and is dropped; a gap is logged at warn.
- Sends: `aether/<node>/tx` → `{"id":"s-<n>","to":<la>,"data":"<HEX>"}`, `n`
  from one process-wide counter (adapters are replaced on config reload and a
  late outcome of the old adapter's send must not match the new one's);
  `retries` is omitted (firmware default: 0 for polls, 1 otherwise, so NACK
  stays absence evidence). Pending sends live in an id map with the Tasmota
  FIFO's bounds (64 entries, five-second expiry) and clear points; a late
  outcome for an id issued before the last clear matches nothing. Raw send
  data is limited to 15 bytes.
- `telemetry` → `BridgeTelemetry{signal_dbm: rssi, link_count, uptime_s:
  t_us / 1e6}`, stale after 3 × `cfg.telemetry_s` (180 s until `cfg` is seen;
  `telemetry_s = 0` disables telemetry, so `stale_after_s = 0` disables the
  age check and only the weak-signal check remains).
- `cfg` (retained): the adapter warns once per connection when `cfg.la`
  differs from `[bridge].logical_address` and when `[fake_tv].replies` is on
  while `cfg.replies` is non-empty (both sides would answer the same query).
  `cmd/*` (including `cmd/state`), `ota` and `log` are not used by Choregos.
- Promiscuous receive: frames whose `from` is the bridge's own LA (its own
  sends echoed as `rx`, or another TV during a migration overlap) are published
  to the UI but never reach the bus model, role signals or the fake TV.
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
src/cec/frame.rs       Frame {from, to, opcode: Option<u8>, params: Vec<u8>, ack: Option<bool>, t_us: Option<u64>}; parse HdmiReceived JSON; to_hdmisend() [core]
src/cec/opcode.rs      const names for the opcodes used + name lookup for the UI [core]
src/cec/bus.rs         BusModel: per-LA device table, active source, vendor/osd/physical learning [core]
src/identity.rs        role resolution (config match rules × bus table)  [core]
src/engine.rs          pure state machine: Event → Vec<Action> (+ timers) [core]
src/action.rs          Action enum + Decision log entry                  [core]
src/runtime/mod.rs     event loop: channels → engine → executor          [core]
src/runtime/mqtt.rs    rumqttc client; HA discovery; state topics; hands bridge topics to src/runtime/bridge [core]
src/runtime/bridge/mod.rs      kind-selected bridge adapter: topic list, send, and the TraceEntry → Event correlation shared by live decode and replay [core]
src/runtime/bridge/tasmota.rs  SENSOR/LWT/STATE/RESULT/HdmiSend codec + 64-entry send FIFO [core]
src/runtime/bridge/aether.rs   status/bus/tx/telemetry/cfg codec + id map, boot tracking, seq dedupe [core]
src/runtime/ha.rs      HA websocket client: auth, call_service, state subscriptions, reconnect [core]
src/runtime/wol.rs     magic packet to directed broadcast                [core]
src/runtime/webos.rs   native TLS SSAP TV power-off                       [core]
src/runtime/observe.rs bounded direct TV and console observation windows  [core]
src/runtime/probe.rs   read-only unicast console discovery over UDP       [core]
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

[bridge]                  # the CEC bridge; `kind` selects the adapter (required, no default)
kind = "tasmota"          # tasmota | aether
topic = "tasmota_cec"     # tasmota: tele/<topic>/SENSOR, cmnd/<topic>/HdmiSend, stat/<topic>/RESULT, tele/<topic>/LWT, tele/<topic>/STATE
# kind = "aether"
# node = "aether"         # aether: aether/<node>/{status,bus,tx,telemetry,cfg}
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

[verify]                         # optional; direct webOS read windows after active TV dispatch
tv_poll_ms = 2000                 # 500–10000 ms
tv_window_s = 45                  # 5–300 seconds
probe_poll_ms = 2000              # 500–10000 ms
probe_window_s = 45               # 5–300 seconds
tv_s = 45                        # expectation window permits HA reconnect lag after TV wake
receiver_s = 10
receiver_off_s = 20
speakers_s = 5
source_s = 10
drift_grace_s = 30                # reporting-only drift grace, 5–600 seconds

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
verify_standby = false  # no CEC verification/retries; optional probe is the only standby evidence
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
`{physical: Option<u16>, device_type: Option<u8>, vendor: Option<u32>, osd: Option<String>, power: Unknown|On|Standby|Transition, power_at: Option<Instant>, last_seen: Option<Instant>, verified: bool, confirmed_at: Option<DateTime<Utc>>, identified: bool}`
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
1. `WakeTv` if room intent is Off **or** `tv.entity` state is not `on` (HA state cache; if unknown, send anyway).
2. For every other role currently On: `takeover` action — `cec_standby` → directed `CecSend{to: la, data: "36"}` followed by `"8F"`; `cec_pause_standby` → directed `"41 25"` (Play: Still), `"36"` (Standby), then `"8F"` (Give Device Power Status), for sources that ignore Standby while playing: pause through deck control first; `ha_media_stop` → `HaCall{media_player.media_stop, entity}`; `none`.
3. If Sonos state is `playing`: `HaCall{media_player.media_stop, sonos.entity}`.
4. `HaCall{media_player.turn_on, receiver}` if room intent is Off **or** receiver not `on`; `receiver_was_off` uses that same rule. Then after `takeover_settle_ms`: `HaCall{media_player.select_source, receiver, {source}}` if `select_source` and current receiver source ≠ `source`; then `HaCall{script.turn_on, preset_for(now)}` — the preset only runs when `receiver_was_off` or the active role changed.
Takeovers are idempotent: a repeated On for the same role with no state change does nothing.

`cec_pause_standby` uses deck-control Play: Still rather than the user-control Pause key: keys sent by the TV can make the displaced source assert Active Source and briefly steal the input. Play: Still pauses without that activation side effect, allowing directed Standby to be honoured during playback.

- Per-device `verify_standby` defaults to `true`. When false, send the complete CEC stop/query sequence once (including all-off/manual standby), without retries or CEC standby evidence. If a probe is configured, create `[ps5]` only; otherwise create no standby expectation for that role. A console can answer `36` with `90 00` and sleep silently later, so CEC On is not a refutation for an opted-out role. Apply normal configured `on`/`off` lists to its `90 xx` replies. Reject this opt-out for CEC policies if `on` includes `power_on`, since the query's On reply could undo takeover.

In active mode, CEC stops with `verify_standby = true` retain the original
verification lifecycle independently of verdicts. At each due check, remove
the slot if the role woke, its identity became blocked/remapped, or bus power
is Standby/Transition. Otherwise resend the original sequence up to
`standby_retries`, scheduling the next check at `now + takeover_settle_ms`;
on exhaustion remove the slot. Until that check removes it, `90 xx` updates
bus power but is neither a role signal nor a buffered signal. A transient
Standby then On before the check must not take back the room. Reporting
confirmation or deadline expiry never removes or extends the control slot.
Cancellation supplies unknown to an unfinished expectation; exhaustion stores
an unknown fallback with `observed: "absent"` only when
`sent > 0 && !acked && nacked >= sent`. Fresh matching evidence still wins,
and a fresh disagreement at the reporting deadline is missed. Sequence ACK/NACK
accounting, address-move retargeting and cancellation on bridge offline/reload/
shadow are unchanged; independent probe confirmation does not stop retries.

### 5.5 Release (role turns Off)
Start/restart `off_delay_s` timer. On expiry, if no role is On: `TvOff` and `HaCall{media_player.turn_off, receiver}`. `TvOff` uses HA by default, or native webOS SSAP over TLS (default port 3001) with an environment-only pairing key. Native off has one five-second connect/register/request/close deadline per attempt, three attempts one second apart, and no HA fallback. If a role turned On meanwhile the timer is cancelled.
Before each native power-off attempt, query `getPowerState`; `Power Off`, `Suspend`
and `Active Standby` succeed without `turnOff`, which can wake a standby TV.
An unsupported-service/error response permits `turnOff` on older webOS.
Pairing-required responses are permanent errors and are never retried.
After an acknowledged power-off, websocket close is best-effort.

Room intent is separate from observations: `RoomIntent { power: Unknown|On|Off,
active_role: Option<String>, receiver_source: Option<String>,
off_at: Option<DateTime<Utc>> }` starts Unknown with empty role/source/off time
and is never persisted. Unknown uses HA alone for initial wake suppression,
avoiding redundant power requests after a restart with the room already on.
Wake transitions set On, the active role, and the configured receiver source
when source selection is enabled. Reload clears the intended receiver source.
Release clears the active role; all-off only releases roles and schedules the
room-off timer. Only timer expiry sets Off, clears role/source, and records its
wall time in `intent.off_at`. Keep that timestamp across subsequent wakes:
cached On counts only when its HA `changed` time is strictly later than the
last room-off; older/equal or missing timestamps cannot suppress repeated
wakes during shutdown lag. Off requests never write optimistic HA state.

Only for native webOS TV off, active TV wake/off dispatch starts read-only
`getPowerState` polling through the shared TLS/SSAP session. Poll immediately,
then every `[verify].tv_poll_ms` (default 2000, 500–10000) until target or
`tv_window_s` (default 45, 5–300 seconds); a newer action replaces the window.
`Active`, `Screen Off`, and `Screen Saver` map to On; `Active Standby`,
`Suspend`, and `Power Off` map to Off. Failed queries map to Unreachable, not Off. Pairing-required stops
polling and warns once until adapters restart. Shadow mode does not poll.
The last `Event::TvPower { state: On|Off|Unreachable, at }` is kept separately
as `tv_direct: Option<(TvState, DateTime<Utc>)>` in engine and snapshot, never
persisted. Observation windows do not create expectations/verdicts or actions.

Optional per-device `probe = { kind = "ps5", host = "console.lan", port = 9302 }`
adds Sony console observations. Send one unicast datagram
`SRCH * HTTP/1.1\ndevice-discovery-protocol-version:00030010\n`; wait at most
one second. First reply line `HTTP/1.1 200 …` maps to Awake, `620 …` to Standby;
silence, transport errors or unsupported replies map to Unreachable.
Active wake transitions start an Awake window; dispatched CEC stop sequences
for that role start a Standby window. Poll immediately then every
`verify.probe_poll_ms` (default 2000, 500–10000), until target or
`probe_window_s` (default 45, 5–300 seconds). A new target cancels the previous
role window; duplicate stop retries do not extend a pending window. Shutdown
and adapter reload cancel all windows, including in-flight queries.
`Event::ConsolePower { role, state: Awake|Standby|Unreachable, at }` updates
the role's separate, non-persisted `direct` observation; `RoleView` exposes
`direct: Option<(ConsoleState, DateTime<Utc>)>` and `probe_configured: bool`.
Remote Play must be enabled and UDP reachability provided by the operator.

### 5.5.1 Expectations and verdicts

Each active observable decision carries `expectations: Vec<Expectation>` and a
`verdict`. `Expectation` has `subject` (`tv`, `receiver`, `speakers`,
`role:<name>`), `field` (`power`, `source`, `playback`), string `expect`,
`via` (ordered `tv_direct`, `ps5`, `ha`, `cec`), `deadline_ms` relative to
the decision time, `attempts: u8` (standby resends), and tagged `outcome.kind`:
`pending`, `met {via, observed, latency_ms}`, `missed {observed}`,
`unknown {observed?}`, or `cec_only {observed}`. Values are strings.

Wake transitions expect TV On only when `WakeTv` was sent (direct webOS then
HA, or HA only), receiver On only when `turn_on` was sent, speakers stopped
only when `media_stop` was sent, and standby for stopped CEC-policy roles with
verification enabled or an independent probe. `cec_standby` and
`cec_pause_standby` permit `[ps5, cec]` with a probe, otherwise `[cec]`.
Opted-out roles and non-CEC policies permit `[ps5]` only when a probe exists;
`ha_media_stop`/`none` never gain a CEC power expectation.
All-off/manual standby rows use the same policy for dispatched stops.
`takeover_settled` expects the selected receiver source only if selection
was sent; presets have no observable. `off_timer_expired` expects TV Off only
when `TvOff` was sent and receiver Off only when its `turn_off` was sent.
Unchanged room power is not an observable effect to verify. All other rows,
including shadow would-rows, have empty
expectations and verdict `none`.

Default deadlines under `[verify]` are TV 45 s, receiver power 10 s, speakers
5 s, source 10 s. The TV deadline is an observation window accommodating
20–33 s HA reconnect lag after wake, not a latency target. Standby with only a
CEC leg uses `takeover_settle_ms × (standby_retries + 1)`; probe-only standby
uses `verify.probe_window_s × 1000`. With both legs, use the greater deadline
so independent confirmation can arrive after CEC retries exhaust. The CEC
retry schedule and verification-slot lifetime are unchanged.
At creation only, the newest sample before the decision across listed independent sources (`ha`, `tv_direct`, or `ps5`, never `cec`) settles `met` with zero latency if it matches, is in `[decision.at - deadline_ms, decision.at)`, and postdates every still-pending decision carrying the same subject and field; if all expectations meet this way, the decision is immediately `achieved`.
On every event, evaluate pending fields against observations newer than the
decision: HA power/playback uses `changed`, source attributes use `updated`,
direct observations use their timestamps. Independent sources settle a fresh
match immediately with actual latency; a different source's disagreement cannot
veto it. The CEC leg is confirmed only by the verification's due confirmed check,
with fresh Standby/Transition still present, never by raw frame sampling.
A brief Standby followed by On before that check is not success; exhaustion with
fresh On is missed, and without usable evidence is unknown. With a probe, retain
checked CEC confirmation until the deadline if no independent match arrives;
later cancellation must not erase that checked success. At the deadline,
use only each independent source's current sample, newer than the decision:
the latest usable disagreement supplies `missed.observed`. Unreachable direct
samples and HA unavailable/unknown replace that source's earlier disagreement;
they neither confirm nor refute. Without current fresh usable evidence the
outcome is unknown, not a remembered missed result.
All deadlines feed `next_deadline`, including during warm-up.

When all expectations settle, verdict is `achieved` if all met/CEC-only,
`partial` if some did and some missed/unknown, `unknown` if all unknown, and
`missed` otherwise. Verdict latency is the slowest `met` expectation, or null.
Settlements mutate the existing ring row and persist via the single-slot
writer; `UiEvent::Verdict {id, verdict, expectations}` refreshes the live bus.
After restart, unfinished persisted fields settle unknown through this same
path, retaining completed evidence; old monotonic deadlines are not resumed.
A pending field restored after an abrupt restart settles unknown even if CEC
had confirmed before the crash, because pending verification progress is not
serialized.
`choregos/verdict` traces `{id, verdict, expectations}`; replay ignores
recorded verdicts and prints `{at, verdict_for, verdict, expectations}` on
recomputed settlement. Replay runs control timers only through its existing
last-input-plus-five-second drain, then settles reporting deadlines without
running identification, presence, standby-retry, source-settle, or room-off timers.
Verdicts and drift are reporting-only; neither sends corrective actions.
Bridge offline cancels only pending all-`[cec]` expectations. Independent
HA/TV/probe expectations continue. Reload cancels only fields whose relevant
endpoint, policy, target/source, bridge or deadline configuration changed;
unrelated edits do not abandon in-flight observations.

#### Drift

In active mode outside warm-up, compare `RoomIntent` to `tv.power` and
`receiver.power` when intended power is On/Off and the entity is configured;
also compare `receiver.source` when power is On and a source is intended.
Unknown intent checks nothing. Speakers playback has no persistent intent,
and role power is the CEC-derived wake input, so neither is monitored.
Use the shared newest usable independent sample lookup: HA power uses
`changed`, source uses `updated`, TV direct uses `at`; unknown/unavailable
and direct Unreachable supply no evidence. Never use CEC evidence for drift.
HA media-player power normalizes `on|playing|paused|idle|buffering` to `on`,
and `off|standby` to `off` for both drift and expectations; other usable
states stay raw. Playback normalization remains separate.

Evaluate after expectations, on every event including Tick. Undefined intent
clears grace and resolves any open drift (observed current usable value or
`unknown`); unusable evidence otherwise clears grace, retaining open drift. A pending
expectation for the same subject/field clears grace, retaining an open drift;
another field's pending expectation does not block. Receiver source selection
awaiting the takeover-settle timer also counts as pending for `receiver.source`,
before its expectation exists. Agreement clears grace and resolves an open drift.
Disagreement starts grace at first observation
using injected monotonic time, not the sample's historical timestamp.
Changing the selected disagreement's timestamp or value restarts grace;
identical redelivery preserves it. Thus HA reconnect off/unavailable/off/on
flaps do not accumulate a false disagreement clock.
At `[verify].drift_grace_s` (default 30, bounds 5–600), emit one `drift`
per onset; further disagreement emits nothing until resolution.
Intent changes close the old drift immediately and reset grace before comparing
the new intent. Reload resets all grace clocks, retains the open set until
re-evaluation, and delays evaluation through the existing three-second warm-up.
Entering Shadow resolves every open drift using the current usable observation
or `unknown`, and clears grace in the command pass before cancelling expectations.
Shadow never starts drift. Grace deadlines join `next_deadline`; clearing
grace on unusable evidence also prevents expired-deadline Tick loops.

Both rows have no role/transition/actions/expectations and verdict `none`.
Optional `Decision.drift` / `DecisionView.drift` carries
`DriftDetail { subject: String, field: Field, intended: String, observed: String,
via: Via, since: DateTime<Utc> }`; absent keys default to None and are omitted
on serialization. The resolved row retains the original intended value and
onset wall time (`since`), updating observed/via; its duration is row time
minus since. Onset uses `{"kind":"observation","source":"ha"}` (or `tv_direct`)
only when the handled event supplies the selected observation; unrelated events
and Tick use `{"kind":"timer","name":"drift_grace"}`. Every resolution uses
observation provenance with its selected source (or the original via if unknown).
Rows use the ordinary ring/trace/replay paths without a state version bump.
`Snapshot.drift: Vec<DriftDetail>` exposes the in-memory open set; it is not
persisted or included in the entity-state fingerprint until exposure work.
Drift participates in replay's five-second control drain, not its
expectation-only tail.



### 5.6 Startup / resync
On start and on bridge online (Tasmota LWT `Online`; aether `status.online:true` or a `boot` change): send `85` (Request Active Source, bcast) and `8F` to LAs 1, 4, 8, 11 (and any LA in config `match.physical` tables); the replies populate the model within ~1 s. Until the first reply or 3 s, no power actions are taken (`Warming` state shown in UI).
Resync additionally sends `83` immediately to each unverified restored LA. This counts as its first query; without identification progress, the next `83` waits `identify_retry_s`, not merely the one-second send slot.

#### Persisted state

The runtime loads versioned `choregos.state.json` from the config file's directory.
Version 2 contains `next_decision_id`, learned identities (`la`, `physical`,
`vendor`, `osd`, `device_type`, `confirmed_at`) and the newest 2000 `DecisionView`
entries, newest last. Each has a monotonic `id`, tagged input/timer/service
`trigger`, optional `parent`, additive `expectations` (missing defaults empty),
`verdict` (missing defaults none), and optional `drift` detail (missing defaults
None). Source-settle rows link to their wake
decision; standby mechanics and outcomes belong to the decision dispatching
the stop, including across address moves. Timer rows retain their timer
trigger even when another input processes the deadline.
Version 1 is
atomically migrated in place: ring-order ids start at 1, parents are null,
triggers are `service {name:"migrated"}`, and the next counter is ring length
plus one. The loader raises stale counters above retained ring ids and rejects
exhausted counters as corrupt; ids are scoped to the persisted state lineage,
not globally. Only identities confirmed by an `84` are persisted; empty entries
and the bridge LA are excluded. On load,
drop confirmations older than the fixed 30-day lifetime. Missing files start
empty; corrupt or unknown versions warn and start empty without failing startup.

- A separate mpsc writer appends engine inputs and actual outgoing CEC frames to
  `cec-bus.<UTC-date>.jsonl` beside the state file, with learned identities at
  every open and retention from live-reloadable `[trace].keep_days` (default 30,
  0 disables, maximum 365). Runtime and replay share one JSONL codec. Trace I/O
  never blocks the engine. Bridge telemetry is a runtime input, retained
  only in the snapshot and exposed as diagnostic HA sensors, not an engine event.
- Trace lines are the bridge's own topics and payloads. Tasmota: `tele/<t>/SENSOR`,
  `tele/<t>/LWT`, `cmnd/<t>/HdmiSend`, `stat/<t>/RESULT`. Aether: the raw
  `aether/<node>/status` and `aether/<node>/bus` payloads and the
  `aether/<node>/tx` requests Choregos published (with their `id`). The live
  adapter and replay turn a `TraceEntry` into engine `Event`s through one
  shared correlation (`bridge::Correlation`: Tasmota FIFO, aether id map, boot
  tracking, `seq` dedupe), so replaying a trace reproduces exactly the events
  the engine saw. `telemetry` and `cfg` are not traced.
  The runtime also captures `choregos/decision` creation rows with the full view,
  `would`, and structured actions (not ring display strings); extraction retains
  them and replay ignores them, recomputing from inputs with ids starting at 1.
  Settlement traces use `choregos/verdict` with id, verdict and expectations;
  extraction retains them and replay ignores the recorded outcomes.
  Direct observations are `tv/state` with `{state, at}` and `probe/state`
  with `{role, state, at}`; replay consumes these known topics and extraction
  retains them. Older versions ignore them.

`Engine::new(config, now, restored, next_decision_id)` receives the seed and
persisted id counter without I/O. `record` allocates ids; the startup service
decision uses the same path. Restored
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
`sensor.choregos_last_decision` publishes the newest settled observable ring
row's verdict with id, role, reason and latency_ms attributes. No drift entity
is implemented in this step.

### 5.12 Events into the engine
`Frame`, `SendResult{to, data, status, attempts}`, `BridgeOnline/Offline`, `HaState{entity, state, attrs, changed: Option<DateTime<Utc>>, updated: Option<DateTime<Utc>>}` (tv, receiver, sonos, kodi — subscribed via `subscribe_trigger`/`state_changed`), `Tick(now)`, `Command(AllOff | SetMode | Identify(la) | Forget(la) | SendRaw{to,data} | ManualTakeover(role))`, `ConfigReloaded`.
`TvPower{state: On|Off|Unreachable, at: DateTime<Utc>}` is a direct observation
only, independent of the HA cache and room intent.
`ConsolePower{role, state: Awake|Standby|Unreachable, at: DateTime<Utc>}`
updates only the role's direct observation, never its CEC-derived logical power.
`Frame.ack` (observed ACK bit) and `Frame.t_us` (bridge-monotonic µs) are
`None` on Tasmota; the engine never reads them — the aether adapter already reduced a NACK-cut directed `rx` to header-only before the engine sees it (§1).
`SendResult.status: SendStatus = Ack | Nack | ArbLost | LineError | Busy`,
`attempts: Option<u8>`. Only `Ack` marks a standby verification acknowledged and
only `Nack` counts toward absent-target evidence; `ArbLost`, `LineError` and
`Busy` are neither (the target was never asked). Tasmota `{"Ack":true|false}`
maps to `Ack`/`Nack` with `attempts: None` (its `false` conflates NACK with
transmit aborts; unchanged behaviour).
Tasmota `SendResult`s correlate patched `{"Ack":true|false}` bridge results with
successfully queued `HdmiSend` publishes. Stock `"Done"` / `"Sending failed"`
and other non-ACK responses consume the FIFO entry without emitting an event:
they do not establish whether the target is present. Uncorrelated results are
ignored. Correlated results, including resync NACKs for absent LAs, are
debug-level logs rather than warnings.
The raw-send byte limit is a `BridgeConfig` property (14 Tasmota, 15 aether),
enforced by the engine's `SendRaw` validation and the UI form.
HA's parsed `last_changed` takes precedence; absent/invalid timestamps use injected
wall time on state-string changes, otherwise retain the cached timestamp.
HA `last_updated` independently timestamps attribute freshness; absent/invalid
values use injected observation wall time. Trace `ha/state` retains `updated`;
replay falls back to the line timestamp for older traces without that field.
The HA cache contains observed state only: sending a command never changes
its value or establishes when the device actually changed state.

## 6. Web UI (axum + askama + htmx + SSE; no JS build step)

Pages (all behind auth): 
- `/` Dashboard: mode switch (confirm when going active), bridge status, role cards (name, power, LA/PA, vendor/OSD, last event, "takeover" and "standby" buttons), TV/receiver/Sonos/Kodi HA states, active source, conflicts banner, "all off".
- `/bus` Live bus: SSE stream of frames (decoded: from/to names, opcode name, operands) interleaved with decisions; filters by LA/opcode; last 2000 kept in memory (ring buffer in `AppState`).
- `/devices` Bus table (every LA with learned fields, identified/unidentified, matched role) and the role → source mapping; "identify" and confirmed "forget" controls per non-bridge LA.
- `/decisions` Decision log with stable `d-<id>` anchors, parent links, reasons and resulting actions (and `would:` entries in shadow mode).
- `/config` Current file TOML verbatim (comments/order preserved; secrets are environment-only), edit + validate + save (atomic), reload.
- `/healthz` (unauthenticated) JSON: mqtt connected, ha connected, bridge online, mode.
Auth: OIDC (openidconnect crate, authorization-code + PKCE) with Authentik; local username/password break-glass form at `/login/local`; signed session cookie (`tower-sessions` or axum-extra signed cookies); CSRF for POSTs via same-site cookie + hidden token. Visual design: dark, dense, monospace for bus data; mocked before implementation (see `docs/ui.md` once written).

## 7. Operations

- Docker: multi-stage build, static musl binary, `FROM gcr.io/distroless/static`, non-root, `/healthz` healthcheck; writable `/config` holds `choregos.toml` and `choregos.state.json`, secrets via env. Published as `ghcr.io/yasen-pavlov/choregos` on tags, and `:edge` on every push to `main`.
- `deploy/docker-compose.yml`: standalone example (host port, config directory mount, env secrets, health check). Integration into a specific stack (reverse proxy, log driver, networks) is left to the operator.
- Logging: `tracing` JSON to stdout; decision log lines are structured (`event=decision id=… parent=… role=… from=… to=… reason=…`).
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
pub struct Snapshot { pub mode: Mode, pub warming: bool, pub bridge_online: bool, pub bridge_logical_address: u8, pub bridge_max_raw_bytes: usize /* selected adapter's raw-send limit */, pub bridge_bus: Option<String> /* aether: ok | stuck_low | no_pullup */, pub bridge_telemetry: Option<BridgeTelemetry> /* stale_after_s per adapter */, pub roles: Vec<RoleView>, pub bus: Vec<BusEntryView>, pub ha: HaView, pub active_role: Option<String>, pub intent: RoomIntent, pub tv_direct: Option<(TvState, DateTime<Utc>)>, pub conflicts: Vec<Conflict>, pub recent_frames: VecDeque<FrameView> /* direction Received | Sent | Local; ack: Option<bool> */, pub decisions: VecDeque<DecisionView>, pub connections: Health }
pub enum UiEvent { Frame(FrameView), Decision(DecisionView), Verdict { id: u64, verdict: Verdict, expectations: Vec<Expectation> } } // publish updated snapshot before events
pub enum Command { AllOff, SetMode(Mode), Identify(u8), Forget(u8), SendRaw { to: u8, data: Vec<u8> }, Takeover(String), Standby(String) }

// src/web/mod.rs  (ui owns; core mounts)
pub fn router(state: AppState) -> axum::Router;
```

All `*View` structs are plain `Clone + Serialize` data (no `Instant`s; use
`chrono::DateTime<Utc>` and humanised strings computed in templates).

## 9. Testing contract

- `src/cec/*`, `src/identity.rs`: unit tests, no I/O. Engine behavior tests live in `tests/replay.rs`, driven by `Event`s with injected monotonic and wall times.
- `tests/replay.rs`: feeds `fixtures/*.jsonl` through `BusModel` + `Engine` with the reference config and asserts the decision sequence (Kodi off → streaming box on → takeover actions → streaming box off → off-timer → TV/receiver off → streaming box on again → volume keys forwarded only when Sonos playing).
- Runtime adapters (`mqtt`, `bridge/*`, `ha`, `wol`): thin; tested with a local broker (`rumqttd` in-process) and a fake HA websocket server in tests where cheap, otherwise exercised by the lead's smoke run against the real stack in shadow mode. Bridge codecs are unit-tested per kind on raw payloads (status/boot transitions, seq dedupe and gaps, id correlation and expiry, every `tx` status, rejected/local/err/line events, telemetry) and replayed through `bridge::Correlation` from trace lines so live and replay cannot diverge; `fixtures/aether-*.jsonl` are real captures from the board on the reference bus.
- Web: handler tests with `axum::body` + a fake `AppState` for the pages; auth middleware tests (unauthenticated → redirect, local login happy path, CSRF rejection).
- No tests of wiring/copies; coverage target on new code ≥ 80 % honestly.
