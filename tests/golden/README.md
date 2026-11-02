# Golden replay corpus

These anonymized real-use windows freeze the current offline replay output, not
an idealized reconstruction of the live controller. Each directory contains
`trace.jsonl`, its behavioural `config.toml`, and timestamped `expected.jsonl`.
The integration test discovers directories in name order, requires decisions in
every case, and compares output byte-for-byte. Update intentional behaviour
changes with `UPDATE_GOLDEN=1 cargo test --locked --test golden` and explain the
differences in the same PR. Configurations preserve the captured role policies,
timing, presets and speaker settings; generic web settings satisfy the current
configuration schema but are unused by replay. No test contacts real devices.

## Cases

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

**`aether-standby-retry` — 8 decisions.**
2026-11-02 10:15–12:40 UTC: Streamer's initial wake precedes its takeover by
Console B, including a real `standby_retry`, then Inactive Source and room-off.
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

- In `aether-streamer-to-console-pause-standby`, the first live `PowerOn` at
  22:08:30.777 becomes replay `ActiveSource` at 22:08:33.254, when the physical
  address reply verifies Streamer. Remaining reasons match.
- In `aether-standby-retry`, the first live `PowerOn` at 10:17:47.209 becomes
  replay `ActiveSource` at 10:17:49.557 for the same reason. All remaining reasons,
  including `standby_retry`, match.
- In `aether-restart-mid-takeover`, live Console B `TextViewOn` at 09:20:14.043
  becomes replay `ActiveSource` at 09:20:15.473, after its physical address reply.
  The mid-window identities header intentionally does not restart the replay
  engine, so live's post-restart `ActiveSource` at 09:20:44.989 is a duplicate On
  signal in replay and produces no extra decision. Remaining reasons match.

Windowed replay and extraction share the last pre-window availability record,
when present, at its original timestamp. Extracted fixtures retain it after the
identities seed; both paths therefore share a warm-up clock and bridge availability,
not device power. Both drain final timers through the last selected input plus
five seconds, independently of the input-selection `--until` bound.
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
