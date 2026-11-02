# Operations

For Docker installations using a writable directory mounted at `/config`.
Commands below assume the compose service and container are named `choregos`;
`./choregos` is the host-side config directory. Adapt paths for your volume.
Use a matching local `choregos` CLI for offline replay and extraction.

## Files in the config volume

- **`choregos.toml`** is the configuration source of truth. The UI validates
  and atomically replaces the editor text, preserving comments and order.
  Secrets are environment-only and are not serialized into the configuration;
  saving is not a scrubber for secrets pasted into comments. Back up the file
  before manual edits, and stop the service while editing it outside the UI.
- **`choregos.state.json`** version 2 holds learned identities, the newest 2000
  decisions (newest last), and `next_decision_id`; version 1 is migrated in place.
  It contains no live power or active-source state. Deleting it while stopped
  loses the ring and id continuity; identities must be re-learned through
  interrogation or the next device wake. Restored identities
  also need bus confirmation; confirmations older than 30 days are discarded.
- **`cec-bus.<UTC date>.jsonl`** holds daily replay traces, including identity
  headers, CEC traffic/results, bridge availability, accepted commands and HA
  engine inputs. `[trace] keep_days` controls retention (default 30, range
  0–365; 0 disables capture). Startup and daily rollover prune old files.
  Trace write failures warn without stopping control.

Decision ids are unique within a state-file lineage, not globally: after a crash
or failed asynchronous writes, restart resumes from the last persisted counter.
Deleting or restoring state can also reuse ids already present in traces or logs;
correlate trace decision rows by `(ts, id)`, not by id alone.

**The container runs as UID/GID 65532.** Both the directory and its files must
be accessible to that user. A root-owned mode-0600 config causes `Permission
denied` on startup and, with the compose restart policy, a restart loop.
Install replacement files with explicit ownership, while stopped:

```sh
docker compose stop choregos
sudo cp ./choregos/choregos.toml ./choregos/choregos.toml.backup
sudo install -d -o 65532 -g 65532 -m 0700 ./choregos
sudo install -o 65532 -g 65532 -m 0600 ./replacement.toml ./choregos/choregos.toml
docker compose up -d choregos
```

Mount the directory, not a single file: config/state saves use atomic rename.
`docker logs choregos` may be empty with a non-default logging driver; inspect
that driver's destination and check `/healthz` rather than assuming silence
means success.

## Health

```sh
curl --fail --silent --show-error http://127.0.0.1:8095/healthz
```

The unauthenticated JSON endpoint reports `mqtt_connected`, `ha_connected`,
`bridge_online` and `mode`. The image health check tests that the web process
answers, not that every external connection is healthy. The dashboard header's
`mqtt`, `ha` and `bridge` pills show these connections; the mode and warming
badges explain whether power actions are enabled or deferred.

With MQTT connected, `bridge_online: false` means no supported online message
has been accepted for the configured bridge, or it explicitly reported offline.
Check the retained Tasmota `tele/<topic>/LWT` (`Online`) or aether
`aether/<node>/status` (`online: true`, protocol version 1). Reflashing to a
different firmware or renaming a node/topic can leave Choregos subscribed to a
name that no longer publishes availability. An unsupported protocol version
also leaves the bridge offline. This field is not a test of every CEC device.

HA discovery provides `binary_sensor.choregos_bridge` and diagnostic sensors
`sensor.choregos_bridge_signal` (dBm), `sensor.choregos_bridge_link_count`
(Wi-Fi connections since boot) and `sensor.choregos_bridge_uptime` (seconds).
HA may retain operator-renamed entity IDs. The dashboard additionally shows
telemetry age, weak/stale telemetry warnings and native bridge line faults;
old telemetry is not proof of a current connection.

## Direct TV observations

When `tv.off.kind = "webos"`, an active TV wake/off dispatch starts a direct
`getPowerState` window using the configured pairing key. `[verify].tv_poll_ms`
is the cadence (default 2000 ms, range 500–10000); `tv_window_s` is the total
window (default 45 seconds, range 5–300). It ends at the requested state or
deadline, and a newer TV action replaces the previous window. Shadow mode
does not start direct polling. HA-only TV power-off has no direct source.

