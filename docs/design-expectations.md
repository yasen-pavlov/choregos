# Expectations, verdicts and drift (phase 1, steps 1–3 and step 4 detection implemented)

A decision today ends when the engine has emitted its actions. CEC sends come
back as ack/nack and feed only standby verification; HA calls, webOS off and
WoL return nothing to the engine. The ring has no decision identity, so nothing
can be attached to a decision after the fact. This phase closes the loop:
**decision = intent + expectations + verdict**, persisted, traced, replayable.

Reporting only. Nothing in this phase sends an action because a verdict was
`missed`.

## What `tv_direct` measures

Observed 2026-11-03 on three cold wakes: after the WoL frame the TV's webOS
service answers `unreachable` for 15–19 s (TCP connect fails while the set
boots), then `on`; Home Assistant's webostv integration follows 8–11 s later.
The set itself takes roughly 20 s from WoL to picture (operator estimate, not
measured), so `tv_direct` is the closest LAN-observable proxy for "picture on"
and HA is the laggard. The dashboard labels it "tv (webOS)" because it is the service, not the
panel, that answers.
Direct webOS is the TV truth source whenever `tv.off.kind = "webos"`:
HA observes the same service through its own delayed reconnect, not an
independent signal. HA supplies TV expectation/drift evidence only without webOS
or while direct pairing is unavailable. HA TV remains visible and its control-path
wake gating is unchanged. Two consecutive direct Unreachable samples establish
literal `unreachable` evidence: agreement with Off, disagreement with On. A single
failure is unusable and clears drift grace; On/Off resets the streak. Receiver/
probe Unreachable remains unusable.

## Measured baseline (ring + traces, 2026-10-31 → 11-02)

