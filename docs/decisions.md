# Decisions

Deviations from DESIGN.md agreed in review. Newest first.

## 2026-11-08 — Phase 2 slice 4: Config

- Document-preserving edits: the form never serialises `Config` back to text. Each edit
  is applied to the on-disk TOML document with `toml_edit` (pinned `=0.22.27`, already in
  the lock through `toml 0.8`; only the direct-dependency edge is new). A replaced value
  keeps its comment and spacing; nested records (`match`, `probe`, `receiver.observe`) are
  written through `TableLike`, so `match = { … }` and a `[device.match]` table are both
  edited in place, and a key inserted or removed reformats only an inline record; a value
  equal to what the file already means (an absent key at its default included) is not
  written; list edits keep the file's order and append new entries. A removed `[[device]]`
  (or slot) hands the comment block above its header to whatever now follows (the next
  header, else the end of the file): it may document more than that role. Rows written as
  an inline array (`device = []`, `device = [{ … }]`) stay as written until a row changes,
  then become `[[device]]` tables carrying the key's comments; removing the last row leaves
  `device = []`, since `Config` requires the key. A missing `[trace]`/`[verify]` table is
  created after its predecessor in `Config` order, only when a value differs.
- String fields render as one-row `<textarea>`s styled as inputs: an `<input>` drops line
  breaks from its value, so posting a section would rewrite an untouched multiline string.
  Text compares by trimmed lines, so a browser's CRLF is no change; a newline after the
  start tag keeps a value's leading newline through the HTML parser (the raw editor too).
- The field table (`view::config::FIELDS`, keyed by TOML path) holds labels, units, help,
  defaults and the integer bounds `Config::validate` enforces; unit tests check that each
  key is in `choregos.example.toml` (live or in its comments), that every rendered field
  has a row and every row renders, and that every bound is no looser than `validate`.
  Editable: `bridge` (kind, topic or node, logical address), `trace`, `tv.entity`,
  `receiver` (entity, select_source, observe), `presets` (timezone, slots add/remove),
  `sonos` (entity, mute switch, volume_from), `timing`, `verify`, `fake_tv` (replies,
  osd_name; power_status and cec_version are shown as fixed values), `[[device]]`. Read-only:
  `mode` (top-bar switch), `mqtt`, `home_assistant`, `web`, `tv.wake`, `tv.off`, with each
  secret as its environment variable name and set / not set (read at request time).
- Validate runs the save path's checks on the edited text: deserialisation
  (`deny_unknown_fields` included), `Config::validate` and `runtime::validate_reload`
  (now `pub`), so a
  restart-gated edit is rejected before review. Input errors (not a number, outside the
  table's bounds, unknown choice) are reported per field without touching the document.
  Each `[[device]]` is also validated alone so a problem lands on its row; a validation
  message is pinned to the field it names, else shown for the section. The raw editor
  reports parser errors at their span and validation errors at the key's line.
- Pending edits live only in the form (no server session). A plain change returns only
  out-of-band state (changed / error marks, row headers, footer, nav), so inputs keep what
  is being typed; structural buttons (add/remove role, probe or slot, fill, reset, back)
  re-render the section. Leaving a section with pending changes asks first, on phones too:
  the phone section picker is the same links in a `<details>` rather than a `<select>`,
  because a select already shows the new section when its confirm is cancelled and no
  script may reset it. Controls carry `aria-invalid` with their error; the form posts the
  names it currently marks (`_invalid`) and a state response re-renders only the controls
  whose mark flips (htmx restores focus by id). Checkbox groups are `<fieldset>`s named by
  a visually hidden `<legend>`; the review diff is an `<ol>` with textual `+`/`-`.
- Only the fields the user changed are applied, always, not only after a 409. The section
  form carries `_orig`: one hidden JSON object of form name → the value that field showed
  when the form first rendered it (string, list or null), plus the identity of each file
  row of the section's row array at page load, in file order (`rows:device`: role ids;
  `rows:presets.slot`: each slot's `[from, to, script]`). Validate, preview and save write
  a field only when its posted value differs from its original, compared as the "was"
  note compares (numbers by value, lists as sets, text trimmed with every line break —
  CRLF or a lone CR — as LF, as an HTML textarea normalises it, a blank optional value as
  absent): an edit back to the original is no edit, and a key changed in the file outside
  the form is never written back, even in the same section; an untouched text keeps the
  file's own bytes. A re-render (a structural button, Back to edit) shows an untouched
  field at the file's current value and moves its original there; an edit keeps its own.
  New rows carry no originals and are written whole. Structural changes stay explicit: a
  role, slot or probe leaves the file only through its remove button; rows appended
  outside the form (past the page-load rows) are kept. Form rows address file rows by
  position, so the form fails closed when that position no longer holds the row it opened
  on: if the file's first page-load-count rows are not exactly the recorded identities (a
  row removed, reordered, a role renamed or a slot changed outside the form), validate,
  preview and save (its 409 fresh review) refuse the form unapplied — nothing is proposed
  or written — and answer 409 with the section re-rendered from the file, the user's edits
  to it dropped and a note to make them again; a plain change gets `HX-Retarget: #section`
  and `HX-Reswap: outerHTML` so the whole section swaps. Re-rendering from the file rather
  than keeping the edits is deliberate: kept edits would sit on rows that now mean other
  roles. A form posted without `_orig` applies every posted field and positions unchecked.
- Review → save is one step for form and raw: one sentence per changed key (table
  sentences, else `key: old → new`; a whole added or removed role is one sentence), the
  static restart and resync columns, and a unified LCS line diff with `@@ [table] @@`
  hunks. The editor reads the file through the runtime (`Request::ReadConfig`), never a
  cache, and the review carries that text's 64-bit hash (`runtime::config_hash`). Save
  refuses with 409 and a fresh review when the file read now hashes differently: the
  review applies the user's changes to the current file, so its diff lists only those
  changes and the external edit survives the save.
  `Request::SaveConfig(text, expected_hash, reply)` repeats the check in the runtime's
  serialized save handler: it re-reads the file immediately before the atomic write and
  answers `ConfigError::Changed` (also 409) when it moved, so a hand edit of the mounted
  file or a mode switch since the review is refused, not overwritten. It is not a lock: an
  external writer landing between that read and the rename is not covered (documented in
  operations). `POST /config` is gone.
