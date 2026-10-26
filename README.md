# choregos

Choregos is a Rust home-theater conductor. It learns the HDMI-CEC bus through a
Tasmota CEC bridge and coordinates the TV, receiver, media sources and Sonos
through Home Assistant. Use each source's own remote; Choregos handles the room.

## How it works

Received CEC frames update a bus model and resolve configured roles using OSD
name, vendor ID and/or physical address. A source waking triggers TV wake,
release of other sources, receiver power/source selection and a time-based HA
preset. When the last source goes off, a cancellable delay powers down the TV
and receiver. Polling covers sources that disappear in standby. Identity
conflicts inhibit power actions for the affected roles rather than guessing.

The engine has no network I/O or clock reads. Injected time drives startup
warming, takeover settling, identification retries and presence/off timers.
MQTT, HA websocket and wake adapters execute its actions separately. The web UI
provides the dashboard, bus stream, devices, decisions, config and manual actions.

## Configuration

Start from [`choregos.example.toml`](choregos.example.toml). Replace the example
hosts, entities, sources and matching rules with your own. Secrets are accepted
**only through environment variables**, never as TOML keys:

- `CHOREGOS_MQTT_PASSWORD`
- `CHOREGOS_HA_TOKEN` — Home Assistant long-lived access token
- `CHOREGOS_SESSION_SECRET` — at least 32 bytes of random session signing material
- `CHOREGOS_OIDC_CLIENT_SECRET`
- `CHOREGOS_LOCAL_PASSWORD` — break-glass password; hashed by the web layer
- The environment variable named by `tv.wake.token_env`, for HTTP wake only

| Section | Settings |
| --- | --- |
| root | `mode`: `shadow` (recommended initially) or `active` |
| `mqtt` | `host`, `port`, `username`, `client_id` |
| `bridge` | Tasmota `topic` and claimed `logical_address` (0–14) |
| `home_assistant` | HTTP(S) base `url`; connects at `/api/websocket` |
| `tv` | HA `entity`; `wake` is `{kind="wol", mac, broadcast, port}` or `{kind="http", url, header, token_env}`. HTTP wake sends POST and uses the environment value as the complete header value. |
| `receiver` | HA `entity`, `select_source` boolean |
| `presets` | IANA `timezone` and `[[presets.slot]]` entries with `from`, `to` (`HH:MM`), and HA `script`. Must cover exactly 24 hours; use `24:00` only as an end. |
| `sonos` | HA `entity`, `mute_switch`, `volume_from` list of role IDs. Volume keys are forwarded only while Sonos reports `playing`. |
| `timing` | `off_delay_s`, `takeover_settle_ms`, `standby_retries` (default `2`, range `0`–`10`), `presence_poll_s`, `presence_misses`, `identify_retry_s`. Unconfirmed standby is re-sent after each settle interval, followed by a power-status query; `0` disables retries. Poll/retry intervals and presence misses must be positive. |
| `fake_tv` | `replies=true` (default; false when bridge firmware answers directed TV requests), `power_status="always_on"`, `cec_version="1.4"`, `osd_name` (1–14 ASCII bytes). Broadcast Request Active Source remains service-owned. |
| `device` | Array of roles: unique `role`, `name`, `kind` (`playback`/`recording`), `match`, unique receiver `source`, `on`/`off` event lists, `takeover`, `presence`, optional HA `media_player` |
| `web` | `bind`, externally visible `public_url`; `web.oidc` has `issuer`, `client_id`; `web.local_login` has `username` |

Every provided `match` key must match: `osd` (trimmed exact name), `vendor` (six
hex digits), `physical` (four dotted hex digits, e.g. `2.1.0.0`). At least one is
required. A role matching multiple logical addresses, or multiple roles matching
one address, is a conflict. Learned identities and the decision log survive restart;
live power and active-source state are rebuilt from bus replies.
Role IDs must match `[a-z0-9_]+` and may not be `bridge`, `mode`, `conflicts`,
`active_source` or `all_off` (reserved MQTT discovery/state names).


Power events: `image_view_on`, `text_view_on`, `active_source`, `power_on`,
`standby`, `inactive_source`, `power_standby`, `report_physical_address`.
Takeover policies: `cec_standby`, `ha_media_stop`, `ha_turn_off` (both HA policies
require `media_player`), `none`. Use `ha_turn_off` for boxes that ignore CEC
Standby while playing; it calls `media_player.turn_off` without CEC standby
verification. Presence policies: `events` or `poll` (playback devices only).

Active-mode standby verification retries remain bounded even if a device is
silent or NACKs. With patched bridge firmware, exhaustion records
`standby_target_absent` when all standby and power-status frames were NACKed and
none ACKed (expected for devices that leave the bus in standby), otherwise
`standby_unconfirmed`. Extra/late NACKs never override an observed ACK. Stock
firmware reports only queue/transmit status, not target ACKs, so unconfirmed
standby always records `standby_unconfirmed`. Only an observed
Standby/Transition report confirms standby; a real role On event cancels
verification. Config reload clears pending checks. Raw CEC sends accept 1–14
data bytes, matching the bridge firmware limit.

```sh
cargo run -- check-config --config choregos.toml
cargo run -- run --config choregos.toml
```

The config command resolves environment inputs but prints only redacted public
settings. The live editor shows the current file verbatim, including comments
and ordering; saves validate before atomic replacement and reload. Secrets come
only from environment variables, never from TOML.
Mode changes replace only the mode value, preserving comments/order and unrelated
manual edits. `web`, `mqtt`, `home_assistant`, and `tv.wake` changes require a
restart: the editor rejects these changes explicitly to prevent authenticated
users redirecting credentials to a different host or environment-secret name.
Edit those sections in the mounted config while stopped.

