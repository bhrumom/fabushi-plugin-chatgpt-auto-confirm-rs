# Canonical Architecture

Status: authoritative  
Last reconciled: 2026-09-30

## 1. Decision

Use a recoverable modular monolith with Hexagonal Architecture, deterministic domain/application state machines, SQLite WAL durable state, a transactional effect outbox, and actor-style supervision.

The production target is the ChatGPT desktop application. Browser/CDP code may remain temporarily as migration scaffolding and regression comparison, but it is not the target production topology.

## 2. Dependency direction

Allowed internal dependency direction:

```
cli
 |
 v
runtime ------------------------------+
 |                                     |
 v                                     v
application <---------------------- adapters
 |
 v
domain
```

Rules:

1. `domain` depends on no internal crate and no Tokio, SQLite, D-Bus, AT-SPI, Electron, filesystem, process, HTTP or browser automation API.
2. `application` depends only on `domain` plus interface-support libraries. It owns use cases and recovery policy.
3. adapters point inward and implement application ports.
4. `runtime` is the composition root and actor supervisor.
5. `cli` depends on `runtime` only among internal crates.
6. DOM selectors, URLs, AT-SPI roles/labels, process IDs, Electron details, screen coordinates, SQLite schemas and OS APIs must not leak into `domain` or `application`.
7. recovery decisions live in application policy, never in adapters.
8. desktop adapters report semantic facts and execute requested effects; they do not decide what the task should do next.

## 3. Target crates

Production target:

```
crates/
  domain/
  application/
  adapters/
    chatgpt-desktop-atspi/       # Linux
    chatgpt-desktop-macos/       # macOS
    chatgpt-desktop-process/
    sqlite-store/
    attachment-store/
  runtime/
  cli/
```

Legacy migration-only adapters may coexist while parity is incomplete:

- `crates/adapters/chatgpt-cdp`
- `crates/adapters/linux-browser`

They must not become the default shipping composition once desktop parity is declared complete.

## 4. Domain

`crates/domain` owns pure semantic types and invariants:

- `TaskId`, `RunId`, `DispatchId`, `Phase`, `Round`, `GoalRevision`;
- opaque `ConversationRef` and `ConversationFingerprint`;
- `UserTurnBoundary`, `AssistantResponseBoundary`, ownership confidence;
- `ChatSurfaceSnapshot`;
- five-position `ReasoningPreset`;
- approval fingerprints and settlement identity;
- attachment identity;
- versioned `RecoveryEnvelope`;
- task/run states and durable events;
- completion invariants.

The domain must not parse URLs or expose web/page abstractions. `PageSnapshot`, canonical conversation URLs and browser target IDs are forbidden final interfaces.

## 5. Application

`crates/application` owns policy and use cases:

- task enqueue/run/pause/resume/cancel/delete/edit-goal;
- one-shot and continuous Work -> Review orchestration;
- strict `MAHAYANA_TASK_REPORT_V1` parsing bound to current `taskId` + `round`;
- dispatch intent, send confirmation and deduplication;
- exact current-conversation authorization policy;
- 12-second authorization settlement latch;
- 8-second live no-approval confirmation before destructive handoff;
- ordinary/recovered terminal stability;
- 2-minute Review final settlement;
- reasoning preset verification/recovery;
- attachment readiness policy;
- recovery matrix;
- `RecoveryEnvelope` construction;
- multi-task fairness;
- effect idempotency and settlement rules.

Application ports include:

- `ChatSurfacePort`;
- `ChatProcessPort`;
- task/run stores;
- `EventJournal`;
- `EffectOutbox`;
- `AttachmentStore`;
- `Clock`;
- `EvidenceSink`.

## 6. Semantic desktop contract

Application consumes `ChatSurfaceSnapshot`, never raw accessibility or renderer data.

Minimum facts include:

- app health;
- composer readiness and draft fingerprint;
- user-turn and assistant-response boundaries;
- conversation ref/fingerprint and ownership confidence;
- visible assistant prose;
- visible working/activity trace;
- streaming/busy and Stop state;
- authorization presence, actionability and settlement state;
- response-local Copy evidence;
- strict Review report evidence;
- rate-limit, retryable error, unable-to-load, connection-interrupted, length-limit, stream-timeout/cache-expired;
- renderer/app-shell hydration;
- reasoning picker and verified selected preset;
- attachment readiness;
- blocker/modal;
- progress fingerprint.

Adapter implementation details remain private to the adapter.

## 7. Production runtime topology

```
Supervisor
  |
  +-- DesktopSessionActor   # sole desktop UI mutation owner
  |      |
  |      +-- visible conversation lease
  |      +-- process/window attachment
  |
  +-- RunWorker(task A)
  +-- RunWorker(task B)
  +-- RunWorker(task C)
```

`DesktopSessionActor` serializes all mutating ChatGPT desktop actions. Multiple server-side conversations may continue concurrently, but the local desktop UI has exactly one mutation owner.

`RunWorker` never directly accesses AT-SPI, windows, processes or native dialogs. It emits semantic desired effects and waits for observed settlement.

