# Decisions

Deviations from DESIGN.md agreed in review. Newest first.

## 2026-10-29 — Physical address changes keep the role

- During receiver power-up, a running console at LA 8 announced PA 2.3.0.0,
  temporarily changed to 1.0.0.0, then returned to 2.3.0.0. Replacing its
  entry on each change erased vendor/OSD and briefly lost its role binding.
  The second identity gap exceeded the release delay, switching off the room
  while the console was still running; later identity traffic woke it again.
- Treat a differing PA in `84` (at least three operands) or broadcast `82`
  (exactly two operands) as a same-device PA change only for a verified,
  non-bridge entry below LA 15 with both vendor and OSD known, bound to a role
  that is currently On. Keep its keys, power, buffered signals and pending
  standby verification. Update PA and last-seen time; take device type from
  `84`, and remap active_source when it held the old PA. `84` refreshes
  verification and confirmation time as usual; `82` does not. Unknown keys,
  bridge traffic and unverified restored entries retain replacement behavior:
  stale cache keys are not evidence of bus identity.
- Continuity is needed only for a running device. An Off role at a reused LA
  provides no same-device evidence. Keep its replace → buffer → rebind path,
  so a new device's PA announcement cannot wake the previous occupant's role
  before its differing vendor/OSD replies identify the actual new device.
- Retained keys are provisional, not an assertion that a logical address can
  never be reused. Queue all identity queries through the existing manual
  pending path and shared one-second slot, including in shadow mode. A
  differing vendor or OSD reply still invokes ordinary replacement, clearing
  the other keys and requiring fresh PA verification before role matching.
  This is the same safety argument as provisional keys during an LA move.
- Detect another LA already holding the announced PA first; existing
  address-move inheritance and verification retargeting take precedence.
  Vendor/OSD-matched roles remain bound through a same-device PA change.
  A physical-matched role may legitimately unbind and start release because
  its configured identity key is the PA itself; it can rebind when PA returns.

## 2026-10-27 — Forget a bus entry from the UI

- A disconnected adapter's learned identity remained restored and unidentified
  across service restarts, and automatic identification kept querying its stale
  logical address. Provide an operator-controlled forget action on every
  non-bridge bus row instead of requiring an offline state-file edit.
- `Forget(la)` is a local model edit, including in shadow mode: remove the bus
  entry together with its identification backoff/queued manual query, buffered
  power signals and pending standby verification. Record `command_forget` with
  no actions; send no CEC frames. The next state save omits the identity.
- Recompute identity matching immediately. A role that loses its binding uses
  the existing `identity_lost` transition: an On role becomes Off, clears its
  active-role/presence state and starts the normal room-release timer.
  If forgetting resolves a conflict and binds a role to another LA, replay that
  LA's latest relevant buffered signal through the normal role transition.
- Do not blacklist the address: logical addresses can be reused, and later
  traffic must create a fresh entry and trigger ordinary identification.
- Reject addresses outside 0–14, the configured bridge address and unknown
  entries. Also reject forget while the bridge is warming: role transitions
  are deferred then, so removal could otherwise leave an On role unbound.
  Publish the configured bridge address in snapshots so the UI hides forget
  on the bridge even when it uses a nonzero logical address.

## 2026-10-27 — Native webOS power-off

- After native Wake-on-LAN, the HA webOS integration needed about 30 seconds
  to reconnect. A release 23 seconds after wake called `media_player.turn_off`
  while the TV entity was still unavailable: the receiver turned off, but the
  TV stayed on. A direct paired SSAP connection completed in about 0.23 seconds.
- Add a transport-independent `TvOff` action. Existing configurations retain
  HA power-off; `tv.off.kind = "webos"` connects directly over `wss` (default
  port 3001) and registers with an environment-only key. Before each power-off
  attempt, query `ssap://com.webos.service.tvpower/power/getPowerState`; `Power Off`,
  `Suspend` or `Active Standby` succeeds without sending `ssap://system/turnOff`,
  which can wake a TV in Active Standby. Unsupported-service/error responses
  fall through to turnOff for older webOS. Unknown keys require pairing and
  are never retried, avoiding repeated pairing prompts. There is no HA fallback.
- LG exposes a self-signed certificate without a public trust chain, so the
  TV-only rustls verifier accepts its certificate. TLS handshake signatures
  remain verified, but server identity is not authenticated: use a trusted LAN.
  Explicitly select the ring crypto provider already used by the dependency graph.
