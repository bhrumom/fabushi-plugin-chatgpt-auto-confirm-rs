# Rust/Linux ChatGPT Auto-Confirm Rewrite Spec

Status: authoritative migration spec
Target repository: bhrumom/fabushi-plugin-chatgpt-auto-confirm-rs
Source repository: bhrumom/fabushi-plugin-chatgpt-auto-confirm
Source baseline commit: 1a626b496dccdc21240990ee7d3bbe44700a604f
Primary target OS: Linux x86_64/aarch64
Implementation language: Rust 2024 edition

## 1. Objective

Rebuild the ChatGPT automation/auto-confirm runtime in Rust so it can run natively on Linux without relying on macOS Accessibility (AXUIElement) or Swift.

The rewrite must automate an already authenticated ChatGPT browser session, send prompts, monitor the exact conversation, automatically choose Allow once / 允许一次 when explicitly present, recover from renderer/network stalls, and only declare a response complete when the latest assistant turn has stable terminal UI evidence.

This repository is not a line-by-line Swift translation. The source behavior is preserved behind portable domain contracts and Linux-capable browser adapters.

## 2. Non-goals and safety boundaries

- Do not steal, export, log, or synthesize ChatGPT credentials, cookies, passwords, OTPs, or API tokens.
- Login is user-owned. The runtime uses a dedicated persistent Chromium profile that the user authenticates interactively.
- Never click broad persistent approval such as Always allow. Automatic approval is restricted to Allow once / 允许一次.
- Never infer completion merely because the Stop button disappeared.
- Never claim a real ChatGPT acceptance gate passed from mocks or unit tests.

## 3. Architecture

The canonical architecture is defined in `docs/architecture.md` and the accepted ADRs under `docs/adr/`.

The design is a recoverable modular monolith using Hexagonal Architecture plus actor-style runtime supervision. The enforced dependency direction is:

`cli -> runtime -> application -> domain`

Concrete infrastructure implements application ports and points inward:

- `crates/adapters/chatgpt-cdp` -> application/domain
- `crates/adapters/linux-browser` -> OS/process only
- `crates/adapters/sqlite-store` -> application/domain

`runtime` is the composition root. Recovery policy belongs in `application`, terminal/canonical-URL invariants belong in `domain`, and browser selectors/CDP details belong only in adapters.

CI runs `scripts/check-architecture.sh` to reject dependency inversion and selector leakage.

Production topology is one authenticated browser process per account profile, one leased page target per active run, with Supervisor -> AccountBrowserActor -> RunWorker ownership. Durable recovery uses SQLite WAL, append-only run events, materialized run state, approval fingerprints, and worker leases.

## 4. Linux browser model

### 4.1 Chromium

The Rust runtime launches or attaches to Chromium/Chrome with a local-only CDP endpoint using a dedicated user-data directory and remote debugging port.

Supported discovery order:
1. explicit --browser-binary
2. /usr/bin/google-chrome
3. /usr/bin/google-chrome-stable
4. /usr/bin/chromium
5. /usr/bin/chromium-browser

The first interactive login is performed by the user in headed mode. Later automation reuses that profile.

### 4.2 CDP

The browser adapter obtains page targets from /json/list, chooses a chatgpt.com page, connects to webSocketDebuggerUrl, and uses Runtime.evaluate, Page.navigate, and Page.reload.

No screen coordinates are required for the primary Linux path.

## 5. Required behavior contracts

### 5.1 Prompt dispatch

1. Bind to the current ChatGPT page.
2. Record the current user-turn count.
3. Populate the current composer and click the enabled Send button.
4. Do not mark dispatch confirmed until the page shows a new user turn.
5. If the original send still cannot be confirmed after 90 seconds, resend the original prompt.
6. Continue monitoring the same durable conversation.

### 5.2 Completion

The following are not sufficient by themselves:
- Stop button disappeared.
- assistant text exists.
- old Copy button exists elsewhere in the virtualized transcript.

Terminal completion requires all of:
1. response is not actively streaming or waiting for approval;
2. the latest assistant turn is after and bound to the latest user turn;
3. the latest assistant turn owns a rendered response action row;
4. that row contains Copy/复制;
5. the same terminal evidence is observed stably in at least two consecutive polls.

This ports the source QueueTerminalDecision.swift rule that Stop disappeared is not completion evidence.

### 5.3 Automatic approval

