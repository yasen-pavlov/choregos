# Expectations and verdicts (phase 1 design, draft for review)

A decision today ends when the engine has emitted its actions. CEC sends come
back as ack/nack and feed only standby verification; HA calls, webOS off and
WoL return nothing to the engine. The ring has no decision identity, so nothing
can be attached to a decision after the fact. This phase closes the loop:
**decision = intent + expectations + verdict**, persisted, traced, replayable.

Reporting only. Nothing in this phase sends an action because a verdict was
`missed`.

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
  parent        Option<id>   for takeover_settled, standby_retry,
                            standby_unconfirmed, standby_target_absent follow-ups
  expectations  [Expectation]   empty for rows with no observable effect
  verdict       pending | achieved | partial | missed | unknown | none

Expectation
  subject   tv | receiver | speakers | role:<name>
  field     power | source | playback
  expect    "on" | "off" | "standby" | <source name> | "stopped"
  via       [tv_direct, ps5, ha, cec]   sources that may confirm, in priority order
  deadline  ms from `at`
  attempts  u8   CEC standby resends (replaces the standby_retry rows)
  outcome   pending | met { via, observed, latency_ms }
          | missed { observed }            deadline passed, last observation disagrees
          | unknown                        deadline passed, no observation at all
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
| off→on transition (wake) | `tv.power = on` (tv_direct, ha; 15 s) · `receiver.power = on` (ha; 10 s) · `speakers.playback = stopped` if `media_stop` was sent (ha; 5 s) · `role:<displaced>.power = standby` for every role stopped (cec; `takeover_settle_ms × (standby_retries + 1)`) |
| `takeover_settled` | `receiver.source = <role source>` if `select_source` was sent (ha; 10 s). Preset scripts have no observable → no expectation. |
| `off_timer_expired` | `tv.power = off` (tv_direct, ha; 15 s) · `receiver.power = off` (ha; 10 s) |
| `all_off`, `manual_standby` | `role:<r>.power = standby` per stopped role (cec; as above) |
| everything else | none |

A role's own `on` is what triggered the decision, so it is not an expectation.
`cec_only` counts as met: a console that leaves the bus in standby
(`verify_standby = false`) still lets its takeover read `achieved`, and the
expanded row keeps the honesty. The former `standby_retry` /
`standby_unconfirmed` / `standby_target_absent` rows are folded into the
displaced role's expectation (`attempts`, terminal outcome) and stop being
separate ring rows; golden corpus re-blessed in that PR, ops glossary updated.

Deadlines are per-field config under `[verify]` with the defaults above;
calibration may change the defaults in a later release, not the schema.

## Observations

Already flowing: `Event::HaState` (tv, receiver + `source`, speakers, device
media players), `Event::Frame` power replies `90 xx`, `Event::SendResult`.

New:

- **`Event::TvPower { state, at }`** from a direct webOS `getPowerState`
  (the SSAP session already used by `tv.off`). The runtime polls every 2 s from
  a `tv.power` expectation's creation until it settles; never otherwise. Traced
  as topic `tv/state` so replay sees it. This is the observation that makes the
  TV number honest and bypasses HA's reconnect blind spot.
- **`Event::ConsolePower { role, state, at }`** from a direct Sony-console
  discovery probe: one UDP datagram to port 9302 (`SRCH * HTTP/1.1`,
  `device-discovery-protocol-version:00030010`); reply `200` = awake, `620` =
  rest mode, no reply = off/unreachable. No pairing or account. Configured per
  role as `probe = { kind = "ps5", host = "…" }`; polled like `tv_direct`
  only while an expectation on that role is pending. Traced as `probe/state`.
  Requires Remote Play enabled on the console. Other consoles have no local
  equivalent on retail firmware and stay `cec_only`.
- **Optimistic HA cache writes removed.** `off_timer_expired` currently writes
  `tv`/`receiver = off` into the HA cache before anything happened
  (`engine.rs:913-934`). The verifier needs the cache to be observed truth
  only. The reason that write existed — a re-wake inside HA's lag must still
  send `wake_tv` — is kept by consulting the room intent (below) instead.

## Room intent and drift

The engine gets an explicit `RoomIntent { power: on|off, active_role,
receiver_source }`, derived from the same transitions that create decisions.
It is what the verifier compares against once all decisions have settled:

- A `drift` row (`reason = drift`, `subject.field`, intended, observed) is
  recorded when an observed field disagrees with the intent for longer than
  `verify.drift_grace_s` (default 30 s) with no decision pending; one row per
  onset, a `drift_resolved` row when it agrees again. Examples: receiver turned
  off by hand while the room is on; TV turned on by its remote while the room
  is off.
- Drift rows carry no actions. Remediation is out of scope for this phase.

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
- Snapshot: `last_verdict`, `drift: Option<Drift>`; per-role stats computed
  from the ring on demand (wake achieved %, standby confirmed %, p50/p95
  latency) for the future device page.
- Home Assistant: `binary_sensor.choregos_drift` (attributes: subject, field,
  intended, observed, since) and `sensor.choregos_last_decision` (state =
  verdict; attributes id, role, reason, latency_ms). Same vocabulary as the UI.

## Not in this phase

Remediation, anticipatory wake, per-role deadline overrides, content modes,
richer identities header for mid-session replay (separate follow-up noted in
`docs/decisions.md` 2026-11-02).

## Delivery

1. Decision identity + trigger + parent; state v2 migration; trace
   `choregos/decision`. No behaviour change; golden re-bless (fields only).
2. `TvPower` observation + `tv/state` trace; optimistic cache write replaced
   by room intent.
3. Expectations + verdicts + `[verify]` config; verdict lines in replay;
   `/decisions` chip; HA sensors.
4. Drift rows.

Each step is its own PR with Opus engine review and Luna test/doc review; 1–3
can ship as one release, 4 after a few days of verdicts.

## Decisions taken in review (2026-11-02)

1. `cec_only` counts as met; the Sony console gets a native discovery probe as an
   independent truth source if the console answers it (Remote Play must be
   enabled); other consoles stay `cec_only`.
2. Drift grace 30 s, direct polls every 2 s while an expectation is pending.
3. Home Assistant exposure as described.
4. Standby verification rows are folded into the parent's expectation.