- Bound each entire exchange to five seconds. The ordered executor attempts
  native off three times, waiting one second between attempts for the TV's
  network stack after wake. Warn on each failure and error after the last;
  diagnostics never echo credentials, server replies, or connection addresses.
- After a successful turnOff acknowledgement, websocket close is best-effort:
  a TCP reset while the TV powers down must not trigger another power command.
- Treat `tv.off` as a restart-only credential destination, like `tv.wake`, so
  the live config editor cannot redirect resolved environment secrets.
- Copy aiowebostv's public registration payload unchanged; upstream now keeps
  it in `aiowebostv/handshake.py`, imported by `webos_client.py`.

## 2026-10-27 — Logical address moves keep the role

- During receiver HDMI re-enumeration, a console moved from LA 8 to LA 11
  while keeping its PA. The destination held a different restored identity.
  Dropping LA 8 and clearing LA 11's keys temporarily unmapped the running
  role. The roughly two-second vendor/OSD gap looked like power-off, starting
  the room-off timer before a later Active Source woke the room again.
- A valid `84` (at least three operands) or broadcast `82` (exactly two
  operands) reporting another LA's PA moves that physical device: remove the
  old LA, perform any destination replacement, then inherit physical address,
  vendor, OSD, device type, power and confirmation state.
  Apply the current report afterward, updating last-seen time and recomputing
  identification. Identity follows PA plus keys, not the logical address.
  Vendor/OSD reports alone never imply a move; a subsequent changed key still
  invalidates the inherited identity and requires fresh PA confirmation.
- If the destination is not being replaced and already has vendor/OSD keys
  that differ from the old entry, do not inherit: evict the old LA but retain
  the destination's own keys. Evidence already supplied by the new LA takes
  precedence over the assumption that the device moved.
- Inherited keys are provisional until the new LA supplies its own replies.
  Queue a full `83`/`8C`/`46` identify via the existing manual-pending path,
  subject to the shared one-second send slot, including in shadow mode.
  This lets a different device on the same HDMI port or behind a non-CEC
  switch correct a provisional role through normal key replacement.
- For `82`-first moves, inherit `verified` and `confirmed_at` from the old
  entry: the same PA identifies the same HDMI port, so a previously verified
  role stays continuously mapped even before `84` arrives. `82` does not
  create or refresh a confirmation; a moved unverified restored entry keeps
  its cached keys but remains unverified until `84`. Only `84` refreshes
  `confirmed_at`. Do not transfer restore-reconfirmation flags.
- Identification retry timing and attempts stay destination-LA-local, with
  the existing replacement resets; a move queues full identification there.
  Transfer the old LA's buffered power signals after discarding the destination
  occupant's signals. Treat already-mapped roles as continuously bound, so a
  move alone
  cannot replay an old wake signal and undo a manual standby; unmatched roles
  still consume their migrated buffered wake when identification completes.
- Retarget pending standby verification to the new LA, retaining sequence,
  attempts and deadline but resetting send/ACK/NACK accounting because old
  results refer to the old target. Discard the destination occupant's pending
  verification only on a move, not on a plain vendor/OSD replacement; ordinary
  remapping is handled by verification at its deadline. Presence
  timers/misses/awaiting flags are role-keyed; the move
  frame clears awaiting/misses through normal presence reconciliation and the
  next poll uses the new mapping. Takeover settle and active-role state are
  also role-keyed and remain intact.

## 2026-10-26 — Wake frames re-arm identification

- A device waking shortly after service restart can remain restored/unidentified
  for a minute or more: the startup `83` and tick retries went unanswered while
  it was asleep, and wake announcements did not shorten the backed-off deadline.
- After applying `04`/`0D` to the bridge, broadcast `82` with exactly two
  operands, or broadcast `85`, re-arm identification for a non-bridge LA below
  15 that is still unidentified. If its last query is absent or at least
  `identify_retry_s` old (capped at one hour), clear that instant and reset
  attempts. Compare saturating elapsed time with the capped base interval to
  avoid overflowing an instant for extreme configured retry durations.
  Apply the same rule after replacement: a different occupant announcing its
  PA is awake too.
- Use the base interval for the guard, not the backed-off interval: otherwise
  the wake would still wait for the very deadline it should bypass. Once sent,
  the new query records its instant, bounding a wake burst to at most one
  re-arm per base interval per LA, even under broadcast `82` flapping;
  silence then resumes the base interval followed by exponential backoff.