The dashboard TV row keeps HA state and shows the last direct state and its age.
`unreachable` means the direct query failed (for example, connection failure,
timeout, unsupported power-state service or an unrecognized response); it
does **not** prove the TV is off. A pairing-required warning stops direct polling
until adapter restart; fix the environment pairing key and restart the service.
No direct observation triggers corrective actions. Old direct data remains
visible with its age after the window closes.

Room intent starts Unknown after restart and is not restored from disk. Until
a wake/off decision establishes intent, only the HA cache governs wake
suppression, so a room already On does not receive redundant power requests.
An Off intent always causes the next TV/receiver wake request, even when HA
still reports On; room-off requests never overwrite HA's observed state or timestamps.
The last room-off timestamp remains across wakes: HA On suppresses another
wake only if HA's change timestamp is newer than that off request. All-off
itself schedules the cancellable release delay; intent becomes Off only at expiry.

## Direct console observations

Configure a Sony console role with
`probe = { kind = "ps5", host = "console.lan", port = 9302 }`.
**Remote Play must be enabled; the console must be reachable from the Choregos
host on UDP 9302 (same subnet, or a routed/NAT path).** There is no pairing or
account credential. The probe is a plain unicast datagram from the container.

Wake transitions start an Awake window; dispatched CEC stop sequences start a
Standby window. `[verify].probe_poll_ms` defaults to 2000 (500–10000 ms),
`probe_window_s` to 45 (5–300 seconds). Polling stops at target or deadline;
opposite targets replace the window, and reload/shutdown cancel in-flight work.
Role cards show the last direct reading and its age. `unreachable` means no
usable discovery reply, not proof of standby: check Remote Play and the UDP
path before drawing a power-state conclusion. These observations never send
corrective actions or overwrite CEC-derived logical role power.



## Reading the decision ring

Read the host-side state file (use `sudo` for its mode-0600 permissions):

```sh
sudo python3 -c 'import json,sys; rows=json.load(open(sys.argv[1]))["decisions"]; [print(d["timestamp"], d["role"], d["from"], d["to"], d["reason"], d["actions"], sep="\t") for d in rows[-30:]]' ./choregos/choregos.state.json
```

The **Decisions** page presents the same ring. Null role/from/to values are
normal for timer or service events. In shadow mode, actions describe intent,
not executed effects (`would` marks suppression).

| Reason | Meaning |
| --- | --- |
| `takeover_settled` | The settle delay expired; select the receiver source if needed and apply the preset when applicable. Not a standby confirmation. |
| `taken_over` | Another role displaced this one; its logical state is already Off, independently of physical standby confirmation. |
| `off_timer_expired` | The last-role-off delay expired with no unblocked role On; request TV and receiver off. |
| `presence_timeout` | An On role with polling enabled missed the configured number of polls; mark it Off and its bus power Unknown. |
| `standby_retry` | No observed Standby/Transition report yet; repeat the original stop/query sequence within the retry limit. |
| `standby_unconfirmed` | Verification exhausted without Standby/Transition evidence; this does not establish that the device remained awake. |
| `standby_target_absent` | Verification exhausted, at least one frame was sent, none ACKed, and correlated target NACKs cover every sent frame. |
| `identity_lost` | An On role lost its identity binding; it becomes Off and may start the room-off timer. Investigate identity/address traffic. |
| `service_started` | Runtime startup marker appended after the restored ring. |
| `resync_complete` | A buffered role power transition was applied when startup/bridge resync warming finished. |

For silent sleepers or devices that leave the CEC bus in standby,
`standby_unconfirmed`/`standby_target_absent` can be structural, not a room-state
failure: `taken_over` already set the role Off. Per-role `verify_standby = false`
sends the initial stop/query sequence but omits verification, retries and
`standby_*` decisions. For CEC takeover policies this requires removing
`power_on` from that role's `on` list, or a query reply could undo the takeover.
Keep verification for devices that need retries. `presence_timeout` is the
normal silent-disappearance path only for roles configured with polling.

## Incident replay

Copy the day's trace before retention removes it. Traces can contain private
names and entities even though HA attributes are filtered: keep raw copies
private. For a window spanning midnight, copy both files and pass them in
chronological order. Replay is offline and never executes device actions.
The `choregos/decision` trace topic preserves each creation row, including id, parent, trigger, structured actions and `would`; extraction keeps these rows, while replay ignores them and assigns fresh per-run ids to recomputed decisions.