- Detect exact Allow once, 允许一次, Approve once, 仅允许本次, or 允许本次.
- Click only that exact current-card action.
- Never click persistent/global authorization.
- Persisted fingerprint deduplication is implemented in the SQLite run journal with bounded attempts and durable settlement.

### 5.4 URL provenance

A conversation URL becomes durable only after ChatGPT exposes a stable canonical conversation route such as https://chatgpt.com/c/<conversation-id>.

Do not persist transient local or startup URLs as recovery URLs. The final report records the URL only from a terminal snapshot.

Canonical URL validation is implemented in `crates/domain`, and the SQLite materialized run state plus RecoveryEnvelope persist the canonical URL for crash recovery.

### 5.5 Recovery timers

Default policy:
- 90 seconds: dispatch not confirmed -> resend original prompt.
- 15 minutes with no observable progress -> reload the current conversation.
- Too many requests / 请求过于频繁 -> click Got it / 明白了 when present, pause 5 minutes, maximum 3 rate-limit pauses before failing the run.
- 30 minutes without terminal completion, when not actively streaming -> send 继续完成所有.
- Continue until stable Copy-button terminal evidence or the configured global run timeout.

Connection-interruption evidence is projected into snapshots and durable run progress. Crash/lease recovery reconstructs a versioned RecoveryEnvelope containing the original goal, acceptance prompt, live assistant progress, completed/remaining/blocker state, and canonical conversation URL.

### 5.6 Model and thinking effort

Full replacement requires deterministic model/thinking selection rather than inheriting whatever the page currently has selected.

Target contract:
- expose requested model and thinking effort in task configuration;
- inspect the actual ChatGPT picker DOM;
- set the requested model before dispatch;
- set requested thinking effort, including Extra High / 极高 when available;
- verify the selected UI value before sending;
- fail closed if the requested setting cannot be verified.

The CDP adapter now attempts model/thinking selection against the live UI, re-observes the selected values, and application dispatch fails closed unless they match the requested ExecutionProfile. The local Chromium fixture verifies the observation path; current production ChatGPT UI behavior is reserved for Gate D.

## 6. Source migration ledger

| Source responsibility | Rust destination | Initial state |
|---|---|---|
| Models.swift pure run/report state | crates/domain | implemented for queue/run/report/recovery state |
| QueueTerminalDecision.swift | crates/domain | implemented |
| IPCAndCDP.swift CDP portion | crates/adapters/chatgpt-cdp | implemented for Chromium CDP |
| macOS Unix IPC to ChatGPT.app | crates/adapters/linux-browser + CDP path | not applicable to browser runtime |
| ApprovalAccessibility.swift | crates/adapters/chatgpt-cdp | AX path intentionally removed on Linux |
| ApprovalWatcher.swift | application use case + CDP adapter | implemented for exact current-card approval + durable dedupe |
| QueueMonitoring.swift | crates/application recovery state machine | implemented for dispatch/reload/rate-limit/continuation/terminal policy |
| QueueWorker.swift hidden worker lifecycle | runtime Supervisor/AccountBrowserActor/RunWorker | implemented for leased target worker lifecycle and crash requeue |
| QueueState.swift durable queue | crates/adapters/sqlite-store | implemented for task/run journal, revisions, dependencies, resource locks, waiting, retries and worker leases |
| ChatScripts.swift | crates/adapters/chatgpt-cdp evaluated JS | implemented for Linux-required prompt/approval/notice/snapshot/profile behavior |
| TaskReportParsing.swift | domain/report parser | implemented with V1 report/wait validation |
| Node Actions controller scripts | GitHub CI + runtime queue CLI | redesigned: CI/fixture/evidence orchestration is native GitHub Actions; durable task orchestration is Rust CLI/runtime |
| account/session export scripts | secure user-owned Chromium profile boundary | redesigned: credential/cookie export intentionally removed; profile reuse and local CDP only |

The remaining replacement certification boundary is production evidence: lower-layer tests cannot substitute for Gate D.

## 7. CLI contract

Start headed Chromium:
cargo run -p fabushi-chatgpt-auto-confirm -- browser --headed true

Inspect current page:
cargo run -p fabushi-chatgpt-auto-confirm -- status

Send and monitor with fail-closed execution profile verification:
cargo run -p fabushi-chatgpt-auto-confirm -- send --prompt "完成这个任务" --model "GPT-5.6" --thinking "Extra High" --auto-confirm true