- Keep the shared one-second send gate and request only missing required keys
  (just `83` for a fully cached restored identity). A gated re-arm remains due
  for a later tick. Identified entries and non-wake traffic retain their
  existing behaviour; no configuration key or decision reason is needed.
- Split bus handling into private duplicate-eviction, replacement, learned-key,
  report-application and wake-classification helpers to keep cognitive
  complexity below the Sonar limit without copying the device map.

## 2026-10-26 — Leave Request Active Source replies to the active source

- Never answer broadcast Request Active Source (`85`) from the fake TV, even
  when the bus model knows an active source. Only the current active source
  device may answer with Active Source (`82`) for its own physical address;
  a TV claiming another device's address is not CEC-compliant.
- A live activation flow showed a recording device broadcast `85`, followed by
  a bridge reply `82 <stale PA>` and the recording device's `9D <PA>`. Later
  controlled tests identified Power Off Function's broadcast Standby as the
  cause of the receiver shutdown and recording-device dropout, rather than
  this reply alone. Keep the reply removal independently for CEC compliance;
  bridge firmware must likewise avoid replying on another source's behalf.
- Keep directed `8F`/`9F`/`46`/`91` replies under `fake_tv.replies`. This
  supersedes the earlier decision to retain a service-owned broadcast `85`
  response. Startup/resync still sends `85` to solicit the real source's reply;
  received `82` learning is unchanged.

## 2026-10-26 — Pause before Standby for playback-resistant sources

- Directed Standby (`36`) is ACKed but ignored by the streaming box while
  playing; it works at the idle menu or after playback is paused. Repeated
  back-to-back live tests with another source active showed that directed
  deck-control Play: Still (`41 25`), Standby (`36`), then Give Device Power
  Status (`8F`) stops it during playback without an activation side effect.
  Expose this as `cec_pause_standby` without changing `cec_standby` for devices
  that already honour Standby.
- Reject the user-control Pause key (`44 46` followed by `45`): keys sent by
  the TV make the box assert Active Source (`82`) before honouring Standby,
  briefly stealing the input from the new source. Deck-control Play: Still
  pauses without that side-effect frame.
- Reject Power Off Function (`44 6C`): although it sleeps the box, when the box
  is not the active source it broadcasts Standby to the bus. That shuts down
  the receiver and makes the recording device announce Inactive Source
  (`9D <PA>`), explaining the observed recording-device dropout. It is not a
  safe takeover policy.
- Play: Still followed by directed Standby instead produces Inactive Source
  (`9D <PA>`) and Standby power reports (`90 01`), with no broadcast Standby
  or stray Image View On. Do not suppress `04`/`0D` wakes during verification:
  they keep normal bus learning and role-On cancellation, as does `82`.
  Retain the existing `90 xx` role/buffered-signal suppression while learning
  those replies for verification.
- Remember the entire stop/query sequence for retries and ACK/NACK accounting:
  two frames per round for `cec_standby`, three for `cec_pause_standby`.
  An observed `9D` or `90 01` confirms standby even if the trailing `8F` NACKs
  because the box is asleep. An ACKed `36` prevents absence classification;
  without an observed power report the result remains unconfirmed.
- Keep the independent Request Active Source (`85`) reply removal above:
  only the actual active source should announce its own physical address.

## 2026-10-26 — Bound restored-identity identification traffic

- Standby devices may ignore `83` while answering `46`. Re-querying all keys
  and resetting retries on every reply created a self-sustaining traffic loop
  capable of crashing the bridge. Request only missing required keys; a fully
  keyed unverified restore requests only `83`, including its first automatic
  attempt. Do not add a first-attempt `8C`/`46` burst.
- Permit opportunistic verification of a fully keyed restored identity once
  both its cached vendor and OSD are re-confirmed, in either order. Accept
  `87` and `47` from the restored LA regardless of destination: devices may
  address these replies to the receiver rather than the bridge. This accepts
  standby/wake evidence without soliciting traffic. Differing values retain the replacement
  path, clear re-confirmation evidence, and still require a fresh `84`.
  Identities missing a cached PA/vendor/OSD cannot use this shortcut.
- Reset attempts only for identification progress (a genuinely new key or
  restored-identity verification) and resync/bridge-online, never arbitrary frames
  or repeated unchanged keys. A single matching OSD/vendor reply is not progress.
  Replacements retain the LA's last-query instant and attempts, including across
  a fresh `84` re-confirming a PA retained after a vendor/OSD change; otherwise
  alternating identity values could sustain one identification burst per second.
  Preserve the 10, 20, 40, … second sequence and one-hour cap: after about
  ten minutes the retry interval is about ten minutes, not one hour; the cap
  is reached after about 85 minutes of cumulative silence.
