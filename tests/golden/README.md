# Golden replay corpus

## 2026-11-06 — Direct TV truth source

All seven configs use webOS: TV legs now list only `tv_direct`. HA remains
unchanged for receiver power/source and speakers. None configures receiver
`observe`, so direct receiver support must not change any receiver leg.
Five captures contain no `tv/state`: their formerly HA-met TV legs become
unknown at the 45-second deadline. Verdict settlement timestamps therefore
move to that deadline (and can reorder relative to other rows); action and
decision timestamps never change.

- `aether-console-swap-probe`: decision 1 still achieves via direct On,
  19101 ms, unchanged. Only its TV allowed-source list changes.
- `aether-receiver-off-first`: wake 1 remains unknown. Off 4 remains achieved,
  but TV changes from HA Off (3244 ms) to direct `unreachable` (6147 ms),
  sampled at 14:13:17.067 UTC: the second consecutive failure. Review tightened
  first-failure acceptance (4150 ms at 14:13:15.070) to require that second
  sample. Verdict remains achieved and receiver pre-settlement stays met/0.
- `aether-console-wake-and-streamer-takeover`: TV legs 1/7/8/11 change
  HA met → unknown; achieved → partial. Former TV latencies were respectively
  19090/3213/32331/3157 ms.
- `aether-restart-mid-takeover`: TV legs 7/8/11/12 change HA met → unknown;
  achieved → partial; former latencies 3190/3396/3395/3433 ms.
- `aether-standby-retry`: TV legs 1/7 change HA met → unknown (former
  latencies 29756/3299 ms); verdict 1 achieved → unknown (TV-only),
  verdict 7 achieved → partial.
- `aether-streamer-to-console-pause-standby`: TV legs 1/4/5/11 change
  HA met → unknown (17894/3147/3379/3170 ms); verdict 1 achieved → unknown,
  the other three achieved → partial.
- `tasmota-console-and-kodi`: TV legs 1/4/5/8 change HA met → unknown
  (30046/3716/3374/3175 ms); achieved → partial.

No new decisions, drift rows or verdict rows are introduced. The invariant
compares rows by `(decision id | verdict_for)` after removing TV expectations,
the aggregate verdict, and only TV-bearing verdict settlement timestamps;
every remaining field, including receiver outcomes/latencies, must match.
Row-key comparison permits the explained deadline-driven settlement reordering.
The review round changes only `aether-receiver-off-first` verdict 4's TV
latency (4150 → 6147 ms) and settlement time (14:13:15.070 → 14:13:17.067).
All six other cases are byte-identical to the first implementation's blessing.


These anonymized real-use windows freeze the current offline replay output, not
an idealized reconstruction of the live controller. Each directory contains
`trace.jsonl`, its behavioural `config.toml`, and timestamped `expected.jsonl`.
The integration test discovers directories in name order, requires decisions in
every case, and compares output byte-for-byte. Update intentional behaviour
changes with `UPDATE_GOLDEN=1 cargo test --locked --test golden` and explain the
differences in the same PR. Configurations preserve the captured role policies,
timing, presets and speaker settings; generic web settings satisfy the current
configuration schema but are unused by replay. No test contacts real devices.
Creation lines retain `at`, `id`, `parent`, `trigger`, `role`, `from`, `to`,
`reason`, `actions`, then add `expectations` and `verdict`. Settlement lines
carry `at`, `verdict_for`, `verdict`, `expectations`; IDs restart at 1 per replay.
Drift onset/resolution creation rows additionally carry optional `drift` detail;
other rows omit that key. Drift evaluation participates in the existing
last-input-plus-five-second control drain, never the expectation-only tail.

## 2026-11-05 drift detection regression check

All seven expected streams remain byte-identical; no re-bless. In
`aether-console-wake-and-streamer-takeover`, wake 8 at 18:01:25.295 settles TV
On via HA at 18:01:57.635. HA then reports Off at 18:02:51.559, unavailable
at 18:03:03.796, Off at 18:03:26.528 and On at 18:03:26.536 (the final two
sample timestamps differ by about 1 ms). This reconnect flap must not add
drift: unavailable clears grace and the new Off sample starts a fresh clock
which On immediately cancels. Identical sample redelivery, unlike a new
timestamp/value, preserves grace. Injected replay tests cover both rules,
the flap and unusable evidence arriving at/after a grace deadline.

