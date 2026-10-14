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
- future `crates/adapters/sqlite-store` -> application/domain

`runtime` is the composition root. Recovery policy belongs in `application`, terminal/canonical-URL invariants belong in `domain`, and browser selectors/CDP details belong only in adapters.

CI runs `scripts/check-architecture.sh` to reject dependency inversion and selector leakage.

Production topology is one authenticated browser process per account profile, one leased page target per active run, with Supervisor -> AccountBrowserActor -> RunWorker ownership. Durable recovery will use SQLite WAL, append-only run events, materialized run state, approval fingerprints, and worker leases.

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
- Persisted fingerprint deduplication from the Swift queue is required before full replacement is declared.

### 5.4 URL provenance

A conversation URL becomes durable only after ChatGPT exposes a stable canonical conversation route such as https://chatgpt.com/c/<conversation-id>.

Do not persist transient local or startup URLs as recovery URLs. The final report records the URL only from a terminal snapshot.

Canonical URL validation is now implemented in `crates/domain`; the durable state store remains required for full source parity.

### 5.5 Recovery timers

Default policy:
- 90 seconds: dispatch not confirmed -> resend original prompt.
- 15 minutes with no observable progress -> reload the current conversation.
- Too many requests / 请求过于频繁 -> click Got it / 明白了 when present, pause 5 minutes, maximum 3 rate-limit pauses before failing the run.
- 30 minutes without terminal completion, when not actively streaming -> send 继续完成所有.
- Continue until stable Copy-button terminal evidence or the configured global run timeout.

Future parity work must also implement the source policy for repeated connection interruptions and fresh-chat handoff with recovery context.

### 5.6 Model and thinking effort

Full replacement requires deterministic model/thinking selection rather than inheriting whatever the page currently has selected.

Target contract:
- expose requested model and thinking effort in task configuration;
- inspect the actual ChatGPT picker DOM;
- set the requested model before dispatch;
- set requested thinking effort, including Extra High / 极高 when available;
- verify the selected UI value before sending;
- fail closed if the requested setting cannot be verified.

This is specified but not yet implemented in the initial Rust cut because it requires a live current ChatGPT DOM acceptance fixture.

## 6. Source migration ledger

| Source responsibility | Rust destination | Initial state |
|---|---|---|
| Models.swift pure run/report state | crates/domain | partial |
| QueueTerminalDecision.swift | crates/domain | implemented |
| IPCAndCDP.swift CDP portion | crates/adapters/chatgpt-cdp | implemented for Chromium CDP |
| macOS Unix IPC to ChatGPT.app | crates/adapters/linux-browser + CDP path | not applicable to browser runtime |
| ApprovalAccessibility.swift | crates/adapters/chatgpt-cdp | AX path intentionally removed on Linux |
| ApprovalWatcher.swift | application use case + CDP adapter | partial |
| QueueMonitoring.swift | crates/application recovery state machine | partial |
| QueueWorker.swift hidden worker lifecycle | runtime Supervisor/RunWorker | pending |
| QueueState.swift durable queue | future sqlite-store adapter | pending |
| ChatScripts.swift | crates/adapters/chatgpt-cdp evaluated JS | partial |
| TaskReportParsing.swift | domain/report parser | pending |
| Node Actions controller scripts | future Rust orchestration | pending |
| account/session export scripts | secure browser-profile boundary | redesign required |

Partial means the Linux automation path exists, not that source parity is complete.

## 7. CLI contract

Start headed Chromium:
cargo run -p fabushi-chatgpt-auto-confirm -- browser --headed true

Inspect current page:
cargo run -p fabushi-chatgpt-auto-confirm -- status

Send and monitor:
cargo run -p fabushi-chatgpt-auto-confirm -- send --prompt "完成这个任务" --auto-confirm true

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

## 10. Initial implementation status

Implemented in the first Rust cut:
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
- source-aligned domain tests for terminal semantics.