- Rate-limit automatic and manual identification together in the pure bus
  model using injected time: one LA per event and at least one second between
  bursts. Keep deferred manual requests pending rather than dropping them.
  Include the shared slot in identification deadlines, choosing earliest due
  entries with LA tie-breaking, so due devices do not cause a busy tick loop
  or starve behind more recently due entries.
  Record acceptance of queued identify commands immediately (with no sent
  actions yet), preserving the command log/persistence contract while they wait.
- Retain the existing one-off resync burst, but reserve its following second
  before allowing retries. Count each unverified entry's resync `83` as its first
  query, so its next PA retry waits the full base interval rather than one second.
  This bounds identification load without changing startup interrogation or
  introducing executor-side timing/I/O.
- Re-confirmation flags are transient. Do not change persisted state format,
  `confirmed_at`, or its lifetime: only received `84` refreshes confirmation.
  Same-model swaps with identical vendor/OSD remain indistinguishable by this
  evidence; accept that tradeoff instead of keeping silent standby entries
  permanently unverified.

## 2026-10-26 — Bridge send results and startup log noise

- Devices that leave the CEC bus in standby legitimately NACK Standby and
  power-status queries, including startup resync. Blind bridge-NACK warnings
  obscured the distinction between absent devices and devices still on the bus.
  Only patched `HdmiSend.Ack` responses emit `SendResult { to, data, ack }`
  engine events. Stock `Done` means queued, while `Sending failed` means the
  bridge refused to transmit (busy or no LA claimed): neither reports the
  target's ACK/NACK. Correlated replies log at debug, never warn for NACKs.
- The bridge answers sends in publish order. Keep at most 64 outstanding
  successfully queued sends, dropping the oldest on overflow; discard entries
  older than five seconds before consuming a reply. Every `HdmiSend` reply,
  including stock responses and firmware errors such as `Invalid JSON`,
  `Buffer empty`, and `Buffer too large`, consumes an entry to preserve order.
  Clear on bridge Offline/Online, MQTT disconnect/reconnect, and any config
  reload (which replaces the transport). Results never gate safety.
- Pending standby verification counts outgoing `36`/`8F` frames and their
  correlated NACKs, and remembers any ACK. After the same bounded retries,
  `sent > 0 && !acked && nacked >= sent` becomes `standby_target_absent`;
  extra or late NACKs cannot override an observed ACK. Mixed ACKs or missing
  results without a confirming power report remain `standby_unconfirmed`.
  ACK-based absence classification requires patched firmware; stock firmware
  can only produce `standby_unconfirmed`. Neither NACK nor silence confirms
  standby or suppresses retries. Observed Standby/Transition and real role On
  retain confirmation/cancellation semantics; reload clears pending checks.
  Results outside pending verification (such as resync) are ignored.
- Suppressed shadow-mode standby sends never enter the transport FIFO.
- Do not publish retained states while the runtime's MQTT connection is down.
  Clear the state cache on connect so the latest values publish then, avoiding
  startup noise rather than lowering the severity of real dropped publishes.
  Disconnected-drop logging remains warn; no process-wide connection flag.
- Raw-send validation accepts at most 14 data bytes, matching firmware's
  rejection of payload lengths of 15 or greater.

## 2026-10-26 — Standby verification after takeover

- A live takeover on 2026-10-26 sent Standby during the incoming source's
  announcement burst, but the displaced source stayed on; a later manual
  Standby worked. The previous Node-RED automation had the same failure.
  Bus arbitration/NACK or a device ignoring a mid-burst command can explain
  the race, so sending once is not evidence that standby took effect.
- Send Standby followed immediately by Give Device Power Status, including
  all-off and manual standby through the shared stop path. Verify after
  `takeover_settle_ms` and retry that pair at the same interval, bounded by
  `timing.standby_retries` (default 2, range 0–10); record `standby_retry`,
  then `standby_unconfirmed` after N retries without confirmation.
- Confirmation requires observed Standby or Transition power from that LA.
  Silence and Unknown power are not standby evidence: absent devices exhaust
  the bounded retries rather than being assumed off.