## 2026-11-02 room-intent regression check

An initial run with intent starting Off added redundant power-on actions to
the first wake in three windows whose HA cache already said On. Startup
intent now starts Unknown: until the first decision establishes intent,
only HA's observed cache governs wake suppression, preserving restart
behavior. Known Off intent still forces power-on through post-off HA lag.
The corpus must remain byte-identical; no re-bless is needed.

## 2026-11-03 expectations/verdicts re-bless

Re-blessed with `UPDATE_GOLDEN=1 cargo test --locked --test golden`. These
five original fixtures have no `tv/state` or `probe/state` lines: no result is attributed
to a direct source. TV/receiver power confirmation uses HA `changed` after
the dispatched action; TV now has a 45-second observation window, allowing
the captured HA reconnect lag while recording actual latency separately.
Source confirmation uses HA `updated`; these older traces lack that field,
so their line timestamps supply attribute freshness without altering power
freshness. A takeover in an already-On room has no redundant power
expectation. Opted-out standby roles without a probe contribute no
expectation: CEC On is not a refutation of a console which sleeps silently
after answering. `cec_only` still requires actual fresh standby evidence
for roles which permit CEC verification. No drift rows are introduced.

| Case | Previous decision rows | Removed rows | Retained decision rows | Added verdict lines |
| --- | ---: | --- | ---: | ---: |
| `aether-console-wake-and-streamer-takeover` | 11 | none | 11 | 6 |
| `aether-streamer-to-console-pause-standby` | 11 | none | 11 | 7 |
| `aether-restart-mid-takeover` | 13 | none | 13 | 6 |
| `aether-standby-retry` | 8 | old id 6 retry follow-up | 7 | 3 |
| `tasmota-console-and-kodi` | 8 | none | 8 | 5 |

- **Console wake / streamer takeover:** source selections 2 and 9 achieve via
  HA updates. Wakes 1 and 8 achieve: TV HA On arrives after 19.090 s and
  32.331 s, both inside the 45 s window; receiver On also arrives in time.
  Takeover 4 has verdict `none`: room power was already On and Console A
  explicitly opted out of CEC verification without an independent probe.
  That does not claim the console slept. Room-off 7 and 11 achieve via HA.
- **Streamer / console pause-standby:** initial wake 1 achieves with TV HA On
  after 17.894 s. Room-off 4 and 11, wake 5, and source selection 6 achieve.
  Takeover 8 achieves through the streamer's fresh `cec_only` standby report.
  Source-settle 9 now achieves `Console B` at 23:23:38.663, 72 ms after
  selection: attribute freshness comes from the HA update's line timestamp,
  not the unchanged HA power-state `last_changed`.
- **Restart mid-takeover:** initial wake 1 has verdict `none` because room
  power was already On. Takeover 4 also has `none`: Console A opted out and
  has no probe. Room-off 7 and 11, wakes 8 and 12, and source selections 9
  and 13 all achieve via HA. The identities header still does not restart replay.
- **Standby retry:** old row 6 disappears; its resend is `attempts: 1` on
  wake 4's streamer expectation. That wake achieves through `cec_only`;
  the already-On TV has no power expectation. Initial wake 1 achieves after
  TV HA On arrives at 29.756 s. Final room-off 7 (formerly 8) achieves via HA.
  Removing the intermediate row renumbers later ids/parents without changing
  any original actions, timestamps or trigger provenance.
- **Tasmota console / Kodi:** source selection 2 achieves. Console wake 1
  achieves with TV HA On after 30.046 s and receiver HA On after 2.059 s.
  Kodi wake 5 and room-off 4 and 8 also achieve via HA. No standby follow-ups
  existed in this window and no CEC/direct confirmation is invented.

The fields-only invariant was checked against base `86f7d19`: remove old
verification follow-ups, remove new verdict lines and `expectations`/`verdict`
keys, and normalize the sequential id/parent numbering after the one removed
row. All five remaining streams compare equal, including timestamps, roles,
transitions, reasons, triggers, actions and parent links. Totals: 50 retained
decision rows, 27 verdict lines, one removed retry follow-up. Normalization is
unnecessary in four cases.