## 8. Durable state

SQLite WAL is the durable source of truth.

Minimum tables:

- `tasks`;
- `runs`;
- `dispatch_attempts`;
- `run_events`;
- `effect_outbox`;
- `approval_fingerprints`;
- `ui_session_leases`;
- `attachments`;
- `acceptance_evidence`.

For each destructive UI effect:

1. calculate next state;
2. append durable event;
3. update materialized state;
4. append outbox effect with idempotency key;
5. commit the same SQLite transaction;
6. execute the UI effect;
7. re-observe semantic postcondition;
8. settle the effect.

UI effects are at-least-once with idempotency and postcondition observation. Exactly-once must never be assumed.

## 9. Conversation identity

A web route is not domain identity.

Durable identity is composed from:

1. `TaskId / RunId / Phase / Round / GoalRevision`;
2. Fabushi dispatch marker;
3. current user-turn boundary;
4. current assistant-response boundary;
5. `ConversationFingerprint`;
6. optional opaque `ConversationRef` supplied by the desktop adapter.

Sidebar title, recency, URL shape or visible assistant text alone is insufficient ownership evidence.

## 10. Safety and completion

Automatic authorization is limited to exact current-conversation/current-session scope. Persistent/global grants are forbidden.

Authorization surface presence and actionability are distinct. Disabled/remounted controls still count as presence and are never success evidence.

Stop disappearance is never completion evidence.

Ordinary completion requires current-run ownership, no streaming/busy state, no Stop, no authorization, no active settlement, no higher-priority blocker/error state, response-local Copy evidence and stable current response evidence.

Normal terminal stability is about 4 seconds; recovered/static fallback is about 8 seconds. Review may complete from a valid current-bound strict report before Copy hydrates, within the dedicated Review settlement policy.

## 11. Recovery ownership

Application owns timer and precedence rules, including:

- 90-second dispatch confirmation followed by safe fresh-conversation recovery and re-dispatch of the prepared intent;
- 60-second unlimited missing reasoning-picker same-surface recovery cycle;
- 45-second initial attachment wait plus bounded retry;
- 15-minute generic no-progress recovery;
- bounded generic renderer hydration recovery;
- explicit unable-to-load recovery every 30 seconds up to seven times then fresh handoff;
- immediate fresh handoff on connection interruption;
- fresh handoff on stream polling timeout;
- once-per-failure retry on stream cache expiry;
- 5-minute rate-limit cooldown, preserving the first three episodes and using fresh recovery on the fourth;
- fresh handoff for conversation length limit with bounded 64k carry.

Memory measurements are diagnostic only and never trigger reload/restart/fresh-chat/task abandonment.

## 12. Desktop adapters

### chatgpt-desktop-atspi

Owns Linux AT-SPI2/D-Bus discovery and mapping between the actual ChatGPT desktop accessibility surface and semantic facts/effects.

### chatgpt-desktop-macos

Owns macOS ChatGPT.app process discovery/launch and native AXUIElement access. The initial adapter supports accessibility-tree discovery, exact composer identification, reasoning-option selection, safe new-chat start, and a unique Send action. Response/authorization evidence and recovery remain unsupported and must fail closed until mapped and accepted. macOS GitHub Actions builds do not establish desktop behavior parity.

It may know roles, labels, hierarchy, Electron accessibility quirks and native dialog details. Those facts never cross the port boundary.

### chatgpt-desktop-process

Owns app discovery/launch/attach/health/restart observation and build/version evidence. It does not inspect conversation content.

### sqlite-store

Owns SQL schema, transactions, WAL configuration, optimistic revisions, event journal, outbox and UI-session leases.

### attachment-store

Owns user-selected file bodies or stable local references and attachment hashes/metadata. It must never silently omit a required attachment and send text-only work.

## 13. CLI

CLI is thin. It parses arguments, invokes runtime entrypoints, emits JSON/JSONL and stable exit codes.

The target surface includes:

- `doctor`;
- task enqueue/list/show/edit/delete;
- pause/resume/cancel;
- run/supervise/watch;
- evidence/report export.

CLI must not contain selectors, accessibility labels, process/window logic, SQL or recovery policy.

## 14. Testing and release gates

Development builds/tests may run on `htch-runtime`.

Exact candidate HEAD must pass GitHub Actions:

- architecture gate;
- `cargo fmt --all -- --check`;
- `cargo test --workspace`;
- `cargo clippy --workspace --all-targets -- -D warnings`;
- package/release build;
- deterministic integration fixtures.

Formal real-device release acceptance must download the exact-HEAD Actions artifact to `htch-runtime`. The Rust CLI itself must perform Send, reasoning selection, approval, attachment, retry and recovery actions. Device-control is only an independent oracle and may not substitute for product behavior.

## 15. Migration rule

No migration ledger row may be marked implemented because a struct, trait, stub or mock exists. A row requires production implementation, production wiring, deterministic tests and the highest applicable acceptance evidence.

The legacy CDP/browser path is migration scaffolding only until it is removed or feature-gated away from the production default.