- During pending verification, `8F` replies (`90 xx`) update bus power only,
  never role signals or buffered signals. An unchanged On reply must not
  wake the displaced role, stop the new source, or be replayed after warming.
  Real On events still cancel verification to respect a user wake.
- Cancel blocked/remapped identities and clear pending checks on bridge-offline
  or config reload. Do not register verification in shadow mode, since Standby
  is not executed there; entering shadow also clears pending checks to avoid
  misleading retry decisions.
- Residual risk: a retry deadline may land between a waking device's
  non-on-event frames (`84`/`87`, depending on its configured on list) and its
  on-event. This bounded verification does not distinguish that burst window.

## 2026-10-25 — Restart resilience and directed-broadcast WoL

- Identification requires PA plus the union of OSD/vendor keys used by configured
  match rules, recomputed on reload. Only missing keys are queried on frames and
  timer deadlines; retries back off exponentially to one hour and pause while
  the bridge is offline. Frames and resync reset backoff; resync clears query
  timestamps so its replies are not suppressed by pre-reconnect timing.
  Manual identify requests all keys, excluding the bridge and LA 15, and records
  its query instant to avoid an immediate automatic duplicate.
- Keep disk I/O out of the engine with constructor-injected learned identities,
  not a runtime restore event. Restore Unknown power, no last-seen time and
  verified=false. Cached roles must wait for an `84` confirming the stored PA;
  buffer intervening power signals, and send `83` at resync immediately.
  This prevents stale vendor/OSD data from misclassifying an LA's new occupant.
  Duplicate restored physical addresses keep the earlier LA.
- Treat differing Active Source PA like differing Report Physical Address.
  A changed vendor/OSD invalidates the other non-PA keys and old signals and
  requires a new `84`; otherwise a swapped device can inherit stale role evidence.
- Runtime-owned version-1 `choregos.state.json` beside the config retains only
  learned identities and the newest 2000 redacted decisions, plus a runtime-added
  `service_started` boundary per start. Replay uses only the pure engine and
  therefore never opens the state file. Corrupt/unknown files warn and reset.
  Persist only 84-confirmed identities, excluding bridge/empty entries, with an
  injected-wall `confirmed_at`; drop confirmations older than 30 days on load
  so unplugged historical devices cannot live in the cache indefinitely.
- Serialize changed state into a single-slot watch channel and let one writer
  perform atomic I/O off the event loop. Newest state wins, so slow fsync cannot
  block CEC replies or grow a queue. Failed writes warn once and retry after 60
  seconds or on a new state; recovery logs once. Shutdown flushes the last state.
  Reuse config's path-and-bytes writer with rename as the commit point.
- Correct the earlier m9 deployment restriction: direct WoL works from a bridge
  container with a directed broadcast (e.g. `192.168.1.255`) when the router
  forwards it. Host networking or HTTP wake remain alternatives otherwise.

## 2026-10-25 — Shadow-operation core papercuts

- Presence timeout marks the retained bus entry Unknown as well as its role
  Off. Silence is not an observed standby report; retaining identity supports
  discovery and the next normal power-on frame without inventing bus evidence.
- HA state timestamps preserve parsed RFC 3339 `last_changed` values in UTC.
  Missing/invalid timestamps fall back to injected wall time only when the
  state string changes; attribute-only updates keep the previous timestamp.
  Optimistic TV/receiver off writes preserve the last HA-observed timestamp:
  a command is not an observation that the physical device changed state.
- The config editor retains the actual file text at startup, after successful
  saves, and after mode-only span replacement. Snapshot publication no longer
  regenerates TOML, so saving does not discard comments/order or add the
  `check-config` environment-secret footer. `Config.secrets` is serde-skipped
  in both directions, its secret wrappers are not deserializable, and config
  tables deny unknown fields: supported credential values come only from
  environment resolution, not TOML. Operators must still never put secrets in
  arbitrary comments or public string settings.
  Startup reads the file once and passes the parsed config and original text
  together through the runtime to the infallible `AppState::new(config, text)`;
  there is no normalized-text fallback or second-read race.
- Atomic rename is the config-save commit point. Directory fsync failures
  after rename warn about crash durability but report success, keeping the
  running model/editor synchronized with the already-replaced file. Failures
  before rename still reject the save.
- Engine transition coverage stays together in `tests/replay.rs`, reusing its
  event helpers instead of introducing a duplicate unit-test harness.
- Native amd64/arm64 digest builds also publish `:edge` for `main`; semver and
  `latest` remain tag-only and GitHub release notes/releases are tag-only.
  Per-ref non-cancelling concurrency keeps each manifest build intact without
  conflating independent refs; image inspection uses metadata's actual version.