| expectation | n | p50 | p90 | max | source |
| --- | ---: | ---: | ---: | ---: | --- |
| receiver `on` after wake | 23 | 2.0 s | 2.1 s | 8.3 s | HA state |
| TV `on` after wake | 21 | 22.1 s | 32.6 s | 49.6 s | HA state (bimodal: 3.4 s when HA was still connected, 20–33 s after a WoL wake while HA's webOS integration reconnects) |
| displaced role standby | — | — | — | — | CEC `90 01`; Apple TV 20/26 takeovers needed a retry, consoles structural |

The TV number is HA's reconnect latency, not the TV's. Phase 1 therefore adds a
direct webOS observation (below) and treats the first week of verdicts as
calibration before any default deadline is tightened.

## Model

```text
Decision
  id            u64       monotonic across restarts (persisted counter)
  at            RFC3339
  role / from / to / reason / actions / would   unchanged
  trigger       {"kind":"frame","from":u8,"to":u8,"data":"HEX"}
              | {"kind":"command","command":<Command with kind and optional data>}
              | {"kind":"timer","name":"room_off"|"takeover_settle"|"presence"|"standby_verification"|"warmup"|"drift_grace"}
              | {"kind":"service","name":"started"|"migrated"}
              | {"kind":"observation","source":"ha"|"tv_direct"}
  parent        Option<id>   for takeover_settled follow-ups
  expectations  [Expectation]   empty for rows with no observable effect
  verdict       pending | achieved | partial | missed | unknown | none
  drift         optional DriftDetail {subject, field, intended, observed, via, since: RFC3339}
                omitted when absent; since joins onset/resolution rows

Expectation
  subject   tv | receiver | speakers | role:<name>
  field     power | source | playback
  expect    "on" | "off" | "standby" | <source name> | "stopped"
  via       [tv_direct, denon, ps5, ha, cec]   allowed confirming sources; any fresh match may meet
  deadline_ms  ms from `at`
  attempts  u8   CEC standby resends (replaces separate retry rows)
  outcome   pending | met { via, observed, latency_ms }
          | missed { observed }            deadline passed, last observation disagrees
          | unknown { observed? }           no observation; optional "absent" for all-NACK targets
          | cec_only { observed }          met, but only CEC could see it
```

Verdict rules, evaluated when the last expectation settles or the longest
deadline passes:

- `achieved`: every expectation `met` or `cec_only`
- `partial`: at least one `met`, at least one `missed`/`unknown`
- `missed`: none met
- `unknown`: all `unknown` (bridge/HA down)
- `none`: the decision carries no expectations (identity rows, `taken_over`,
  `conflict`, shadow `would:` rows)

Latency on the verdict = the slowest `met` expectation.

## Which decisions expect what

| decision | expectations (subject.field = expect, via, default deadline) |
| --- | --- |
| off→on transition (wake) | `tv.power = on` only if `WakeTv` was sent (tv_direct with webOS, otherwise ha; 45 s) · `receiver.power = on` only if `turn_on` was sent (denon with observe, otherwise ha; 10 s) · `speakers.playback = stopped` if `media_stop` was sent (ha; 5 s) · `role:<displaced>.power = standby` only for stopped CEC policies with verification enabled or any stopped role with a probe (CEC retry window, probe poll window, or their maximum; source policy below) |
| `takeover_settled` | `receiver.source = <role source>` if `select_source` was sent (ha; 10 s). Preset scripts have no observable → no expectation. |
| `off_timer_expired` | `tv.power = off` only if `TvOff` was sent (tv_direct with webOS, otherwise ha; 45 s) · `receiver.power = off` only if receiver `turn_off` was sent (denon with observe, otherwise ha; 20 s) |
| `all_off`, `manual_standby` | Same standby eligibility: CEC policies with verification enabled, or probe-only for a stopped role with a probe; no CEC expectation for HA media-stop/none policies |
| everything else | none |

Receiver power uses `denon` when `[receiver].observe` is configured, otherwise
`ha`. Receiver source remains HA in either configuration. Deadlines remain
10 seconds On / 20 seconds Off; direct observations measure device latency,
not HA polling delay.

A role's own `on` is what triggered the decision, so it is not an expectation.
Expectations describe actions actually emitted, not unchanged room state:
a takeover with TV/receiver already On has no power expectation for either.
`cec_only` counts as met but requires actual fresh Standby/Transition evidence;
silence from a disappearing console remains `unknown`, not achievement.
Standby retry mechanics are folded into the displaced role's expectation
(`attempts`, terminal outcome), with no separate verification ring rows.
`verify_standby = false` opts out of CEC standby verification and retries.
With a configured probe, create `[ps5]` only; without one, create no standby
expectation for that role. Such consoles may answer `36` with `90 00` and
sleep silently later, so CEC neither confirms nor refutes their standby.
Verifying roles use `[cec]`, or `[ps5, cec]` with a probe. The CEC leg is marked
only at the verification's due confirmed check, with fresh Standby/Transition
still present; a raw transient report is not success. With a probe, checked
CEC evidence waits until the deadline for independent confirmation and is not
erased by a later cancellation. Fresh On at exhaustion yields missed; no usable
observation yields unknown.
`ha_media_stop` and `none` do not imply CEC standby; without a probe they
contribute no standby expectation, including all-off's already-Off media players.
The original CEC verification slot remains independent of the expectation:
it is removed only at its due cancellation/confirmation/exhaustion check,
and retries schedule `now + settle`. A verdict must never change suppression
of `90 xx`, the retry sequence, or the lifetime of that slot.
Standby deadlines follow the enabled legs: `[cec]` uses
`takeover_settle_ms × (standby_retries + 1)`, `[ps5]` uses
`verify.probe_window_s × 1000`, and `[ps5, cec]` uses the maximum.
The probe therefore retains its full configured observation window after the
CEC retry sequence exhausts; this does not extend or reschedule CEC retries.



## Evaluation

At creation, an expectation settles `met` with `latency_ms: 0` if the newest
sample before the decision across its listed independent sources (`ha`,
`tv_direct`, `denon`, or `ps5`, never `cec`) matches and is timestamped in
`[decision.at - deadline_ms, decision.at)`. A newer disagreeing sample blocks
an older match. For direct-sourced fields only, skip pre-settlement while any
same-field expectation is pending. An opposite-value pending expectation makes
the new direct expectation contested: even fresh agreeing reads cannot meet it
early; at its deadline the newest usable fresh sample decides met/missed/unknown.
Retain the newest usable direct sample separately from the raw cache, solely for
expired contested evaluation. A +30 s On sample survives a single unusable
Unreachable at +44 s and meets the +45 s deadline. All ordinary reporting still
uses current raw samples; reload clears both caches, pairing loss clears TV's.
HA fields keep the previous timestamp rule: the sample must postdate every
pending decision carrying the same subject/field.
The deadline bounds sample age, not room-cycle duration. HA uses `changed`
for power/playback and `updated` for source attributes; direct sources use
their timestamps.
If every expectation is already satisfied, the decision is `achieved` at
creation. CEC still requires fresh evidence at its due verification check.
Otherwise evaluation and deadline freshness remain unchanged: observations
arriving after creation must be newer than the decision, not merely recent.

Deadlines are per-field config under `[verify]` with the defaults above;
calibration may change the defaults in a later release, not the schema.
The TV default remains 45 seconds for boot-to-service (observed 17–30 s),
not HA reconnect allowance; tightening is on hold until roughly 2026-11-12.
HA power/playback freshness uses `changed > decision.at`; receiver source
freshness uses `updated > decision.at` from HA `last_updated`. Trace
`ha/state.updated` preserves that timestamp; old traces fall back to line `ts`.


## Observations

Already flowing: `Event::HaState` (tv, receiver + `source`, speakers, device
media players), `Event::Frame` power replies `90 xx`, `Event::SendResult`.

New:

- **`Event::TvPower { state, at }`** from a direct webOS `getPowerState`
  (the SSAP session already used by `tv.off`). Active mode with a configured
  webOS key polls continuously every `verify.tv_idle_poll_s` (default 15,
  bounds 5–120). A dispatched wake/off replaces idle/older windows with
  immediate polling then `tv_poll_ms` for the full `tv_window_s`, then idle
  resumes. Neither direct windows nor idle stop at a target. Require
  `tv_window_s >= tv_s`. Pairing-required blocks until adapter
  restart and emits `TvDirectUnavailable`, traced as `tv/unavailable`, clearing
  frozen cache and selecting HA for pending/new TV expectations and drift.
  Reload explicitly resets fallback/streak, aborts the task and restarts idle.
  Every sample is traced as `tv/state`, preserving replay freshness.
- **`Event::ReceiverPower { state: DirectPower, at, source_code }`** uses the same shared
  On/Off/Unreachable enum as TV. Optional
  `[receiver].observe = {kind="denon", host, port=8080}` reads the main-zone
  HTTP XML status with a three-second timeout. Power ON/OFF is evidence;
  Unreachable is unusable because HTTP remains reachable in standby.
  Optional InputFuncSelect codes are parsed and traced as `source_code`, not mapped
  to HA names. Missing/empty codes do not invalidate Power. Source observations
  and control remain HA. A single shared
  scheduler supports `receiver_poll_ms = 1000` (500–10000),
  `receiver_window_s = 30` (5–300), `receiver_idle_poll_s = 15` (5–120).
  Dispatched receiver turn_on/off replaces idle/window work; it polls for the full
  `receiver_window_s`, then resumes idle. Require
  `receiver_window_s >= max(receiver_s, receiver_off_s)`. Reload/shutdown aborts
  it; reload restarts idle; shadow does
  not poll. Each sample reaches the engine and `receiver/state` trace;
  non-persisted `receiver_direct` cache is cleared on reload.
- **Optimistic HA cache writes removed (step 2).** Off requests no longer write
  TV/receiver power into HA's observation cache. A separate room intent starts
  Unknown and follows wake/off decisions; a wake sends TV and receiver power-on
  whenever intent is Off or the corresponding HA cache is not On.
  Cached On is current only if its HA change timestamp is newer than the last
  room-off request; the timestamp remains across repeated wakes during HA lag.
  All-off changes power intent only when its room-off timer expires.

## Room intent and drift

`RoomIntent { power: unknown|on|off, active_role, receiver_source }` is derived
from the transitions creating decisions.

Monitor TV and receiver power for configured entities when intended power is
On/Off, and receiver source when power is On with a defined source. Startup
Unknown checks nothing. Do not monitor speakers playback (stop is one-shot,
no intent field), or `role:<name>.power` (CEC-derived input; On is a wake trigger).
Newest usable independent sample wins within the field's configured sources:
HA `changed` for power, `updated` for source, direct TV `at`.
`tv_sources(config, direct_available) -> &[Via]` selects `[TvDirect]` with available
webOS and `[Ha]` otherwise. `receiver_power_sources(config) -> &[Via]` selects `[Denon]` with
receiver observe, otherwise `[Ha]`; receiver source and speakers use `[Ha]`.
Two consecutive direct TV Unreachable samples establish literal `unreachable`
at the second sample's timestamp, agreeing with Off and refuting On. A single
failure, HA unknown/unavailable and receiver/probe Unreachable remain unusable;
no CEC evidence.
For power, HA `on|playing|paused|idle|buffering` normalize to `on` and
`off|standby` to `off`; other usable values stay raw. Expectations use the
same normalization; source/playback semantics are unchanged.

Evaluate after expectations on every handle, including Tick, but not during
warm-up or Shadow. For each field, in order:
1. Undefined intent clears grace and resolves any open drift, with current
   usable observation or `unknown`.
2. Unusable observation clears grace, leaving an open drift untouched.
3. A pending expectation on this subject/field clears grace but keeps an open
   drift; receiver source awaiting takeover settle counts as pending too.
   Other fields' expectations do not block.
4. Agreement clears grace and resolves an open drift only if its sample is
   strictly newer than the opening disagreement. Keep that timestamp with the
   open detail: stale agreement and loss of evidence do not resolve.
5. Disagreement starts grace on first observation. A changed value restarts
   grace; newer timestamps with the same value keep it. Unusable evidence clears
   grace, so continuous polling can report sustained drift without accumulating
   time across unavailable reconnect flaps. At grace, emit one `drift` row and
   freeze the opening sample timestamp until resolution.

`verify.drift_grace_s` defaults to 30 s, validated in 5–600 s. Injected monotonic
time measures grace from first disagreement, not historical HA sample time.
Intent changes close the old drift immediately and reset grace before comparing
the new intent. Entering Shadow resolves open drift with current usable evidence
or `unknown` and clears grace before cancelling expectations in that command pass.
Reload resets every grace clock, keeps the open set until re-evaluation,
and delays evaluation through three-second warm-up. Grace edits apply on
the next evaluation. Deadlines wake Tick exactly at the boundary. Clearing grace
on unusable evidence prevents expired-deadline Tick loops and rejects HA
reconnect off/unavailable/off/on flaps.

Rows have reason `drift` / `drift_resolved`, no role/transition/actions or
expectations, and verdict `none`. Optional `drift` carries subject, field,
intended, observed, via and onset wall time `since`. Resolution retains the
original intended and since so pairs can be joined. Onset uses observation
only when the handled event supplies the selected sample (matching HA entity
or direct TV); unrelated inputs and Tick use timer `drift_grace`. Resolutions
use observation with the selected source. No corrective actions; continuous
direct TV idle polling supplies fresh reporting evidence even between actions.
The per-field open set is in-memory only, exposed as `Snapshot.drift`, never
restored from historical ring rows; state remains version 2.


## Persistence, trace, replay

- `choregos.state.json` → `version: 2`: adds `next_decision_id`; ring entries
  gain `id`, `trigger`, `parent`, `expectations`, `verdict`. A v1 file is
  migrated in place: ids assigned in ring order, `verdict = none`. Ring entries
  are mutable until their verdict is final; the state file is written on every
  verdict change (same single-slot writer).
- Trace gains two topics: `choregos/decision` (the row at creation) and
  `choregos/verdict` (`{id, verdict, expectations}` at settlement), plus
  `tv/state` and `receiver/state`. `extract` keeps them; `replay` ignores
  decision lines (it recomputes) and consumes both direct observation topics.
- `replay` output: the decision line as today plus, when it settles, a
  `{"at", "verdict_for": id, "verdict", "expectations"}` line. The golden
  corpus is re-blessed in the same PR; expected differences are exactly the
  added `id`/`trigger`/`parent` fields and the new verdict lines (verdicts in
  the corpus will mostly be `unknown` for `tv` because the captures have no
  `tv/state` lines yet; documented in `tests/golden/README.md`).

## Exposure

- Ring/UI: `/decisions` rows show a verdict chip and expand to their
  expectations (expected / observed / via / latency). `drift`/`drift_resolved`
  rows instead show `subject.field intended → observed via <source>`, plus
  how long it lasted on resolution, with no verdict chip. The dashboard shows
  one amber pill per open drift next to room intent:
  `subject.field: observed, intended X, for <duration>`. These are reporting
  only; the broader surfaces come with the phase-2 redesign.
- Snapshot: the mutable decision ring exposes verdicts and `drift` details;
  `Snapshot.drift` contains currently open drifts. Per-role stats remain deferred.
- Home Assistant: `sensor.choregos_last_decision` (state = verdict; attributes
  id, role, reason, latency_ms). It ignores actionless drift rows.
  `binary_sensor.choregos_drift` (device class `problem`) uses retained
  `choregos/state/drift` JSON for its value and attributes: `state` is `ON`
  when `Snapshot.drift` is non-empty, otherwise `OFF`; `fields` lists
  `subject.field`, `since` is the oldest open onset (RFC3339, or null), and
  `details` contains the complete open drift records. Empty means
  `fields: []`, `since: null`, `details: []`. Availability follows
  `choregos/status`; the entity-state fingerprint includes the open set so
  drift-only changes publish.

## Not in this phase

Remediation, anticipatory wake, per-role deadline overrides, content modes,
richer identities header for mid-session replay (separate follow-up noted in
`docs/decisions.md` 2026-11-02).

## Delivery

1. **Implemented:** decision identity + trigger + parent; state v2 migration;
   trace `choregos/decision`.
2. **Implemented:** direct TV/console observations and room intent, without
   optimistic HA-cache writes.
3. **Implemented:** expectations + verdicts + `[verify]` deadlines; mutable
   persisted ring, verdict trace/replay lines and final drain; UI chip/details;
   HA last-decision sensor. Golden output updated with case explanations.
4. **Detection and HA/UI exposure implemented:** drift rows, grace configuration,
   trace/replay, open-set snapshot (PR A); dedicated HA problem sensor,
   open-set publication fingerprint, dashboard open-drift pills and
   decision-row detail/duration without verdict chips (PR B).

Each step is its own PR with Opus engine review and Luna test/doc review; 1–3
can ship as one release, 4 after a few days of verdicts.

## Decisions taken in review (2026-11-02)

1. `cec_only` counts as met. A configured Sony console gets an independent
   discovery truth source; other devices retain CEC-only observation.
2. Drift grace 30 s, direct polls every 2 s while an expectation is pending.
3. Home Assistant exposure as described.
4. Standby verification rows are folded into the parent's expectation.

## Step 3 implementation details / deviations

- Outcomes serialize as objects tagged by `kind`; `expect` and `observed`
  stay strings. Unknown optionally carries `observed: "absent"` so target
  NACK evidence is preserved without falsely claiming standby.
- Uncontested independent sources' fresh matches meet immediately, even after another
  source disagreed or became unreachable. CEC success comes only from the
  verification confirmed branch, never raw bus lookup; checked success keeps
  the probe wait and survives later cancellation. Independent disagreements
  are derived from each source's current sample, not retained across samples.
  Probe Unreachable and HA unavailable/unknown clear their source's earlier
  disagreement without confirming or refuting; direct TV Unreachable is evidence
  only after two consecutive failures, for Off and against On, retained literally.
  At the deadline, the newest current usable sample newer than the decision supplies `missed.observed`;
  if no such sample or checked CEC evidence exists, settle unknown.
- `cec_only` requires a checked CEC confirmation for roles permitting verification.
  Opted-out roles without a probe contribute no expectation; with a probe they
  accept only independent evidence. CEC On is not a refutation for them.
- No redundant `Snapshot.last_verdict`: derive the latest settled observable
  decision from the ring. Statistics and dedicated drift surfaces remain later work.
- Persisted pending rows interrupted by restart settle their unfinished fields
  unknown, preserving already completed evidence; old monotonic deadlines and
  effects are not resumed.
- Replay retains the five-second control-timer drain. Its additional reporting
  tail evaluates only expectation deadlines, never presence/off/standby timers.
- Bridge offline cancels only all-`[cec]` expectations; HA/direct/probe evidence
  remains usable. Reload cancels only relevant endpoint/policy/target/deadline
  changes, not unrelated configuration edits.
- Bus entries own fresh power provenance (`power_at`); no verifier-local CEC
  power cache exists. Reporting is never a control input.
