# Decisions

Deviations from DESIGN.md agreed in review. Newest first.

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
- `fake_tv.replies` defaults true. Firmware-owned directed replies can be disabled
  without disabling the service's Active Source response to broadcast `85`.
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