## 2026-10-25 — Shadow `would:` prefix rendered once

- The runtime's per-action `would: ` prefix in `DecisionView.actions` is the
  single source of truth; `/decisions` and the `/bus` stream no longer add a
  second template-level prefix (shadow rows showed `would: would: …`).
- `DecisionView.would` still marks the whole row (amber arrow). The dashed amber
  action style now applies only to actions whose string starts with `would: `,
  so identification, resync and presence actions on a shadow row, which do run,
  are not shown as suppressed.

## 2026-10-25 — Real-adapter runtime coverage

- Extract the production scheduling loop into a private method accepting a
  shutdown future. Production supplies the existing OS-signal future; tests use
  a one-shot, exercising the same channel/timer dispatch and snapshot updates.
- Runtime tests use rumqttd's actual Router/Server inside an owned Tokio runtime
  because `Broker::start` exposes no shutdown handle. Dropping that runtime
  closes the listener and every connection for actual disconnect/reconnect tests.
- Loopback HA authenticates, returns initial states and acknowledges/records
  services. All wake packets target an ephemeral loopback UDP socket; environment
  credentials and installation endpoints are never used.
- Timing reload is observed through the config view and the resulting snapshot
  decisions at the new deadline; the public snapshot DTO is not expanded merely
  for coverage. Tests use bounded observations and explicit input barriers.

## 2026-10-25 — Sonar maintainability refactor

- Replace production wildcard imports with the explicit opcode and state types
  each module consumes.
- Split configuration validation by existing sections and device matching/policy,
  preserving validation order and error messages.
- Dispatch engine events through named handlers; keep frame learning, identity
  reconciliation, volume forwarding, warming and timer phases separate while
  retaining their action/decision ordering and injected-time behavior.
- Group runtime loop state in a private owner with input, command, connection,
  reload, effect and snapshot methods. Adapter ownership is temporarily absent
  only while draining/restarting during a config reload.
- This is a behavior-preserving refactor with no public interface changes or rule
  suppressions. Extracted engine phases reuse action buffers rather than adding
  per-frame temporary action allocations.

## 2026-10-25 — Lead-approved core/runtime review corrections

Engine review decisions:
- E1: successful takeover policies mark the displaced roles Off with
  `taken_over`, without starting an off timer. Otherwise silent standby or HA
  media-stop targets keep the room on forever. Kodi also accepts `power_standby`.
- E2: bridge/MQTT offline pauses presence queries and miss counting; Online
  resync resets outstanding probes so transport outages cannot imply power-off.
- E3: a physical address has one owner. A new Report Physical Address removes
  stale entries on other LAs and their signal history. Conflicts inhibit only
  involved roles; unrelated settle/off timers continue.
- E4: resync defers pending settle/off timers through warming instead of deleting
  them; retained Online messages must not lose an in-progress release/takeover.
- E5: room-off actions optimistically mark cached TV/receiver state off, allowing
  a quick re-wake before HA's state_changed response arrives.
- E6: never answer directed requests from unregistered LA 15.
- E7: every `handle(event, now, wall)` receives fresh UTC wall time, independently
  of monotonic timer time, so host-clock corrections affect the next preset.
- Remove redundant warming checks and duplicate command/transition action logs.

Runtime review decisions:
- M1: `web`, `mqtt`, `home_assistant`, and `tv.wake` changes require restart.
  These sections choose authentication state or secret destinations; a live
  editor must never redirect credentials or select a different environment secret.
- M2: MQTT disconnect becomes an engine BridgeOffline event, not a field write.
- M3: presence power-status queries are read-only and allowed in shadow, like
  Identify/Resync. DESIGN §5.10 is amended; suppressing them falsified shadow state.
- M4/M5: all CEC effects use an immediate, non-blocking MQTT fast lane; disconnected
  or full queues drop with warnings, and disconnect discards queued CEC requests.
  Only HA calls and TV wake use the ordered worker, admitted with try_send.
- M6: delete snapshot broadcast events. The snapshot is written once per input
  into the RwLock; bounded frame/decision rings use VecDeque. This supersedes the
  earlier boxed-Snapshot decision and avoids per-client deep snapshot clones.
- M7: HA websocket pings every 30 seconds and reconnects after 60 seconds silence.
- m1/m2: MQTT logs changed errors and reconnects exponentially; HA disconnect
  invalidates tracked entity states to unknown so independent WoL still runs.