- Deviations from the boards: one section is edited and reviewed at a time (05a shows two
  sections and a combined count; edits live in the form, which holds one section).
  "On the bus now" compares the filled keys with `view::devices::rule_matches` over the
  snapshot's `BusEntryView`s (`identity::matches` needs engine `BusEntry`s); when nothing
  matches it lists unbound entries to fill from. The raw textarea has no line-number
  gutter; errors name their line. The review's diff is open on phones too (no JS to fold
  it by viewport).

## 2026-11-08 — Phase 2 slice 3: Devices

- `/devices` is the registry: roles (config order), outputs (TV, receiver, speakers),
  the bridge and unbound bus entries (no role bound; the bridge, the receiver's
  audio-system entry — the first unbound `device_type = 5` — and entries a restored role
  waits on are listed elsewhere). `/p/devices` returns the `#registry` block, polled every
  5 s. `/devices/{role}` (404 for an unknown role), `/devices/out/{tv,receiver,speakers,
  bridge}` and `/devices/la/{n}` are the detail pages; an LA page for a bound role, the
  bridge, the audio system or a restored role's entry redirects (303) to that page, an
  unknown LA is 404. Device profiles come from the configuration text, parsed per request.
  The receiver page's CEC entry panel carries Identify and a confirmed Forget for the
  audio-system LA (same action routes and warming rule as an LA page), so the redirect
  loses no operator control.
- Statistics ("How it behaves", output timings) are computed from the ring on request.
  Excluded everywhere: `would` rows and migrated history; latencies of 0 ms (already
  satisfied when decided). Percentiles are nearest-rank, `sorted[ceil(p·n/100) − 1]`; p95
  is hidden under n = 5; n is always shown. Wakes = rows of the role with `to = on` and a
  settled verdict (not `none`, not `pending`), achieved / total. Wake → picture =
  `tv.power = on` met on those wakes, restricted to `tv_direct` samples when any exist
  (the via is shown). Receiver ready = `receiver.power = on` met on those wakes. Source
  switch = `receiver.source` met on `takeover_settled` rows whose parent is one of those
  wakes. Standby confirmed = settled `role:<r>` power expectations on any row: met or
  `cec_only` of total, missed and unknown counted separately, with every allowed via.
  Polled roles add "left the bus" = `presence_timeout` rows. Output timings use the same
  rules over all rows: TV off (`tv.power = off`, `tv_direct` preferred), receiver on, off
  (p50/p95) and source, speakers `playback = stopped`.
- Agreement between truth sources compares normalised power: CEC on/standby; Home
  Assistant `on|playing|paused|idle|buffering` → on, `off|standby` → off; probe
  awake/standby. A CEC transition, unknown or unavailable Home Assistant, an unreachable
  probe or Denon are not evidence; for the TV, as on the cockpit, webOS unreachable and
  Home Assistant unavailable/unknown read as off. Fewer than two readings = "one source".
- Identity events: the role's `identity_lost` rows, plus `conflict`, `conflict_resolved`,
  `command_identify` and `command_forget` rows (no role) whose trigger frame sender or
  command target is one of the role's addresses: current, last (below), restored, any in
  a current `Snapshot.conflicts` entry naming the role, or one a loss row names. Synthetic
  `verified` (bound entry's `confirmed_at`) and `restored` (service start) lines. An entry
  confirmed before the service start is shown as restored and re-confirmed by vendor and
  OSD (only a live 84 refreshes `confirmed_at`).
- Additive snapshot fields: `BusEntryView.confirmed_at` (from `BusEntry.confirmed_at`) and
  `BusEntryView.next_identify_at` (from the new read-only `BusModel::next_identify_at`, the
  per-entry arithmetic of `next_identify_deadline`; `None` while the bridge is offline,
  when no query is sent). `RoleView.last_logical_address`, view only, filled by the
  snapshot builder: the binding, else the restored entry's LA, else the previous
  snapshot's value (a conflict or a forget on an Off role unbinds without any row naming
  the role), else after a restart the trigger LA of the role's newest `identity_lost` row.
  The engine keeps no such memory.
- Role page sections are in the DOM once: Identity, Identity events, Quirks and Decisions
  are open `<details>` that read as plain panels on desktop and fold on phones. Phones
  therefore open them expanded (board 04d shows them folded; no script can fold them by
  width under the CSP); the "Edit in Config" and "All in Recorder" links sit at the foot
  of their panels, outside the summary, at both widths. The bridge page polls its own
  `#bridge-page-info` wrapper (`/devices/out/bridge`), the drawer its `#bridge-info`;
  both include the same status content with id prefixes, so no id is shared.