Not yet claimed complete:
- live authenticated ChatGPT Linux E2E;
- deterministic model/thinking picker;
- full durable queue parity;
- source account/session/Actions orchestration parity.

Until Gates D and E pass, this repository is a working Linux Rust automation implementation and migration base, not a certified complete replacement of the Swift/Node source.


## 11. macOS desktop extension

The default CLI backend now targets the installed macOS ChatGPT Electron client.
Linux Chromium remains available with explicit `--backend browser`.

Implemented locally:
- a separate `macos-desktop` adapter launches ChatGPT.app through LaunchServices;
- a separate user-owned profile and loopback CDP port, without credential export;
- exact main-renderer selection (`app://-/index.html`), excluding browser pages,
  avatar overlays and prewarmed/auxiliary app windows;
- inspect and new-chat operations inside the desktop renderer;
- Chat composer readiness checks, refusal to dispatch into Work or overwrite drafts;
- native desktop user/assistant selector fallback without double-counting turns;
- active message-region selection when the app renders multiple main panes;
- pointerdown-aware mode switching followed by the native Chat/Work composer toggle;
- latest-turn response-action scope and canonical desktop conversation identity;
- asynchronous composer update settlement before looking for Send.

Regression coverage includes desktop target exclusion and launcher argument handling,
plus the sanitized local DOM contract suite in `scripts/test-desktop-dom.cjs` for
duplicate desktop selectors, multiple panes, sibling response actions, stale Copy,
React input settlement, draft protection, scoped Send, exact visible Allow once,
and native ChatGPT/Chat mode switching.
The application terminal invariant, two-poll stability and exact Allow once scope remain
unchanged. Live desktop dispatch, reply and authorization evidence must be recorded
separately; these changes do not establish Linux production acceptance or full queue,
model-selection or source-replacement parity.


Local macOS smoke evidence (2026-10-12): the installed arm64 runtime sent
"请只回复 FABUSHI_DESKTOP_OK" to the desktop main renderer and returned
`state: complete`, `assistant_text: ChatGPT 说： FABUSHI_DESKTOP_OK`, with zero
retries, reloads or continuations. No live authorization card appeared, so exact
Allow once has fixture coverage only. This was a native local desktop conversation;
no remote canonical URL was exposed and the report correctly retained null.

## 11. Implemented desktop workbench extension

The current desktop contract is `desktop-tui-userscript-parity.md`, based on
userscript 2.10.47 at `4ff2f386dc40ab0e542250c0710f05c77bf1f612`.
The original Linux single-prompt behavior above remains a distinct backend.

Implemented: macOS app launcher and explicit main-renderer targeting; terminal
goal workbench; SQLite WAL queue/journal, revisions and endpoint leases;
verified model/effort and attachments; separate conversation rotation with
exact marker checks; durable dispatch intent; execution/independent review
rounds and report parsing; conversation-first scoped approvals; paced same-card
load retry; interruption/length-limit context handoff; pause/edit/cancel/delete,
queue import/export, scan/switch/recovery logs and live dashboard.

Validation covers 48 Rust tests and sanitized DOM contracts. Local desktop
observations demonstrated two complete goal dispatches, matching conversation
markers, both conversation switches and current-conversation authorization.
These observations do not prove completion of the mobile goals, exhaustive
userscript parity, or authenticated Linux acceptance. CI evaluates each PR HEAD.

Desktop recovery refinement: native role-less Work cards with countdown badges
are answered with Stay in chat. Nested load errors outside cached transcript
containers latch recovery on the same conversation even before dispatch
confirmation. Unconfirmed desktop intents are retained after 90 seconds instead
of automatically replayed. The past-hour operation journal filters polls before
limiting history, with successful card clicks and dispatch preflight recorded.

Desktop implementation status: embedded workbench entry and shared Queue command bridge implemented, with DOM and runtime regression coverage; full source replacement remains governed by the migration ledger.

