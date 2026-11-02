# Roadmap

Choregos is feature-complete for a single room: it has run the living room
since 2026-10-26 without an unexplained room flip. What follows is polish and
added capability on top of a controller that already works, so the roadmap is
ordered to keep that true while changing a lot.

## Principles

- **Break on purpose, never by accident.** A golden replay corpus built from
  real traces runs in CI; an engine change that alters any recorded decision
  fails the build until the corpus is re-blessed in the same change. The corpus
  makes behaviour drift visible, it does not forbid it.
- **The configuration file is the source of truth.** It is a TOML file, meant
  to live in version control. UI editing writes that file (secrets redacted, as
  today); forms are a convenience over it, never a separate store. The file
  keeps parsing across releases until a deliberate schema break for v1.0.
- **Observe before act, where acting can flip the room.** Features that would
  switch devices on their own (pre-warming, drift remediation) ship as recorded
  "would" rows first and act only after their verdicts have been reviewed.
  Everything else ships as a feature.
- **No lab assumptions in new code.** Room, device set and entity ids come from
  configuration; device quirks live in shareable profiles rather than prose.
  Usability by other households is not the priority yet, but nothing new should
  make it harder.

## Phases

### 0. Regression corpus and operations notes

- `tests/golden/`: curated windows from live traces (incidents, one ordinary
  evening) with identities headers, replayed in CI against the recorded
  decision sequence. Start curated, grow when a change deserves it.
- `docs/operations.md`: volume ownership, logging, state file lifecycle,
  incident replay, rollback.

### 1. Expectations and verdicts

A decision currently ends when the bridge acknowledges the send. It should end
when the room is observed in the state the decision intended.

- Each decision carries an **expectation**: per device, the field, the expected
  value, the source that can confirm it (CEC power status, Home Assistant
  entity, webOS SSAP, receiver) and a deadline.
- A **verdict** (`achieved`, `partial`, `missed`, `unknown`) with observed
  values and latencies is recorded on the decision row and persisted with it.
  Devices with no non-CEC truth source are marked `cec_only` rather than
  pretended to be verified.
- Deadlines come from measured latencies in the existing decision history, not
  guesses; the measurement is the first deliverable.
- The same comparison runs continuously against the room's intended state and
  records `drift` rows (a receiver switched off by hand while the room is on).
  Reporting only; remediation is opt-in per field.
- Per-device statistics fall out of the verdicts: wake success, standby
  confirmation, p50/p95 latency.

The decision row schema is persisted and traced, so it is designed and reviewed
before implementation.

### 2. Interface redesign

A redesign of the whole product surface, not page-by-page additions:

- **Room cockpit**: current state, consistency across truth sources, one-tap
  actions; usable from a phone.
- **Flight recorder**: bus frames, decisions and verdicts linked both ways
  (frame → decision it triggered or belonged to; decision → the frames it
  caused), with a scrubbable window around any decision.
- **Device registry**: identity keys, each truth source side by side (CEC says,
  Home Assistant says, SSAP says), decision history for the device, statistics,
  quirk notes.
- **Configuration**: forms with validation driving the TOML file; raw editor as
  the escape hatch.

Information architecture is drafted first, the screens are designed as a set
against real snapshot data, then implemented. Server-rendered HTML stays the
default; the flight-recorder timeline may use a small vendored script without a
build step.

### 3. Multiple rooms and v1.0

- `[[room]]` scopes bridge, TV, receiver, roles and presets; today's flat layout
  parses as an implicit single room.
- Configuration naming audited once, schema versioned, then frozen as v1.0.0.

### 4. After v1.0, by appetite

- **Anticipatory wake**: use measured cold-start latency and decision history
  to pre-warm the receiver when a wake is likely; `would` rows first.
- **Content-aware modes**: per-role scenes (receiver sound mode, TV picture
  mode, speaker pairing) expressed as extra expectations on the takeover, so a
  scene that did not apply shows as `partial`.
- **Agent access**: an MCP endpoint on the existing server exposing decisions,
  bus windows, devices and fixture extraction, so triage does not need shell
  access to the host.
- **Device twins**: encode learned device behaviour as models that drive a
  simulated bus for property tests, and double as shareable profiles.
- **Usable by others**: onboarding, generic discovery, profiles, documentation.

## Not planned

Suspending the Kodi box, polling a console that leaves the bus, a metrics endpoint, replacing
Home Assistant as the device API gateway, additional console standby opcodes,
removing the Tasmota adapter. Each was considered and rejected with reasons in
[decisions.md](decisions.md) or the project history.