- Deviations from the boards: logical addresses use the CEC names (LA 9 is "recording
  device 3", LA 11 "playback device 3"); an unbound entry an identity conflict involves
  reads `conflict`; the LA page notes say why no role matches (unidentified, conflict or
  unmatched).

## 2026-11-08 — Phase 2 slice 2: the Recorder

- `/recorder` (decision list), `/recorder/{id}` (one decision), `/recorder/live` (live
  frames, SSE at `/recorder/live/events`) and `/recorder/glossary` replace `/decisions` and
  `/bus`, which are removed. The list filters by role (`room` = no role), reason group,
  exact reason, verdict, would, time range and migrated history, all as GET parameters;
  pages hold 100 rows behind a `before=<id>` cursor and the first page polls
  `/p/recorder` every 5 s so verdicts settle in place. The poll replaces one
  `#recorder-results` block (rows, count and pager, all from one snapshot), so the Older
  cursor always follows the displayed last row when decisions arrive or a verdict filter
  drops a settled row. Canonical URLs keep custom-range bounds lossless (RFC3339 with the
  input's fraction). Day separators are UTC days.
- Bus window (`/p/window/{id}?from=&to=`, seconds around the decision, bounded
  −300…0…600): read from the trace day files when `trace.keep_days > 0` and a file exists
  for the window's UTC date(s), else from memory (`recent_frames` + the ring, no
  observations); the footer names the source, and says when the trace has no frames for
  the window (memory is not used to fill a valid trace window). The viewed decision is
  always one row, keyed `(timestamp, id)`, even when the trace lacks its creation line.
  Trace records go through the same `runtime::bridge::Correlation` as replay; a send
  outcome attaches to the send the adapter matched (see the decoder metadata below), never
  to a row found by similar bytes, and an aether host `tx` with no recorded send (manual
  or expired id) is still an `ours` row.
- Window read caps (`runtime::trace::ReadBudget::WINDOW`): at most 5000 in-window entries
  are returned; one line is buffered up to 1 MiB (longer lines are consumed and dropped,
  never held whole); the scan stops after 64 MiB or 2 M lines in total, pre-window, torn
  and unknown lines included. The reader flags `truncated` only when it stops with input
  left (another in-window entry, or unread bytes when the scan budget is spent), and the
  window footer says "Window truncated" from that flag.
- Frames are tagged `trigger` (equal to the trigger frame, closest within ±2 s), `ours`
  (sent or local), `reply` (to the bridge within 1 s after one of ours) or `other`; an
  observation within ±1.5 s of `timestamp + latency_ms` with the met value is noted as
  settling that expectation. Home Assistant observations are read as the engine reads
  them: power `on|playing|paused|idle|buffering` → `on`, `off|standby` → `off`, playback
  `idle|paused|off|stopped|standby` → `stopped`, source from the `source` attribute, and
  sampled at `last_changed` (power, playback) or `last_updated` (source). A trace decision
  row links to its detail page only when the ring holds the same id at the same time (ids
  repeat across state lineages); a verdict line settles the newest earlier row with its id.
- New runtime API: `runtime::trace::read_window(directory, from, to, ReadBudget) ->
  WindowRead { entries, truncated }` (and `trace::day_file`), and
  `AppState::set_trace_directory` / `trace_directory`, set once in `runtime::run` from the
  state file's directory.
- Additive, display-only decoder metadata on `runtime::bridge::Decoded`: `result_for:
  Option<SendKey>` (`Fifo(position)` for Tasmota, positions never reused; `Id(id)` for
  aether) names the recorded send an outcome consumed, including a stock Tasmota reply or
  an aether rejection that yields no `SendResult`; `host_tx: Option<HostTx>` describes an
  aether host transmission that yields no `SendResult`. `Correlation::record_send` returns
  the Tasmota FIFO position. `events` are unchanged, and the live runtime and replay ignore
  both fields.
- The glossary page lists all 38 reason literals by group with the group's rule; the
  detail page links to its anchor. Action strings are decoded in the view from their
  JSON (`Action` has no `Deserialize`); unparsable strings are shown verbatim.
- On the detail page the desktop window loads on `intersect once` and the phone copy, in a
  closed `<details>`, on `toggle once`: one of the two is hidden by the layout, and a
  hidden `load` trigger would read the trace for nothing.
- The top-bar mode switch is a `<fieldset>` with a visually hidden legend instead of
  `div role="group"` (Sonar Web:S6819).

## 2026-11-07 — Phase 2 slice 1: cockpit and chrome

- Phase 2 slice 1: cockpit + chrome; consistency badge rule (warming → not checked
  (shadow) → unknown: no intent yet → drift → unknown: TV unreachable → consistent);
  toasts instead of flash; bridge drawer (raw send moved there); dashboard route replaced.
- Additive, display-only snapshot fields: `RoleView.media_player` (the configured HA
  media player), `RoleView.restored` (unbound, but an unverified entry restored from the
  state file matches the role's keys, using the same predicate `identity::resolve` binds
  with), `Snapshot.bridge_node` and `Snapshot.bridge_boot` (from the last aether status the
  adapter accepted; cleared on bridge offline), and `Snapshot.off_deadline` (reserved;
  engine accessor pending, so always `None` and the cockpit shows no countdown).
## 2026-11-07 (evening) — Probe trust and same-field closure

- E3: entering rest produced only 9–10 seconds of Standby before a roughly
  37-second dark phase, falsely resembling a boot. Require trusted settled rest
  for `probe.rest_s` (default 30, inclusive bounds 5–300 seconds) before the
  first Unreachable query. Real measured boots had at least 54 seconds of rest.
- E4: after an interrupted sleep, discovery kept reporting Standby while CEC
  had reactivated the console. Trust Standby only if its consecutive run began
  strictly after the latest Off→On transition and latest configured On frame,
  including rejected/suppressed late frames. Use this for pre-settlement,
  ongoing/deadline expectations and the start baseline; Awake is unaffected.
- E5: quick re-wakes may go directly Standby→Awake with no dark phase. There
  is then no start evidence; preserve ordinary announcement-order takeover.
- E6: repeats during boot can make the receiver follow the loser again, and
  the first Standby can be ignored. After the initial late announcement, allow
  three repeat sequences per episode; the fourth and later repeats within
  `boot_s` are suppressed. Would remains hypothetical and preserves live takeover.
- E7: a later same-field command closes every older pending field before its
  new expectation exists. Direct runs need three equal samples spanning four
  seconds; TV Unreachable needs two fresh failures. HA needs one fresh usable
  sample. Matching/contradicting/no run closes met/missed/unknown; latency uses
  the run start (second fresh failure for TV Unreachable). CEC/probe closure
  keeps cancellation semantics, including checked CEC success and control slots.
  Opposite direct closure to unknown passes contested shutdown-lag protection
  forward: no pre-settlement or single-read success, but a fresh sustained run
  may meet early. A run-settled opposite leaves the new field ordinary.
- Exact quick-cycle trace outcomes supersede approximate initial expectations:
  the first short Off is unknown; the following wake is missed/unreachable
  when the next Off closes it, because a sustained contradictory run exists.
  That next Off pre-settles met/0. The following wake meets at 4002 ms
  query-send latency and final Off at 6109 ms. No later cycle votes backward.
- E8 root cause was not intent ordering: one failed TV read replaced raw On
  just before the new intent. A single failure is unusable, so the closure
  lookup returned none despite retained usable On. Intent-change drift closure
  now uses retained direct evidence when current raw evidence is unusable.
  Ordinary drift, pre-settlement and dashboard remain raw; entering Shadow also
  uses raw usable evidence or unknown, never retained fallback. Pairing/reload
  clears retained TV/receiver evidence. No Snapshot/schema changes are needed.
- Keep the observation-first rollout (`start_wake = "would"`) pending ring review.

### Accepted review amendments

- A rejected episode survives short/untrusted rest-entry Standby samples:
  update its baseline only until trusted rest reaches `rest_s` or boot expires.
  Keep the winner in `StartState::Rejected`, not a
  separate role field. Every configured On frame advances `awake_at`, so a
  subsequent rejected repeat invalidates an intervening Standby run.
- Preserve probe-run trust and its settled-rest clock on reload when that role's
  probe configuration is unchanged; reset both when it changes. Continue clearing
  raw observations/start episodes. Same-config reload must not launder stale rest.
- Act repeats require a different current active role. If the winner releases,
  end the episode and treat the console's next frame as ordinary wake, without
  stream-path reversal. Would uses the stored winner only while the console
  holds the room live.
- On frames within `takeover_settle_ms` of the previous late row are one
  announcement, with no further row, actions or repeat count. Three repeat
  sequences are allowed after the initial reversal; not three individual frames.
- Remove obsolete same-field pre-settlement guards now made unreachable by
  closure. Keep retained drift fallback exclusive to intent changes, not Shadow.
- Review regression coverage adds rejected rest-entry/+17-second repeat,
  delayed darkness, coalesced `0D`/`82`, winner release, probe-preserving/resetting
  reloads and raw-evidence Shadow closure. The review fixes leave golden bytes
  unchanged; no re-blessing is needed.
- Final would-mode parity fix: coalesced and over-cap frames suppress only the
  hypothetical row; ordinary live takeover still runs. Regression tests put a
  different live source between the first late frame and suppressed next frame.
- Trusted Standby lasting `rest_s` ends a rejected episode before boot expiry.
  With `boot_s = 90`, rest at 68–108 seconds allows darkness at 109 seconds to
  create a new start at 110 seconds; the On frame at 123 seconds is not late.
  Short/untrusted rest still preserves the original episode.
- Reduce private probe-run tracking to `(ConsoleState, Instant)` and remove the
  unused wall timestamp parameter. Raw direct samples retain query-send wall time
  for reporting. No snapshot or persistence changes.

## 2026-11-07 — Console start evidence and startup source guard

- A console's discovery responder went from rest-mode Standby to Unreachable
  9.6 seconds before its first CEC activation; Awake followed CEC by 3.5 seconds.
  Two consecutive Unreachable samples after Standby establish a boot interval
  `(last Standby send, first Unreachable send]`; time all direct samples at query
  send, not reply. Expiry uses the first Unreachable send. Awake cannot confirm.
  Only a configured On frame confirms; Standby retracts and `boot_s` expires
  unconfirmed evidence. No evidence is persisted, restored or created in shadow.
- Share immediate-first-read idle/action-window/idle polling across consoles,
  TV and receiver. Console `idle_poll_s` defaults to 2 seconds (1–60);
  `boot_s` defaults to 30 seconds (5–120). Windows run their full duration.
- `start_wake` defaults to `off`: track ordering without early wake rows.
  Roll out `would` first, inspecting inert `console_start` rows, before `act`.
  Would rows carry full hypothetical actions but never mutate control slots,
  intent, power, or expectations, and never enter the returned action vector.
  Act uses the ordinary wake/release paths; a network blip or full shutdown can
  retract/expire it. Remote Play is an explicit caveat, not another gate.
- A console is newer only if its last Standby send is newer than the active
  role's `on_since`; ambiguous intervals favor the active role. Idle cadence is
  ordering resolution; one-second polling provides one-second resolution.
  Would and act share eligibility (including warming, blocked, already On and
  nonempty immediate plan) and do not require an LA. Ineligible starts are
  silently Superseded; would never records an empty early-wake row.
  Eligibility uses the actual preview action plan, not a duplicate effect predicate.
- Displacement releases an unconfirmed evidence-only On role even before its
  LA is known. Real late announcements release a still-On loser and report the
  actual from state. Preserve the winner's intent, settle and expectations:
  reassert its verified stream path, but skip HA source selection while its
  settle owns it. Would keeps today's real arrival-order takeover.
- Defer retraction/expiry while warming or blocked. Clamp boot deadlines to
  warm-up completion and exclude blocked deadlines to avoid spinning.
  Reload/entering Shadow releases unconfirmed early wakes before clearing them.
  Keep evidence private, omit unused wall timestamps, and model confirmation/
  rejection in the lifecycle enum rather than an independent flag.
  Would starts also get a parented, actionless would expiry row on reload/Shadow.
  Forget replay and receiver-On startup adoption pass configured signals through
  the evidence-confirming path, preventing contradictory later expiry rows.
- A startup Request Active Source was answered by an always-on player while
  the room was off. Learn `82`, `90 00` and `84` replies without ordinary signal
  buffering for five seconds while intent is Unknown. Adopt held signals only
  when receiver power is On (direct first, else HA); discard for Off or unknown
  at the deadline, recording `startup_source_ignored` once per resync.
  Holding masks only the current signal, not role reconciliation of earlier
  explicit wakes. At deadline, unbound holds return to ordinary buffers only
  with receiver On; otherwise discard with role None. A second resync reports
  discards before clearing its old window.
- This is observed start evidence, not phase-4 prediction/anticipatory wake.
  HA actuation is unchanged. Core `Decision.would` omits false on serialization;
  existing replay rows keep their schema bytes.

## 2026-11-06 — Device truth sources for TV and receiver

- HA webostv observes the same SSAP service as direct webOS, but 8–33 seconds
  later with its own reconnect backoff. With native webOS configured, use only
  `tv_direct` for TV expectations and drift; use HA without webOS or while direct
  pairing is unavailable. Keep HA table and control-path wake gating unchanged.
- A direct Active sample 2.2 seconds after TvOff can still describe shutdown
  lag. For direct-sourced fields, a pending same-field expectation blocks
  pre-settlement. A new opposite-value expectation is contested and waits until
  its deadline, even if its first post-command read still says Active.
  HA fields keep their previous timestamp-after-pending-decision rule.
- HA unavailable once promoted a 130-second-old direct On sample and closed
  real drift. Resolution now needs agreement newer than the opening
  disagreement; intent-change, undefined-intent and shadow closures are unchanged.
- Two consecutive TV direct Unreachable samples establish literal evidence,
  matching Off and refuting On at the second sample's timestamp. A single
  transient failure clears grace and cannot meet Off. HA unknown/unavailable
  and console/receiver Unreachable remain unusable.
- Continuous active-mode webOS reads use `verify.tv_idle_poll_s` (15 seconds,
  bounds 5–120), replaced by fast action windows and resumed afterward.
  Pairing block, reload/shutdown cancellation and shadow suppression remain.
- Keep `tv_s = 45` for boot-to-service (observed 17–30 seconds), not HA
  reconnect lag. Default tightening is on hold until roughly 2026-11-12.
- Optional receiver `observe = { kind = "denon", host, port = 8080 }` uses
  device HTTP main-zone power; HA is the fallback only when no direct
  transport exists. Control remains HA. Receiver source stays HA because
  InputFuncSelect code-to-name mapping is vendor-specific; raw codes are traced.
  Receiver Unreachable is unusable (HTTP is reachable in standby), unlike TV.
  Share `DirectPower` and the window/idle loop between both transports.
- Receiver `receiver_poll_ms` / `receiver_window_s` / `receiver_idle_poll_s`
  default to 1000 ms / 30 s / 15 s; bounds mirror TV settings. Keep receiver
  On/Off deadlines 10/20 seconds pending calibration; device latency no longer
  includes HA polling. The 2026-11-05 receiver-off rationale below is superseded
  for direct observation only.
- Simplify grace identity for all sources: same disagreeing value keeps grace
  even with a new timestamp; new value restarts, unusable evidence clears.
  Otherwise continuous direct polls can never reach 30-second grace. HA
  off/unavailable/off/on still cannot accumulate grace across the unavailable.
  Freeze the selected opening sample timestamp with the open detail for resolution.
- Pairing-required emits `TvDirectUnavailable` (`tv/unavailable` trace), clears
  frozen direct cache, and switches pending/new TV expectations and drift to HA.
  Reload explicitly resets this fallback/streak and restarts the observer.
- Shared native host/port validation rejects schemes, paths, whitespace, embedded
  ports, zero ports, and unbracketed IPv6. Receiver Power alone decides state;
  missing/empty InputFuncSelect only omits the raw source code. Receiver samples
  use the ordinary Event ingress/tracing path.
- Second review: direct TV/receiver action windows run their full configured
  duration, never stopping at the target. Require TV window >= TV deadline and
  receiver window >= max(On, Off deadlines), so contested verdicts have fresh
  action-cadence reads regardless of idle cadence. Console probe windows still
  stop at target.
- Preserve the newest usable direct sample beside raw TV/receiver caches, used
  only at expired contested deadlines. A single transient failure cannot erase
  a prior fresh On; ordinary verdicts, drift and dashboard still use raw evidence.
  Reload clears retained caches; pairing loss clears TV's retained cache.
- Restrict unbracketed native observation hosts to `[A-Za-z0-9._-]`, rejecting
  URL authority/query/fragment delimiters too. A real loopback TLS webOS prompt
  test verifies unavailable emission and suppression of later action queries.

## 2026-11-05 — Reporting-only room drift

- Compare configured TV/receiver power and intended receiver source after
  expectation evaluation, using the shared newest usable independent sample
  lookup. No CEC evidence: role power is the wake input, not desired state;
  speakers stop is one-shot and supplies no playback intent.
- A same-field pending expectation owns reporting until settlement. First
  disagreement then starts a monotonic grace clock (default 30 s, 5–600 s),
  independent of historical HA timestamps. Intent changes/reload reset grace.
  Unusable evidence clears grace but retains open drift, preventing expired
  deadlines from causing a Tick busy loop. A new disagreeing timestamp or
  value restarts grace; identical redelivery preserves it. Both rules reject
  the observed HA webOS reconnect flap: after an achieved wake, HA reported
  off, unavailable, then off/on one millisecond apart while the TV stayed on.
  Neither a gap without evidence nor two different off samples establish a
  continuously held disagreement.
- One actionless onset and one resolution share onset wall time in structured
  `drift` detail. Tick onset uses `timer/drift_grace`; sample onset and
  resolution use `observation` with the selected source.
- Review: entering Shadow closes all open drift and clears grace in the mode
  command pass before cancelling expectations. A changed field intent likewise
  closes its previous pair immediately, then starts a fresh comparison. Open
  drift must never refer to obsolete intent or survive disabled monitoring.
- HA media-player power treats playing/paused/idle/buffering as On and standby
  as Off, sharing normalization with expectation verification rather than
  reporting false disagreements for normal playback states.
- Receiver source awaiting takeover settle counts as pending even before
  `select_source` creates its expectation. Onset observation provenance belongs
  only to an input supplying the selected sample; unrelated inputs use the
  grace timer. Onset construction is extracted from field evaluation.
- Keep monotonic and wall onset together in one optional tuple, preventing
  mismatched clocks without runtime assertions.
- Keep the open set in memory and expose `Snapshot.drift`; historical rows
  persist additively in v2, but do not restore current drift. Reporting must
  not become control or resume stale observations after restart.
- PR B exposes the open set as `binary_sensor.choregos_drift` (problem), with
  retained JSON state and `fields`, oldest `since`, and complete `details`
  attributes on `choregos/state/drift`. Include the complete open set in the
  entity-state fingerprint so drift-only changes publish, independently of the
  last settled decision sensor. Empty drift clears attributes and reports OFF.
  The dashboard shows one reporting-only amber pill per open drift next to
  room intent: `subject.field: observed, intended X, for <duration>`.
  `drift`/`drift_resolved` decision rows show
  `subject.field intended → observed via <source>` without a verdict chip;
  resolved rows also show how long the drift lasted.

## 2026-11-05 — Separate receiver off deadline

- The receiver reports `on` a steady 2 s after `turn_on` but `off` anywhere from
  0.2 s (when CEC Standby reached it first) to 10.2 s after `turn_off`, because
  its integration learns power-off by polling. One shared `receiver_s = 10`
  produced an `unknown` leg and a `partial` verdict for a room-off that took
  10.17 s. `[verify] receiver_off_s` (default 20) now bounds the off
  expectation; `receiver_s` keeps bounding the on expectation. Existing files
  parse unchanged (`[verify]` keys default).

## 2026-11-03 — Already-satisfied expectations

- At expectation creation, accept a matching current independent observation
  (`ha`, `tv_direct`, or `ps5`) from `[decision.at - deadline_ms, decision.at)`,
  recording `met` with zero latency. An entirely satisfied decision starts
  `achieved`. Use the newest independent sample before the decision; a newer
  disagreement blocks an older match. Require the sample to postdate every
  still-pending decision with the same subject and field so an unresolved
  command cannot reuse evidence from before that command. Older samples do
  not count; post-creation freshness, deadline evaluation, and CEC's
  due-confirmation requirements remain unchanged.
- The receiver can obey a streamer's broadcast Standby before the room-off
  timer sends its own power-off request. Requiring a later HA state change
  then incorrectly reports unknown; the observed receiver is already Off.
  Preserve the actual action sequence and fix reporting, not device control.
- Add the real receiver-off-first golden and injected freshness-boundary,
  independent-source, immediate-verdict, and CEC non-pre-settlement tests.
- Follow-up candidate, not implemented: the off-timer could omit receiver
  `turn_off` when HA already reports Off, like the wake path omits redundant
  `select_source`. This change does not suppress that action.

## 2026-11-03 — Probe-only standby window and unreachable samples

- Standby deadlines follow enabled evidence: CEC-only keeps
  `takeover_settle_ms × (standby_retries + 1)`; probe-only uses
  `verify.probe_window_s × 1000`; mixed evidence uses the greater window.
  This permits late independent standby confirmation without changing CEC
  retries, power-reply suppression, or verification-slot lifetime.
- Evaluate independent disagreements from current samples only. A direct
  Unreachable or HA unavailable/unknown sample replaces its source's earlier
  disagreement and cannot itself confirm or refute. At the deadline, use the
  newest current usable observation newer than the decision for missed;
  absent such evidence or checked CEC confirmation, settle unknown.
- A new wake decision cancels that role's earlier pending standby expectation:
  preserve checked CEC success as CEC-only, otherwise settle unknown. A later
  Awake probe for the new intent must not refute the old standby decision.
- Settle due independent expectations before applying an incoming event so
  a sample arriving beyond the deadline cannot replace deadline evidence.
  Active CEC verification slots retain their existing post-event timer pass.
  Reload and shadow cancellation preserve checked CEC evidence; interrupted
  rows preserve completed serialized outcomes rather than resuming timers.
- Add the scrubbed console-swap probe golden: Console B → Console A → Kodi →
  Streamer. Console B sleeps after the old six-second CEC deadline; its
  independent probe must confirm inside the configured 45-second window.
  Other golden cases have no probes and require no output changes.

## 2026-11-03 — Expectations and verdicts

- Implement phase 1 steps 1–3: observable decisions carry expectations and a
  reporting-only verdict, without drift rows or corrective actions.
- Default per-field deadlines under `[verify]`: TV 45 s, receiver power 10 s,
  speakers stopped 5 s, receiver source 10 s. Role standby uses
  `takeover_settle_ms × (standby_retries + 1)`.
- Require observations newer than the decision: HA `changed` for power,
  `updated` for source attributes, direct timestamps, or fresh CEC power
  evidence. Old traces lacking `updated` use line time for attribute freshness.
  The TV deadline is an observation window allowing HA reconnect lag; actual
  confirmation latency is recorded separately. Any allowed source can meet an
  expectation; an earlier disagreement from another source cannot veto a match.
  CEC-only confirmation waits for the independent probe until the deadline.
  Keep only the last disagreement for missed outcomes.
- Fold standby retries into the displaced role's parent expectation; attempts
  count resends, not the initial sequence. Preserve bounded retry sequences,
  ACK/NACK accounting, address retargeting and cancellation. Opt-out roles
  use only a configured probe (`[ps5]`) or no standby expectation; CEC On
  cannot refute a console that answers before sleeping silently.
- Preserve main's CEC verification lifecycle independently of reporting: remove
  slots only at their due cancellation/confirmation/exhaustion check and schedule
  retries from the actual check time. This preserves power-reply suppression
  through transient Standby/On reports and delayed warm-up timers.
- Never infer CEC standby for HA media-stop or none policies; an independent
  configured probe is the only standby observable for those policies.
- Bridge offline cancels only all-CEC fields; reload cancellation is field-specific
  to relevant configuration changes. Bus entries own monotonic power provenance.
- Keep replay's five-second control-timer drain, followed only by reporting
  settlement. No later presence/off timers or device actions may be synthesized.
- Use tagged `outcome.kind` variants. Extend `unknown` with optional `observed`
  (omitted normally) to retain `"absent"` when every sent frame was NACKed and
  none ACKed. This is not a claim that absence proves standby.
- Keep state version 2; new fields are additive and missing fields default to
  no expectations / verdict none. Final settlements mutate the existing ring
  row, publish an SSE verdict event, trace `choregos/verdict`, and submit the
  same single-slot state writer. HA exposes `sensor.choregos_last_decision`
  with verdict state and id, role, reason and slowest independent latency.
  Interrupted pending rows loaded after restart settle unknown through that
  same path; old monotonic deadlines and dispatched work cannot be resumed.
- Re-bless the golden corpus for removed verification follow-up rows and added
  verdict lines; retain original ids modulo the removed rows, parent links and
  all unchanged decision fields. Per-case outcomes are documented in the corpus.
- Known follow-up (pre-existing q8): all-off at warm-up completion can replay a buffered wake after the stops and power the room back on; unchanged in this phase.

## 2026-11-02 — Direct observations and room intent

- Remove the optimistic TV/receiver HA-cache writes at room-off: a command is
  not evidence that either device changed. The HA cache now contains only
  observations, including their original timestamps.
- Keep a separate, non-persisted `RoomIntent { power, active_role,
  receiver_source, off_at }`, initially Unknown with empty fields. Wake
  decisions set On, the role, and the configured receiver source if selection
  is enabled; reload clears that source. Intent is the engine's single
  active-role source. Role release clears the active role; all-off only
  releases roles and schedules the room-off timer. Expiry sets Off, clears
  role/source, and records its wall time. Preserve this `off_at` across wakes:
  cached On suppresses wake only if HA's `changed` is strictly later than
  the last room-off. This preserves repeated quick re-wake through HA lag,
  not just the first wake after an off request.
  Unknown startup intent uses only the HA cache, preserving wake suppression
  after restart with the room already On. The first golden run exposed three
  redundant first-wake power requests when startup was Off; Unknown fixes that
  regression without re-blessing any golden output.
- Add direct TV observation through the existing webOS TLS/SSAP registration
  and `getPowerState` exchange, independently of HA reconnect timing. Only
  active-mode TV wake/off dispatches start polling; HA-only off configuration
  has no direct source. `Active`, `Screen Off`, and `Screen Saver` mean On;
  `Active Standby`, `Suspend`, and `Power Off` mean Off. Failed reads are Unreachable,
  not an assertion of power-off. Pairing-required stops polling and warns once
  until the adapters restart.
- `[verify].tv_poll_ms` defaults to 2000 (500–10000 ms); `tv_window_s` defaults
  to 45 (5–300 seconds). Poll immediately after dispatch, then at the interval,
  stopping at target or deadline; a newer TV action replaces the older window.
  These observations are reporting-only: no expectations, verdicts or remediation.
- Trace direct observations as `tv/state` with `{state, at}`; replay consumes
  them and extraction retains them. Snapshot keeps the last direct TV state
  and timestamp separately from HA; neither intent nor direct state is persisted.
- Add optional per-role `probe = { kind = "ps5", host, port = 9302 }` for
  read-only Sony console discovery. One unicast UDP request, one-second reply
  deadline: status 200 is Awake, 620 is Standby, failed reads are Unreachable.
  Wake transitions and dispatched CEC stops open target-scoped windows with
  `verify.probe_poll_ms` (default 2000, 500–10000) and `probe_window_s`
  (default 45, 5–300 seconds). Opposite targets replace windows; duplicate
  retries do not extend a pending window. Reload/shutdown abort in-flight work.
  Any queued Standby to the configured role's logical address starts its stop
  window, even if that role was already Off and another role supplies the
  all-off transition decision. Reload also clears all cached direct TV and
  role observations so old endpoints cannot lend truth to a new configuration.
  Trace as `probe/state` `{role, state, at}`; replay/extract handle both sources.
  Role snapshots keep direct observations separate from logical role power.
  Remote Play and a reachable UDP path are operator prerequisites; there is
  no pairing, remediation, or new network attachment in Choregos.

## 2026-11-02 — Decision identity

- Assign each engine decision a monotonic `u64` id, an input/timer `trigger`,
  and optional `parent` so a follow-up can be linked to the intent that scheduled
  it without changing actions, timing, or append-only ring behavior.
- Serialize triggers as `{"kind":"frame","from":…,"to":…,"data":"HEX"}`,
  `{"kind":"command","command":<Command>}`, `{"kind":"timer","name":…}`,
  or `{"kind":"service","name":…}`. Commands retain their tagged kind and
  arguments; timer names are `room_off`, `takeover_settle`, `presence`,
  `standby_verification`, and `warmup`; service names are `started`
  and `migrated`. Inputs that cannot directly record rows have
  no current trigger; bridge events need no serialized trigger variant.
- Keep timer/service provenance allocation-free and encode frame hex only when
  recording a row. Warmup provenance applies only to warmup decisions, never
  another command handled at the same deadline.
- `takeover_settled` links to its wake decision; standby retry and terminal
  verification rows link to the decision that registered the stop sequence,
  preserving that link when the target moves logical address. Due timers use
  their own trigger even when processed alongside another input.
  Parent only verification slots for roles actually stopped by that transition;
  an already-Off role uses its command fallback row if one exists, otherwise
  leaves parent null rather than claiming another role's row.
- Persist state version 2 with `next_decision_id`. Migrate version 1 atomically
  in place: ring-order ids start at 1, parent is null, trigger is
  `{"kind":"service","name":"migrated"}`, and the next counter is ring length
  plus one. Restore the counter, not the newest retained row, across restarts.
  Repair stale counters to exceed every retained ring id; exhausted counters are
  corrupt state, not a startup panic. Uniqueness is per persisted state lineage;
  crash/write failure or restoring backups can reuse trace ids, so correlate
  historical trace rows by `(ts, id)` and restore state backups on v2 rollback.
- Record creation rows on trace topic `choregos/decision`, including `would`
  and structured actions rather than the ring's display strings. Extraction
  retains them; replay ignores them (including for identity-seed selection) and
  recomputes decisions from engine inputs.
- Replay numbers decisions from 1 per run and emits fields in order:
  `at`, `id`, `parent`, `trigger`, `role`, `from`, `to`, `reason`, `actions`.
  The golden corpus changes only by the added identity fields; expectations,
  observations, verdicts, and behavior changes remain out of scope.

## 2026-11-02 — Golden replay corpus and trace extraction

- Curate five real-use replay windows covering native bridge wake/takeover,
  restart continuity and standby retries, plus the Tasmota adapter. Keep public
  names, entities and bridge topics in fixtures; private captures stay ignored.
- Share inclusive window selection, identity seeding, chronological-order checks
  and torn-line warnings between `extract` and `replay`. Extraction preserves
  later headers without treating them as runtime restarts.
- Share the last pre-window bridge availability record between replay and
  extraction, preserving its original timestamp and the same warm-up clock.
  Preserve initial in-window headers without duplicating the selected seed.
- Apply ordered substring renames recursively, including serialized JSON and
  ASCII Set OSD Name operands. Validate only changed names against the 15-byte
  frame limit; preserve undecodable/non-ASCII originals. Validate node segments.
- Atomically replace extraction output after success; validate bounds and resolve
  aliases before touching it so failures cannot truncate captures or old output.
- Add `at` only to replay's output wrapper, not the engine's `Decision` contract.
  Drain final timers at their actual deadlines through last input plus five
  seconds, independent of `--until`, so full-window and fixture replays agree.
- Send offline CLI diagnostics to stderr so sequence-gap warnings cannot pollute
  deterministic replay JSONL; retain structured stdout logs for the live service.
- Compare offline replay output byte-for-byte in one sorted, nonempty corpus
  test. `UPDATE_GOLDEN=1` is an explicit review action, committed with and explained
  alongside intentional behaviour changes rather than hiding unexplained drift.
- Follow-up, not implemented: identity headers contain no role power,
  verification state or pending timers, so they cannot faithfully restore a
  window that starts mid-session. A richer trace snapshot carrying those fields
  is a phase-1 candidate; the corpus documents today's known differences.

## 2026-11-01 — Aether bridge adapter

- Require `[bridge].kind` explicitly: Tasmota and native aether use different
  topics, raw-send limits and outcome semantics. Adapter changes restart the
  transport and reset the bus model rather than guessing a legacy default.
- Keep native status/bus trace payloads raw and share one correlation decoder
  between live ingress and replay: boot changes, sequence deduplication and
  ID-based outcomes must reproduce the same engine inputs.
- Firmware-local transmissions are visible in the UI as `local`, never engine
  inputs or host sends. Promiscuous receive from the bridge's own address is
  likewise UI-only and cannot create an identity or activate a role.
- Snapshot exposes the current adapter's raw-send limit for the UI; the engine
  and command validator use the same `BridgeConfig::max_raw_bytes()` property.
- `cmd/state` and receive-error HA sensors are deferred; line state is
  diagnostic snapshot data, not a decision-engine input. Tasmota `Ack:false`
  remains `Nack` for compatibility; native arbitration/line/busy failures do
  not establish target absence.
- A directed native `rx` with `ack:false` is presence evidence only: the shared
  adapter emits a header-only engine frame but retains the full bytes for the UI
  ring and raw trace. A NACK-cut identity reply must not replace a running device.
- Availability starts unknown and accepts bus events without inventing an online
  event, so daily files and `--since` windows replay even without a status prefix.
  Explicit offline or unsupported protocol status blocks bus events until a
  supported online status arrives.
- A board LWT clears pending sends but preserves boot and sequence history.
  Only a different boot resets sequence deduplication; a newer same-boot status
  timestamp triggers resync, while equal/older retained redelivery does not.
- Allocate native `s-<n>` send IDs from one process-wide atomic counter, not an
  adapter-local counter, so late results cannot collide across config reloads.
- Remove the unreachable generic frame-input branch; bridge ingress is the sole
  frame/UI path. One native telemetry-period constant supplies the 180-second
  initial stale threshold; configured zero disables the age check, not weak RSSI.
- Expose a lock-only raw-send-limit accessor rather than cloning the snapshot for
  command validation. Local transmission ACK metadata is true only for `ack`,
  false only for `nack`, and absent for every other outcome.
- Name the real identify capture `fixtures/aether-identify.jsonl`; keep its
  authentic OSD/vendor bytes rather than rewriting bus evidence. Native loop
  verification takes back the stopped engine and injects its settle tick instead
  of waiting a real second. Cross-kind save/reload is exercised with an in-process
  broker and retained native status; the existing ephemeral-port harness stays.

## 2026-10-31 — Per-role standby verification

- Add per-device `verify_standby`, default `true`, so a role can send its
  complete CEC stop/query sequence once without scheduling verification,
  retries, or `standby_*` decisions. Console-style sources structurally answer
  Standby with `90 00`, re-announce, then silently sleep more than six seconds
  later or vanish from the bus. Retrying cannot verify that transition in the
  current window; `taken_over` already marks the role Off. Nothing confirms
  the console actually slept once verification is disabled.
- Opting out leaves `90 xx` replies on the ordinary configured `on`/`off`
  event path, not a special suppression path. For CEC takeover policies,
  reject `verify_standby = false` when `on` includes `power_on`: the stop
  sequence's own query reply could wake the role and undo the takeover.
- The streamer's verification window and timing defaults were deliberately left
  alone pending more trace data: it is the source that needs standby retries.
  Public example configuration uses generic console names.

## 2026-10-30 — Bus trace and bridge telemetry

- Bridge Wi-Fi signal, link count and uptime are runtime `Input` data, not
  engine `Event`s: diagnostics must not influence the pure decision engine.
  The snapshot keeps their arrival time and the last received-frame time.
- Runtime and replay use one JSONL encoder/decoder. Raw `HdmiSend` RESULT
  payloads, including uncorrelated and stock firmware replies, are captured
  before passing correlated ACK/NACK events to the engine. Replay correlates
  them against recorded `cmnd` lines with the live adapter's bounded FIFO and
  five-second expiry, never against its own generated sends: filtered windows,
  shadow suppression and configuration changes can produce different actions.
- Identity headers bypass `--since`/`--until`; otherwise a time window would
  discard the seed needed to match a device whose vendor/OSD predates it.
  The last header before the first in-window event wins, including restart
  headers carrying a newer restored seed. If none precedes that event, a
  pre-scan supplies the first header as a fallback. Headers after replay starts
  never reset the reconstructed model.
- Append-only UTC daily files flush once per line. At roughly 300 lines/hour
  this is inexpensive, limits buffered data loss and keeps incident slices
  immediately available. Capture runs in a separate mpsc task with a bounded
  1024-record queue; overload drops records with one warning per drop streak.
  Retention and identities use a separate latest-value watch channel, so a full
  record queue cannot lose a reload or leave future headers permanently stale.
  I/O errors warn once per failure streak and never stop room control. Retention
  uses the UTC date encoded in strictly named trace files, leaving other files
  untouched; pruning failures do not prevent opening the current day's file.
  A newline before headers appended to non-empty files separates torn writes.
- Commands retain their existing tuple variants and use adjacent serde tags:
  `kind` is snake_case and `data` holds a tuple value or the `SendRaw` object.
  This round-trips all commands without changing engine/UI callers.
- Traces include only the HA attributes listed in the engine's `HA_ATTRS`
  contract (currently `source`), excluding media proxy tokens and viewing
  history. The full attributes remain available to the live snapshot.
- Recorded commands were already accepted by the live runtime. Replay passes
  them directly to the engine without repeating runtime validation against a
  filtered window's incomplete or warming model. File replay warns with
  `file:line` and skips invalid JSON fragments left by torn writes; valid JSON
  with invalid envelopes or known-topic payloads still fails with that context.
  Text fixture replay remains strict. Inputs must be chronological; backwards
  timestamps fail with an explicit ordering error and file/line location.
- Broker disconnect/reconnect is `choregos/connection` with `{"mqtt":false}`
  or `{"mqtt":true}`, distinct from bridge-origin LWT. Replay clears its send
  FIFO on either marker and maps disconnect to `BridgeOffline`, just like the
  runtime; reconnect alone never asserts that the bridge is online.

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