Single-task resume implementation status: after saving a runnable task, resume also
enables global supervision and clears that task's pending stop request. Other paused
tasks remain paused; the original dispatch marker, timestamp and confirmation state
are preserved. Panel regression coverage includes global pause followed by individual
resume and repeated resume of an already-runnable task while supervision is stopped.

Desktop mode recovery implementation status: `chat-mode` restores and verifies the
ChatGPT mode over CDP without clicking New chat or sending input. It refuses
to hide native drafts or selected attachments. Already-dispatched supervision
restores ChatGPT mode before activating or observing the original task; dispatch
markers and timestamps remain unchanged. DOM fixtures cover delayed mode menus,
the Chat tab, idempotence and draft protection; application tests cover restoring
mode for unconfirmed sends without creating a conversation or repeating dispatch.

Lost-dispatch recovery implementation status: after Send, the first marker-proven
conversation location is saved before the next scan, retaining it across pause.
`recover-task ID --conversation ID` verifies the original marker and exact sidebar
identity before recording a pending binding; it never replaces the original prompt,
phase, round, token or dispatch timestamp. Sidebar positioning waits for delayed
history pagination at the bottom instead of wrapping before older rows mount.
Runtime, application and DOM regression tests cover these recovery contracts.

Mode/load-error recovery refinement: mode verification uses the native current-mode
control independently of composer readiness. A ChatGPT load-error page with no input
therefore reaches the same-conversation Retry path instead of accumulating mode
errors and backoff. New-chat and Send still require a ready composer. Repeated
interruption handoffs flatten prior carry checkpoints instead of nesting envelopes
or duplicating `work_result` and `goal`, and shared turn containers exclude user
prompt units from `visible_work`. The DOM and application regressions cover a
composer-less load-error card, its exact Retry, blocked Send and bounded handoff carry.

Interruption protection refinement: an active or waiting response and native drafts
retain their original session despite interruption labels. An inactive interruption
must remain stable for 60 seconds before handoff. Prior checkpoints and work results
each use an 8,000-character window; newest progress uses 24,000 characters, with
head/tail retention and the full material left in durable journals and source chats.
This keeps the 48,000-character envelope from cutting off the latest next action.
Regressions cover streaming/waiting/draft protection, the full grace interval and
oversized Unicode checkpoints with original goal and newest tail preserved.

Approval settlement refinement: a persisted click receipt no longer hides a current,
unresolved approval card when no terminal response exists. Already-completed and
historical cards remain suppressed. Current requests allow at most three scoped
grant attempts, spaced at least 60 seconds apart, with counts surviving restart;
exhaustion retains the original task and reports required manual verification.
Regressions cover unresolved receipts, completed-card suppression, retry spacing,
restart persistence and the three-attempt limit. Permanent grants remain excluded.

Virtualized-transcript recovery refinement: native surface identity is observed
separately from the permanent sidebar ID, ignoring hidden sidebar rows and hidden
selection metadata. An already-recorded matching native or permanent location
does not require searching a local temporary ID in the sidebar. When user turns
are unmounted, durable location proof supplies user-turn ownership for the current
assistant response; visible unmarked or foreign user turns cannot inherit it.
Permanent binding promotion and stable terminal checks still require complete
response actions, and independent review parsing still verifies exact task/round.
Regression coverage includes local-to-permanent promotion, virtualized review
completion, another native session, Copy-only evidence and visible unmarked users.

Embedded UI parity implementation status: userscript-style launcher/layout, task and workspace controls, scoped preferences, private attachment bridge/previews, shared two-hour feed and one-hour operations, safe cleanup, import/export and hydration retention implemented. Desktop-only substitutions and evidence are tracked in desktop-tui-userscript-parity.md.

Approval deduplication implementation status: persisted request receipts survive remount/restart/phase reset; request-targeted click rechecks the live card. Resolved and older-user-turn cards are excluded from pending observations and terminal guards. New request IDs remain actionable even when their text matches a resolved request. Missing identity uses bounded message/card metadata and a content digest, with the ambiguity documented in desktop-tui-userscript-parity.md. Native conversation identity changes do not invalidate a receipt.