### Persisted state

The runtime keeps `choregos.state.json` beside the config file (in Docker,
`/config/choregos.state.json`). This versioned JSON file contains only learned
CEC identities (logical/physical address, vendor, OSD name, device type and last
`84` confirmation time) and the newest 2000 decisions, including `service_started`
restart boundaries. Empty entries and the bridge are not saved; identities
without an `84` confirmation are memory-only, and confirmations older than 30
days are dropped on load. No frames, credentials, live power or active-source
state are saved. Restored identities remain unverified and do not match roles
until `84` confirms the cached PA. Resync queries PA immediately; intervening
power signals stay buffered. Unverified devices show `restored`, not a last-seen
time. Missing-key retries back off to one hour and pause while the bridge is offline.

Updates are coalesced to the newest state and written atomically by one background
writer, not the CEC event loop; shutdown flushes the last state. Missing, corrupt
or unsupported-version files start empty. Write failures warn once, retry after
60 seconds or on new state, and log recovery without stopping the service.
It is safe to delete the file while stopped to discard
learned identities and decision history; the next start re-interrogates the bus.
Replay neither reads nor writes this state file.

### MQTT exposure

Bridge inputs are `tele/<bridge.topic>/SENSOR`, `tele/<bridge.topic>/LWT` and
`stat/<bridge.topic>/RESULT`; sends use `cmnd/<bridge.topic>/HdmiSend`.
Retained HA discovery creates role power binary sensors, active-source and
conflict sensors, mode select, all-off button and bridge binary sensor. Node
availability is `choregos/status`, with an `offline` last will. Retained entity
states use `choregos/state/<name>`. Commands use `choregos/cmd/mode` (`shadow` or
`active`) and `choregos/cmd/all_off` (button payload). One instance per broker
namespace is supported.

## Running with Docker

Images are published to `ghcr.io/yasen-pavlov/choregos` on `v*` tags for amd64 and
arm64, with `:edge` updated on every push to `main`. Pin a release tag for
production; `latest` follows tagged releases only.

```sh
mkdir -p config
cp choregos.example.toml config/choregos.toml
# Edit config/choregos.toml and export the environment variables listed above.
# Give the container's uid write permission for config saves and persisted state.
sudo chown -R 65532:65532 config
docker run --rm --name choregos -p 8095:8095 \
  --cap-drop ALL --security-opt no-new-privileges --read-only \
  -v "$PWD/config:/config" \
  -e CHOREGOS_MQTT_PASSWORD -e CHOREGOS_HA_TOKEN \
  -e CHOREGOS_SESSION_SECRET -e CHOREGOS_OIDC_CLIENT_SECRET \
  -e CHOREGOS_LOCAL_PASSWORD \
  ghcr.io/yasen-pavlov/choregos:latest
```

Mount the **directory**, not the single config file: atomic rename cannot replace
a bind-mounted file. The static musl image runs as uid/gid 65532 without a shell
or curl. Its built-in health check invokes:

```sh
choregos healthz --url http://127.0.0.1:8095/healthz
```

`/healthz` is unauthenticated and reports MQTT/HA connections, bridge availability
and mode. The container health check verifies the web process responds with a
valid health object; it does not restart a healthy process solely because an
external system is offline. Put the UI behind HTTPS. A standalone compose example
is in [`deploy/docker-compose.yml`](deploy/docker-compose.yml); merge it into
your own stack as needed. Direct WoL from a bridge network works when the
configured broadcast address is a directed broadcast (e.g. `192.168.1.255`)
that the router forwards to the TV's network. Otherwise use `network_mode: host`
(instead of `ports:`) or the `http` wake variant with a LAN wake service.

## Shadow mode

Start in `shadow`. The complete engine still runs and publishes its learned
states and decisions, but the executor permits only read-only identification,
startup/resync interrogation and presence `8F` polls on the CEC bus. TV wake, HA service calls, raw CEC,
fake-TV replies and volume forwarding are suppressed and logged as `would:`
with their action payload. Mode/config persistence and HA discovery/state
publication remain operational. Inspect learned identities and decisions before
switching to `active` through the UI or MQTT. The switch is saved atomically.

## Development and replay

Rust stable, edition 2024. No JavaScript build step. See [DESIGN.md](DESIGN.md)
for behavior and [docs/decisions.md](docs/decisions.md) for implementation choices.

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo run -- replay fixtures/capture-streamer-wake.jsonl \
  --config choregos.example.toml
```

Replay reads timestamps and bus/LWT payloads, advances internal timers, and
prints JSON decisions with resulting actions. It never connects to MQTT or HA
and needs no secrets. Unrelated fixture topics are ignored. The acceptance test
seeds the pre-capture Kodi state and appends an explicit wake because the raw
capture starts with Kodi standby and ends with volume presses. Both Sonos
playing and paused branches are tested with injected HA events.

Unit and replay tests use injected time; MQTT tests run an in-process local
`rumqttd` broker, HA tests use a loopback fake websocket. Runtime acceptance tests
drive the production loop against both adapters, including shadow suppression,
mode/config persistence, retained discovery/state, broker restart and HA disconnect.
The broker's listener and connections are owned by a disposable test runtime,
so reconnect tests genuinely close sockets rather than inject connection flags.
Tests never need a real home-theater installation. CI runs formatting, clippy, tests and LLVM LCOV,
then uploads coverage to SonarCloud for same-repository PRs and main pushes.
Docker builds run on PRs without pushing. Releases use git-cliff notes; the
package version stays `0.0.0`, with release identity provided by tags.
