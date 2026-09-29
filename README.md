# fabushi-plugin-chatgpt-auto-confirm-rs

Rust/Linux rewrite of bhrumom/fabushi-plugin-chatgpt-auto-confirm.

The current implementation can attach to a Chromium or Chrome session through CDP, send a ChatGPT prompt, automatically click Allow once / 允许一次, monitor the latest turn, recover from stalls, and return only after stable Copy-button terminal evidence.

The authoritative migration contract is `docs/specs/rust-linux-chatgpt-auto-confirm.md`. The canonical architecture is `docs/architecture.md`; architecture decisions are recorded under `docs/adr/`.

## Architecture

This is a modular monolith with strict Hexagonal Architecture boundaries:

- `crates/domain` — pure invariants and durable value types;
- `crates/application` — use cases, recovery policy, Browser/Clock/RunJournal/QueueStore ports;
- `crates/adapters/chatgpt-cdp` — ChatGPT/CDP implementation;
- `crates/adapters/linux-browser` — Linux Chromium process implementation;
- `crates/adapters/sqlite-store` — SQLite WAL durable queue, event journal, approval fingerprints and worker leases;
- `crates/runtime` — composition root plus Supervisor/AccountBrowserActor/RunWorker ownership;
- `crates/cli` — thin operator surface.

Run the architecture gate with `./scripts/check-architecture.sh`.

## Build

cargo build --release -p fabushi-chatgpt-auto-confirm

## First login on Linux

Install Chromium or Google Chrome, then run:

./target/release/fabushi-chatgpt-auto-confirm browser --headed true

A dedicated browser profile is created under the local data directory. Log in to ChatGPT yourself in that browser.

## Run

./target/release/fabushi-chatgpt-auto-confirm status

./target/release/fabushi-chatgpt-auto-confirm send --prompt "继续完成所有" --auto-confirm true

Default recovery policy:
- unconfirmed send after 90s -> resend original prompt;
- no progress for 15m -> reload current conversation;
- rate limit -> dismiss notice, wait 5m, at most 3 times;
- no terminal answer after 30m -> send 继续完成所有;
- completion -> latest assistant turn has stable Copy action evidence.

## Security

This runtime does not request or export your ChatGPT password, OTP, cookies, or API tokens. Authentication remains in the user-owned Chromium profile.


## Durable queue

```bash
./target/release/fabushi-chatgpt-auto-confirm --db ./queue.sqlite3 queue-enqueue \
  --task-id task-1 \
  --prompt "完成这个任务" \
  --model "GPT-5.6" \
  --thinking "Extra High"

./target/release/fabushi-chatgpt-auto-confirm --db ./queue.sqlite3 queue-status

./target/release/fabushi-chatgpt-auto-confirm \
  --db ./queue.sqlite3 \
  --cdp http://127.0.0.1:9222 \
  queue-run-once --account-id default
```

The durable runtime uses append-only run events, materialized run state, worker leases with heartbeat, approval fingerprints, dependency/resource-lock scheduling, task-report validation and RecoveryEnvelope crash handoff.

## Acceptance

The normal `ci` workflow runs architecture, fmt, workspace tests, clippy, release build, and a real headless Chromium fixture. Production ChatGPT acceptance is deliberately separate in `authenticated-linux-e2e.yml` because it requires a user-owned authenticated Linux browser session. A fixture or mock is never reported as production ChatGPT evidence.