The exact verdict-field distribution across creation and settlement lines is:

```text
27 "verdict":"achieved"
23 "verdict":"none"
27 "verdict":"pending"
```

All 27 settled verdicts achieve; negative outcomes remain covered by injected
engine tests, rather than forcing real captured successes into false failures.
The review fix preserves these counts and every original decision field. The
corpus does not exercise transient Standby/On, delayed retry warm-up, or a Kodi
takeover/all-off. Dedicated replay regressions cover those gaps: p12/p7/p10
retain main's exact CEC send sequence, Kodi gets no fabricated CEC expectation,
and a polling-role trace cannot generate presence/off actions in the reporting
tail. Any allowed source may confirm despite a prior direct disagreement.
The final checked-confirmation fix changes only two settlement times: pause-
standby verdict 8 moves from 23:23:37.949 to its 23:23:38.591 verification check
(after the same-tick source decision); retry verdict 4 moves from 11:43:49.441
to its 11:43:50.960 check. Their `cec_only` outcomes, resend counts and all
creation rows remain unchanged. These timing changes prevent transient CEC
Standby/On chatter from being reported as success before verification checks it.


## 2026-11-05 receiver off deadline re-bless

`[verify] receiver_off_s` (default 20) replaced the shared 10 s deadline on the
receiver's off expectation after a live room-off whose receiver reported off at
10.17 s settled `unknown`/`partial`. Every receiver `power = off` row now carries
`deadline_ms: 20000`; no outcome, verdict, timestamp or action changed.

## Cases

**`aether-receiver-off-first` — 4 decisions + 2 verdict lines.**
2026-11-03 13:57:45–14:14:30 UTC: the receiver obeys the streamer's broadcast
Standby before the room-off timer. Receiver HA Off at 14:13:09.730979 precedes
replay decision 4 at 14:13:10.920; its expectation now starts
`met { via: "ha", observed: "off", latency_ms: 0 }`. TV HA Off confirms
3,244 ms later, settling the room-off decision `achieved` at 14:13:14.166.
Before the fix it settled `partial` at 14:13:20.920 because the receiver had
no newer HA sample and became unknown at its ten-second deadline.
The recorded old verdict remains evidence, ignored by replay.

The originally requested 14:10 start contained no wake and replayed no
decisions: identity seeds do not carry role power. With approval the start
extends to just before the preceding Streamer wake at 13:57:49.084, without
inventing initial state. Replay verifies its identity at 13:57:50.309 and
reconstructs `ActiveSource` rather than the live `PowerOn`. A cold HA cache
causes TV/receiver wake actions absent live; neither gets new On evidence
in this window, so that replay wake remains `unknown`. The first run without
re-blessing showed changes only in this new case; all six existing cases
remained byte-identical. Only the new case's two room-off lines were blessed.

**`aether-console-swap-probe` — 11 decisions + 5 verdict lines.**
2026-11-03 13:50–13:58:30 UTC: four consecutive source activations/takeovers,
Console B → Console A → Kodi → Streamer. Console B has CEC verification
disabled and an independent probe. Its post-stop Awake sample is followed
by Unreachable samples before late Standby, after the old six-second deadline.
Probe-only verification now uses the full configured 45-second window and
settles the Console A takeover via the probe rather than falsely missing.
This capture also includes direct TV and HA observations. Extracted probe
roles are renamed to `console_b`; `ps5` remains only the public protocol/via
identifier, including the captured decision/verdict evidence. The five
earlier cases remain byte-identical.

Replay decision 4 corresponds to captured decision 700: before the fix it
settled `missed { observed: "on" }` at six seconds; afterward it settles
`met { via: "ps5", observed: "standby", latency_ms: 10103 }` at
13:53:03.484 UTC. The trace's recorded old verdict is retained as evidence
but ignored by replay. Settled verdicts 1, 4, 5, 8 and 11 all achieve;
their other confirmations use direct TV or HA observations.

**`aether-console-wake-and-streamer-takeover` — 11 decisions.**
2026-11-01 16:25–18:35 UTC: Console A wake stops playing speakers, then Streamer
takes over with directed Standby/Give Device Power Status (`36`/`8F`). Streamer
standby/wake cycles exercise delayed room-off and takeover-settled actions.
The reason sequence matches the live ring for this window.

