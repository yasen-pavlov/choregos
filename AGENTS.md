# Working on choregos

Read `DESIGN.md` first; it is the contract. Record any deviation agreed in
review in `docs/decisions.md` (date, what, why).

## Invariants
- Rust 2024 edition, stable toolchain, `cargo fmt`, `cargo clippy --all-targets -- -D warnings` clean, no `unsafe`.
- The engine (`src/engine.rs`, `src/cec/*`, `src/identity.rs`) is pure: no I/O, no clocks, no globals. Time is an injected `now`.
- Shadow mode must never cause a side effect other than identification queries and the startup resync. Enforce it in the executor, not in callers.
- Secrets only from environment variables; never in the config file, logs, UI, or tests. Redact on every output path.
- No JavaScript build step: htmx + askama templates + one CSS file. Vendored htmx lives in `src/web/static/` with its version in the filename.
- Version in `Cargo.toml` stays `0.0.0`; releases are tags.
- Tests: behaviour, boundaries, transitions, error paths. No tests of wiring, copies, or defaults. Fixture replays live in `tests/replay.rs`.
- Public repo from day one: nothing lab-specific that is secret; lab-specific *values* (entity ids, MACs, topics) belong in the example config, not in code.

## Commands
```
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo run -- check-config --config choregos.example.toml
cargo run -- replay fixtures/capture-streamer-wake.jsonl --config choregos.example.toml
```
Use `CARGO_TARGET_DIR=~/.cache/choregos/target-<yourname>` so parallel workers do not fight over the lock.

## Workers
- Work only in the files your task names. Never commit, push, or rewrite git history; the lead commits (GPG-signed).
- Do not contact the real MQTT broker, Home Assistant or the CEC bridge from tests or ad-hoc runs; the lead runs the shadow-mode smoke against the lab.
- Finish with: files changed, decisions + why, tests added and their results, open risks, anything not verified.
