# Expectations and verdicts (phase 1, steps 1–3 implemented)

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
              | {"kind":"timer","name":"room_off"|"takeover_settle"|"presence"|"standby_verification"|"warmup"}
              | {"kind":"service","name":"started"|"migrated"}
  parent        Option<id>   for takeover_settled follow-ups
  expectations  [Expectation]   empty for rows with no observable effect
  verdict       pending | achieved | partial | missed | unknown | none

Expectation
  subject   tv | receiver | speakers | role:<name>
  field     power | source | playback
  expect    "on" | "off" | "standby" | <source name> | "stopped"
  via       [tv_direct, ps5, ha, cec]   allowed confirming sources; any fresh match may meet
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
| off→on transition (wake) | `tv.power = on` only if `WakeTv` was sent (tv_direct, ha; 45 s) · `receiver.power = on` only if `turn_on` was sent (ha; 10 s) · `speakers.playback = stopped` if `media_stop` was sent (ha; 5 s) · `role:<displaced>.power = standby` only for stopped CEC policies with verification enabled or any stopped role with a probe (CEC retry window, probe poll window, or their maximum; source policy below) |
| `takeover_settled` | `receiver.source = <role source>` if `select_source` was sent (ha; 10 s). Preset scripts have no observable → no expectation. |
| `off_timer_expired` | `tv.power = off` only if `TvOff` was sent (tv_direct, ha; 45 s) · `receiver.power = off` only if receiver `turn_off` was sent (ha; 20 s — the receiver integration reports off 0.2–10 s after the command, on in a steady 2 s) |
| `all_off`, `manual_standby` | Same standby eligibility: CEC policies with verification enabled, or probe-only for a stopped role with a probe; no CEC expectation for HA media-stop/none policies |
| everything else | none |

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
`tv_direct`, or `ps5`, never `cec`) matches and is timestamped in
`[decision.at - deadline_ms, decision.at)`. A newer disagreeing sample blocks
an older match. The sample must also be newer than every still-pending
decision carrying an expectation on the same subject and field, preventing
evidence predating an unresolved command from satisfying a later action.
The deadline bounds sample age, not room-cycle duration. HA uses `changed`
for power/playback and `updated` for source attributes; direct sources use
their timestamps.
If every expectation is already satisfied, the decision is `achieved` at
creation. CEC still requires fresh evidence at its due verification check.
Otherwise evaluation and deadline freshness remain unchanged: observations
arriving after creation must be newer than the decision, not merely recent.

Deadlines are per-field config under `[verify]` with the defaults above;
calibration may change the defaults in a later release, not the schema.
The TV default is a 45-second observation window: HA can reconnect 20–33 s
after a WoL wake, while the observed latency is recorded independently.
HA power/playback freshness uses `changed > decision.at`; receiver source
freshness uses `updated > decision.at` from HA `last_updated`. Trace
`ha/state.updated` preserves that timestamp; old traces fall back to line `ts`.


## Observations

Already flowing: `Event::HaState` (tv, receiver + `source`, speakers, device
media players), `Event::Frame` power replies `90 xx`, `Event::SendResult`.

New:

- **`Event::TvPower { state, at }`** from a direct webOS `getPowerState`
  (the SSAP session already used by `tv.off`). Step 2 polls every 2 s after an
  active TV wake/off dispatch until target or a 45 s window ends. Traced as
  `tv/state`, this bypasses HA's reconnect blind spot.
- **Optimistic HA cache writes removed (step 2).** Off requests no longer write
  TV/receiver power into HA's observation cache. A separate room intent starts
  Unknown and follows wake/off decisions; a wake sends TV and receiver power-on
  whenever intent is Off or the corresponding HA cache is not On.
  Cached On is current only if its HA change timestamp is newer than the last
  room-off request; the timestamp remains across repeated wakes during HA lag.
  All-off changes power intent only when its room-off timer expires.

## Room intent and drift

The engine gets an explicit `RoomIntent { power: unknown|on|off, active_role,
receiver_source }`, derived from the same transitions that create decisions.
It is what the verifier compares against once all decisions have settled:

- A `drift` row (`reason = drift`, `subject.field`, intended, observed) is
  recorded when an observed field disagrees with the intent for longer than
  `verify.drift_grace_s` (default 30 s) with no decision pending; one row per
  onset, a `drift_resolved` row when it agrees again. Examples: receiver turned
  off by hand while the room is on; TV turned on by its remote while the room
  is off.
- Drift rows carry no actions. Remediation is out of scope for this phase.

Intent, direct observations and verdict comparison are implemented in steps 1–3;
drift remains step 4. A per-device `ps5` discovery probe
provides Awake/Standby/Unreachable over unicast UDP; it requires Remote Play
and a reachable UDP path from the Choregos host. Its trace topic is `probe/state`.


## Persistence, trace, replay

- `choregos.state.json` → `version: 2`: adds `next_decision_id`; ring entries
  gain `id`, `trigger`, `parent`, `expectations`, `verdict`. A v1 file is
  migrated in place: ids assigned in ring order, `verdict = none`. Ring entries
  are mutable until their verdict is final; the state file is written on every
  verdict change (same single-slot writer).
- Trace gains two topics: `choregos/decision` (the row at creation) and
  `choregos/verdict` (`{id, verdict, expectations}` at settlement), plus
  `tv/state`. `extract` keeps them; `replay` ignores decision lines (it
  recomputes) and consumes `tv/state`.
- `replay` output: the decision line as today plus, when it settles, a
  `{"at", "verdict_for": id, "verdict", "expectations"}` line. The golden
  corpus is re-blessed in the same PR; expected differences are exactly the
  added `id`/`trigger`/`parent` fields and the new verdict lines (verdicts in
  the corpus will mostly be `unknown` for `tv` because the captures have no
  `tv/state` lines yet; documented in `tests/golden/README.md`).

## Exposure

- Ring/UI: `/decisions` rows show a verdict chip; a row expands to its
  expectations (expected / observed / via / latency). Minimal; the real
  surfaces come with the phase-2 redesign.
- Snapshot: the existing mutable decision ring exposes verdicts; the newest
  settled observable row supplies the last-decision sensor. Per-role stats
  and a dedicated drift snapshot are deferred with their future surfaces.
- Home Assistant: `sensor.choregos_last_decision` (state = verdict; attributes
  id, role, reason, latency_ms). The drift binary sensor is step 4.

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
4. **Not implemented:** drift rows/sensor (intentionally out of this task).

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
- Independent sources' fresh matches meet immediately, even after another
  source disagreed or became unreachable. CEC success comes only from the
  verification confirmed branch, never raw bus lookup; checked success keeps
  the probe wait and survives later cancellation. Independent disagreements
  are derived from each source's current sample, not retained across samples.
  Direct Unreachable and HA unavailable/unknown clear that source's earlier
  disagreement without confirming or refuting. At the deadline, the newest
  current usable sample newer than the decision supplies `missed.observed`;
  if no such sample or checked CEC evidence exists, settle unknown.
- `cec_only` requires a checked CEC confirmation for roles permitting verification.
  Opted-out roles without a probe contribute no expectation; with a probe they
  accept only independent evidence. CEC On is not a refutation for them.
- No redundant `Snapshot.last_verdict`: derive the latest settled observable
  decision from the ring. Statistics and drift exposure remain later work.
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