```sh
docker cp choregos:/config/cec-bus.2026-11-01.jsonl ./incident-private.jsonl
choregos replay ./incident-private.jsonl --config ./choregos/choregos.toml \
  --since 2026-11-01T18:00:00Z --until 2026-11-01T18:05:00Z
```

The most recent identity header before the window seeds replay; it is not a
full checkpoint of prior role power, timers or HA state. Include enough lead-in
to reproduce the incident, and use the configuration that was in effect then.

Cut a fixture with the extraction command:

```text
choregos extract <trace…> --since <RFC3339> --until <RFC3339> [--rename OLD=NEW]… [--node NEW] -o out.jsonl
```

Windowed replay and extraction retain an identity seed and, when available,
the last bridge availability line before `--since`, with its original timestamp.

Supply explicit renames for private names, HA entities and source labels, and
use `--node cec-bridge` for a generic native node. Inspect the output, including
ASCII encoded in CEC `47` OSD-name frames, and scrub the accompanying config
before publishing; extraction is not a guarantee that all private data is gone.
Add a directory `tests/golden/<case>/` with `trace.jsonl` and the matching,
public `config.toml`. Generate `expected.jsonl`, then review the decisions:

```sh
CARGO_TARGET_DIR="$HOME/.cache/choregos/target-golden" UPDATE_GOLDEN=1 cargo test --locked --test golden
```

Commit only scrubbed fixtures/configs and reviewed expectations. Later normal
golden test runs compare decisions against those expectations.

## Mode changes and config edits

`select.choregos_mode` and the dashboard switch persist `mode` in the TOML.
Shadow mode still sends read-only identification/resync queries and presence
polls: it is not a network-isolated diagnostic mode.

A successful UI save restarts both MQTT and HA adapters. Retained bridge online
availability then triggers a resync burst. Bridge settings are live-editable;
`web`, `mqtt`, `home_assistant`, `tv.wake` and `tv.off` changes require stopping,
editing the mounted file and restarting. Validate startup configuration with
`choregos check-config --config ./choregos/choregos.toml` (required secret
environment variables must be present; output is redacted).

**Never start a second instance against the live broker with the same MQTT
client ID.** Session eviction can cause reconnect/resync storms roughly every
second and can wedge a fragile CEC sink. Use `check-config` for startup errors
or offline replay for incidents. `CHOREGOS_MQTT_CLIENT_ID` is not an implemented
override: a separate diagnostic setup must change `[mqtt].client_id` in its own
config. A different ID alone does not isolate fixed command/state topics or
prevent CEC sends; use an isolated broker for a second running instance.

## Upgrade and rollback

Pin a release image tag in compose rather than `latest` or `edge`. Back up
config and state before upgrading; read release notes for configuration breaks.
For example, the v0.2.0 migration requires `[bridge] kind = "tasmota"` or
`kind = "aether"`; there is no implicit adapter default.

```sh
docker compose pull choregos
docker compose up -d choregos
curl --fail --silent --show-error http://127.0.0.1:8095/healthz
```

Check all health fields and the latest ring rows (`service_started`, resync and
subsequent device transitions), not just Docker's healthy label. To roll back,
restore the previous image tag and run `pull` plus `up -d` again. When rolling
back across the state-v2 migration, restore the matching `choregos.state.json`
backup while stopped: the old image treats v2 as unknown and otherwise starts
empty, losing identities and history. If configuration changed incompatibly,
restore its matching backup too. Keep UID/GID 65532 ownership on restored files.
Recheck health and decisions after rollback.

## Bridge swap / rename

Renaming an aether node or Tasmota topic requires the matching TOML change
(`[bridge].node` or `.topic`) **and an adapter restart**. Saving through the UI
performs that restart; an external edit requires a service restart. Switching
firmware also requires the correct `kind`. Avoid two bridges claiming the same
CEC logical address during a swap. Check availability on the new name and the
ensuing resync before diagnosing role matching.

Expect cached bus identities to show `restored` until confirmed by physical
address or matching cached vendor plus OSD. Standby devices may not answer:
roles can remain unmatched until each device is seen waking once. A bridge
rename alone is not a reason to delete the learned state.