**`aether-streamer-to-console-pause-standby` — 11 decisions.**
2026-11-01 22:05–23:50 UTC: Streamer wakes, enters standby quickly, wakes again,
and yields to Console B. The takeover sends Play: Still/Standby/Give Device
Power Status (`41 25`/`36`/`8F`); Console B's Inactive Source starts the off timer.
The first wake reason differs from live as described below.

**`aether-restart-mid-takeover` — 13 decisions.**
2026-11-01 09:10–09:45 UTC: Console A wakes and yields to Console B; an identities
header at 09:20:34 records a service restart without resetting replay. Subsequent
active/inactive-source traffic, room-off, another Console A wake and presence
timeout cover continuity through that header. The live ring has 14 decisions
excluding service-start/mode rows; the differences are explained below.

**`aether-standby-retry` — 7 decisions + 3 verdict lines.**
2026-11-02 10:15–12:40 UTC: Streamer's initial wake precedes its takeover by
Console B, including a real resend folded into its parent expectation, then Inactive Source and room-off.
The window begins at 10:15 rather than 11:40 so Streamer's On state is established
from input traffic, not invented from the identities header. The source daily
trace has no pre-window availability record to copy. The first wake reason
still differs from live as described below.

**`tasmota-console-and-kodi` — 8 decisions.**
2026-10-31 16:20–17:10 UTC: the Tasmota adapter sees Console A wake, presence
timeout and room-off, then Kodi's Image View On, Standby and room-off. The copied
pre-window LWT establishes bridge availability so presence polling is active.
The reason sequence matches the live ring for this window.

## Known replay-vs-live differences

Identity headers contain learned keys, not verification, role power, cached HA
state or pending timers. Restored identities therefore begin unverified and with
unknown power. A live controller that already verified a device can react to its
first wake immediately, while replay buffers wake signals until the keys are
verified. A later Active Source can replace an earlier buffered Power On or Text
View On. Consequently:

- In `aether-console-swap-probe`, live Console B `TextViewOn` at
  13:52:20.040 becomes replay `ActiveSource` at 13:52:20.269, when the
  physical-address reply verifies its identity. This shifts the TV direct
  observation latency from 19,330 ms live to 19,101 ms in replay; the
  observation itself remains at 13:52:39.371.
- In `aether-streamer-to-console-pause-standby`, the first live `PowerOn` at
  22:08:30.777 becomes replay `ActiveSource` at 22:08:33.254, when the physical
  address reply verifies Streamer. Remaining reasons match.
- In `aether-standby-retry`, the first live `PowerOn` at 10:17:47.209 becomes
  replay `ActiveSource` at 10:17:49.557 for the same reason. Other retained
  decision reasons match; the retry is now an expectation attempt, not a row.
- In `aether-restart-mid-takeover`, live Console B `TextViewOn` at 09:20:14.043
  becomes replay `ActiveSource` at 09:20:15.473, after its physical address reply.
  The mid-window identities header intentionally does not restart the replay
  engine, so live's post-restart `ActiveSource` at 09:20:44.989 is a duplicate On
  signal in replay and produces no extra decision. Remaining reasons match.

Windowed replay and extraction share the last pre-window availability record,
when present, at its original timestamp. Extracted fixtures retain it after the
identities seed; both paths therefore share a warm-up clock and bridge availability,
not device power. Both run control timers through the last selected input plus
five seconds. Outstanding expectation deadlines then settle in a reporting-only
tail, with no presence/off/retry/source timers or actions, independently of
the input-selection `--until` bound.
Starting a fixture mid-session can still omit an already-running role and its
later takeover/standby decisions. A richer trace snapshot is a documented,
unimplemented follow-up, not synthetic state added to these expected files.

Trace timestamps have millisecond precision, while persisted live decisions keep
nanoseconds. Replay timer decisions occur at exact injected deadlines; live event
loop scheduling can execute slightly later. Cold HA caches can also change
individual wake/source-selection actions. These are replay baselines, not a claim
that every timestamp or action equals the persisted live ring byte-for-byte.

Each full private trace was replayed with the matching window and behavioural
configuration, then its decision strings were mapped to these public names.
All five resulting JSONL outputs were byte-identical to the fixture replays.