- m3/m4: handle SIGTERM and bound HTTP/SSE draining; config reload drains the
  ordered worker with a timeout and requests clean MQTT shutdown before restart.
- m5: role IDs use `[a-z0-9_]+` and exclude built-in discovery/state names.
- m6/m7: removed-role discovery and state are cleared with retained tombstones;
  discovery uses `default_entity_id`, not deprecated `object_id`.
- m8: mode persistence replaces only the TOML mode value using its parsed span,
  preserving comments, order, and unrelated manual edits.
- m9 (corrected by the restart-resilience decision above): bridge-network direct
  WoL works with a directed broadcast the router forwards. Use host networking
  or HTTP wake otherwise; README and compose describe the alternatives.
- m10: remove no-op PersistMode; Resync is payload-free with fixed query expansion;
  share Config using Arc; the retained bridge LWT initiates startup resync once.
- `fake_tv.replies` defaults true. Firmware-owned directed replies can be
  disabled. The former service-owned broadcast `85` response is superseded by
  the 2026-10-26 Request Active Source decision above.
- CLI operations move to tested runtime/cli.rs; main.rs remains parsing/dispatch
  and is explicitly excluded from Sonar coverage, not the runtime implementation.

## 2026-10-25 — Web UI slice

- Vendored htmx `2.0.11` (`src/web/static/htmx-2.0.11.min.js`, sha256
  `d6fdc75f204e6bdefa99b69bf1e6d4ac69b8a364f77929f45c13476b4000f717`) and the
  htmx SSE extension `htmx-ext-sse 2.2.4` (`src/web/static/htmx-ext-sse-2.2.4.min.js`,
  sha256 `98a46496de0c3605fbffdce9167ba427bdd9553184f83f149c261891a92c0136`),
  downloaded from unpkg (`dist/htmx.min.js`, `dist/sse.min.js`) and checked
  byte-identical against jsdelivr. Assets and the stylesheet are compiled into
  the binary with `include_str!` (the distroless image has no files beside the
  binary); versioned files are served `immutable`, the CSS `no-cache`.
- Askama templates live in `src/web/templates/` (configured in `askama.toml`).
  Template helper calls receive field arguments by reference.
- Sessions: a small bounded in-memory `SessionStore` (`src/web/session_store.rs`,
  1024 sessions) instead of `tower-sessions`' `MemoryStore`, which never removes
  expired records. It purges expired records, caps anonymous sessions (login
  form, pending OIDC) at a 15-minute idle lifetime, and when full evicts the
  least recently saved anonymous session; authenticated sessions are never
  evicted, new ones are rejected instead. Sessions are saved on every request
  (`always_save`), so the 7-day expiry is a true inactivity timeout. Signed
  cookie `choregos_session`: HttpOnly, SameSite=Lax so the IdP's top-level
  callback carries it, Secure unless `public_url` parses with scheme `http`
  (the parsed, normalised scheme is used; an unparsable URL fails closed and
  disables SSO). Sessions do not survive restarts. The `cookie` crate needs a
  64-byte key and its `key-expansion` feature is not enabled, so the key is
  derived from `CHOREGOS_SESSION_SECRET` with low-cost Argon2id (the secret
  itself is high-entropy). Login cycles the session id and rotates the CSRF token.
- CSRF: one random token per session, required on every POST (login and logout
  included) as the `X-CSRF-Token` header (htmx `hx-headers` on `<body>`) or a
  `_csrf` urlencoded field; comparison is constant-time. Unauthenticated htmx
  requests get `401` + `HX-Redirect: /login` instead of a `303`.
- OIDC: routes `/login` (chooser), `/login/oidc`, `/auth/callback`,
  `POST /logout`, `/login/local`. Redirect URI is `<public_url>/auth/callback`;
  scopes `openid profile email`; PKCE S256; nonce and `at_hash` verified.
  Discovery is lazy (the IdP may be down at startup), cached for one hour so
  rotated signing keys are picked up, and HTTP redirects are not followed.
  Every user the IdP authenticates for this client is allowed; restrict access
  with an IdP application policy. Logout is local only (no RP-initiated logout).
  A callback's `state` is compared with the pending transaction before anything
  is consumed (error callbacks included), so an uncorrelated callback cannot
  cancel a genuine login; a matching transaction is consumed once, before the
  code exchange.