Enqueue a durable task:
cargo run -p fabushi-chatgpt-auto-confirm -- --db ./queue.sqlite3 queue-enqueue --task-id task-1 --prompt "完成这个任务"

Run one durable queue claim:
cargo run -p fabushi-chatgpt-auto-confirm -- --db ./queue.sqlite3 --cdp http://127.0.0.1:9222 queue-run-once --account-id default

Inspect durable state:
cargo run -p fabushi-chatgpt-auto-confirm -- --db ./queue.sqlite3 queue-status

One-shot approval:
cargo run -p fabushi-chatgpt-auto-confirm -- approve-once

## 8. Acceptance gates

### Gate A — Rust code quality

Required:
- ./scripts/check-architecture.sh
- cargo fmt --all -- --check
- cargo test --workspace
- cargo clippy --workspace --all-targets -- -D warnings

### Gate B — Linux binary

Build on Linux:
cargo build --release -p fabushi-chatgpt-auto-confirm

### Gate C — local CDP fixture

A Chromium fixture must prove:
- target discovery;
- send-button dispatch;
- Allow once exact-match only;
- stale old Copy button does not produce terminal;
- latest-turn Copy action produces terminal;
- rate-limit dialog dismissal;
- reload recovery.

### Gate D — real authenticated Linux ChatGPT

Human-owned login, no credential injection. Evidence must record:
- exact commit SHA;
- Linux distro and arch;
- Chromium version;
- CLI command;
- starting conversation URL;
- canonical final /c/... URL;
- prompt dispatch proof;
- stable terminal Copy proof;
- auto-confirm proof when an actual Allow once card appears.

### Gate E — full source replacement

May only be marked complete when the migration ledger has no pending or partial items required by source behavior, including:
- durable multi-task queue;
- per-task worker isolation;
- persisted approval deduplication;
- task-report protocol parser;
- recovery/fresh-chat handoff;
- account/profile lifecycle;
- deterministic model and thinking selection;
- CI plus real Linux acceptance evidence.

## 9. Evidence contract

No gate can be marked passed by editing Markdown alone.

A real acceptance record must bind:
- exact tested commit;
- test or workflow identifier;
- environment;
- command;
- machine-readable result artifact;
- SHA-256 of that artifact where stored externally.

Mocks prove code behavior only. They do not prove ChatGPT production behavior.

## 10. Current implementation status

Implemented on the migration branch after the initial Rust cut:
- Rust workspace and Linux CLI;
- canonical modular-monolith/hexagonal architecture with ADRs;
- compile-time crate boundaries plus CI architecture gate;
- application ports for browser and virtual clock;
- Chromium process launcher;
- CDP target discovery and WebSocket transport;
- composer prompt dispatch;
- 90-second dispatch-confirm resend policy;
- exact Allow once click path;
- terminal latest-turn Copy evidence;
- canonical `/c/<conversation-id>` validation in the domain layer;
- two-poll terminal stabilization;
- 15-minute stale reload;
- 5-minute rate-limit pause, max 3;
- 30-minute 继续完成所有 continuation;
- deterministic application tests for 90-second resend, 15-minute reload, 5-minute rate-limit handling and 30-minute continuation;
- source-aligned domain tests for terminal semantics;
- SQLite WAL durable queue, materialized run records, append-only events, revision checks and worker leases;
- durable approval fingerprint attempts/settlement;
- dependency/resource-lock scheduling, waiting state, retry and continuation limits;
- Supervisor -> AccountBrowserActor -> RunWorker target ownership with lease heartbeat and crash requeue;
- source-compatible task-report and wait-marker parsing;
- versioned RecoveryEnvelope preserving original goal, acceptance prompt, live progress, completed/remaining/blockers and canonical URL;
- fail-closed ExecutionProfile verification for model and thinking effort;
- Linux profile directory mode 0700;
- real headless Chromium fixture job in GitHub Actions;
- separate self-hosted authenticated Linux ChatGPT evidence workflow with commit/environment/artifact hash binding.

Not yet claimed complete:
- Gate D live authenticated ChatGPT Linux E2E on a user-owned logged-in Linux browser session.

The source account/session export path is intentionally not reproduced because the security contract forbids credential/cookie export. GitHub-hosted CI proves Gates A-C only. Full replacement certification remains blocked until Gate D is executed successfully on an authenticated self-hosted Linux runner and its evidence artifact is retained.