- The break-glass password is Argon2id-hashed (default parameters) when the
  router is built and verified on a blocking thread; the username comparison is
  constant-time and the hash is always verified to avoid user enumeration.
  Attempts are rejected (`429`, `Retry-After: 60`), not queued, beyond 10 per
  minute (one global fixed window; behind a reverse proxy the client address is
  not meaningful) or 2 concurrent verifications; the semaphore permit lives
  inside the blocking closure so a cancelled request cannot release it early.
- `POST /config` parses and validates with `Config::parse` before calling
  `save_config`, so rejected text never reaches the writer; a validate-only
  action is offered. Errors show `ConfigError`'s message, which never echoes source.
- Only `/bus` uses SSE (`/bus/events`, filters mirrored from the page query).
  Each stream subscribes first, then sends a `reset` event that re-renders the
  table from the bounded snapshot; live `row` events follow, skipping anything
  the reset already covered (by timestamp). A lagging subscriber gets another
  reset, so reconnects and lag never leave silent gaps and no second event log
  exists. Live rows accumulate in the open page until the next reset.
- The dashboard polls itself every 5 s (`hx-select="#dashboard"`) and swaps the
  header's `#status` badges (mode, warming, bridge/MQTT/HA) out of band from
  the same response, so the header and the controls cannot disagree. Every
  control in the polled region has a stable id, which htmx uses to restore
  keyboard focus after the swap. Role path segments use `urlencode_strict`.
- Responses carry CSP (`default-src 'self'`, `frame-ancestors 'none'`),
  `X-Frame-Options: DENY`, `nosniff`, and `Referrer-Policy: same-origin`; htmx
  runs with `allowEval: false`. HTTP spans record method and path only, since
  callback query strings carry authorization codes.
- Web tests build `AppState` from a TOML string and resolve secrets through
  `Config::resolve_secrets` with a closure rather than `std::env::set_var`
  (unsafe in Rust 2024). `tower` (already locked via axum) is a dev-dependency
  for `ServiceExt::oneshot`. The OIDC test uses an in-process fake discovery
  server; the token exchange leg needs a signing IdP and is left to the smoke run.

## 2026-10-25 — Runtime reload and deterministic replay

- With lead approval, web/auth setting edits are rejected by the live config
  editor with an explicit restart-required error. `AppState::auth()` returns
  a stable reference used by the UI; all other settings reload live.
- Physical addresses cannot be converted to logical addresses. Startup polls
  the standard playback/recording LAs plus currently resolved configured roles;
  physical-only match rules resolve when their Report Physical Address arrives.
- The capture starts before Kodi identifies itself and ends with volume keys,
  not a second wake. Replay acceptance tests explicitly seed pre-capture Kodi
  identity/on state and append a synthetic wake; the real capture is unchanged.
- HA websocket service failures are acknowledged and logged without replaying
  stale actions after reconnect. Effects run separately from the pure event
  loop so websocket latency cannot block engine timers.

## 2026-10-25 — Initial crate boundaries

- Pin direct dependencies to stable versions discovered with `cargo search`;
  retain `Cargo.lock` for reproducible binary builds.
- Use Askama 0.16 with axum's `Html` response wrapper. The former
  `askama_axum` crate is deprecated and current Askama has no framework-specific
  integration feature. Templates render explicitly into `Html`.
- Use `openidconnect`'s re-exported reqwest client with rustls instead of adding
  a second direct HTTP client. Sessions use `tower-sessions` with an in-memory
  store and signed cookies. Include an embedded, plaintext-only `rumqttd`
  dev-dependency for future isolated adapter tests.
- Keep resolved credentials in non-serializable, redacted-debug secret wrappers;
  configuration TOML contains only public settings and environment variable
  names. Configuration errors do not echo rejected TOML source.
- The scaffold deliberately returns unavailable errors for commands and config
  edits. `run` and `replay` exit 2 without performing I/O. Implementers replace
  these boundaries; no user action is silently acknowledged.
- UI addresses use dotted/hex strings, timestamps use UTC, and role/bus power
  states use explicit enums. `HaView.media_players` holds configured Kodi or
  other media-stop entities without hard-coding installation-specific roles.
- With lead approval, `UiEvent::Snapshot` carries `Box<Snapshot>` rather than
  inline `Snapshot`. The full snapshot made the enum 440 bytes versus the next
  largest 144-byte variant. Boxing only that variant preserves plain view DTOs,
  keeps event traffic compact, and avoids suppressing `large_enum_variant`.
