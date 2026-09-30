# Desktop ChatGPT Userscript-Parity Rust CLI Migration Spec

Status: authoritative migration spec  
Last updated: 2026-09-30  
Target repository: `bhrumom/fabushi-plugin-chatgpt-auto-confirm-rs`  
Target baseline commit: `7e9f2f1625b3ac9dd79ce0f91771e27a4d01c26b`  
Source repository: `bhrumom/fabushi-chatgpt-auto-confirm-userscript`  
Source baseline commit: `587a03d6e607851ee8ced5d3ad13f65242fea07c`  
Source release at baseline: `2.10.15`  
Primary production surface: ChatGPT desktop application  
Reference real-device acceptance host: `htch-runtime`  
Build/test policy: htch-runtime may build/test during development; GitHub Actions remains the merge/release authority

## 0. Precedence

This document is the product and migration authority for the requested desktop ChatGPT Rust CLI.

It supersedes the browser/CDP product direction in `docs/specs/rust-linux-chatgpt-auto-confirm.md` and any browser-specific production-topology text in `docs/architecture.md` when those documents conflict with this spec. The existing Hexagonal Architecture dependency direction remains valid and should be preserved, but browser-only types, ports, actors, ownership rules and acceptance gates must be reconciled before implementing desktop parity.

The old Chromium/CDP implementation is migration scaffolding only. It must not remain the default shipping runtime once desktop parity is declared complete.

The pinned userscript source commit, its latest applicable specs and its regression tests are the behavioral oracle. Historical README sections describe evolution and can contain behavior later superseded by newer releases; when they conflict, the pinned source code plus the newest applicable spec/test at the pinned commit wins.

## 1. Objective

Migrate the behavior of `fabushi-chatgpt-auto-confirm-userscript` release 2.10.15 into a Rust CLI that operates the **desktop ChatGPT application**, not the web page.

The result must preserve the userscript's task semantics, safety boundaries, recovery rules, continuous Work -> Review orchestration, attachment handling, exact conversation-scoped authorization behavior, model/reasoning verification, abnormal-session carry, multi-task fairness and terminal-completion guarantees while replacing browser-specific implementation mechanisms with desktop-native mechanisms.

The migration is semantic parity, not a JavaScript-to-Rust line translation and not a DOM-to-accessibility selector transcription.

A desktop-specific difference is acceptable only when:

1. the userscript behavior cannot exist on the desktop surface in the same form;
2. the replacement preserves the same user-visible intent and safety invariant;
3. the difference is documented in the migration ledger;
4. deterministic contract tests cover it; and
5. real-device acceptance proves the production path where that behavior is observable.

## 2. Non-goals and hard safety boundaries

- Do not control a standalone Chromium/Chrome window as the production implementation.
- Do not require CDP as the primary desktop automation path.
- Do not embed browser URL, DOM selector, Electron renderer, AT-SPI role, process ID, screen coordinate or accessibility label knowledge into `domain` or `application`.
- Do not use fixed screen coordinates, image matching or OCR as the normal production path. Those may be diagnostics only and never the sole evidence for a destructive action.
- Do not read, export, log, synthesize or transmit ChatGPT passwords, cookies, OTPs, API tokens, payment details or other secrets.
- Login remains user-owned inside the desktop ChatGPT application.
- Automatic authorization is restricted to the exact current-conversation/current-session grant. Never choose persistent/global authorization such as "Always allow".
- A disabled authorization control is never authorization-success evidence.
- Stop-button disappearance alone is never successful completion evidence.
- Sidebar titles alone are never sufficient conversation identity.
- An old Copy action elsewhere in the app is never current-run terminal evidence.
- A mock, fixture or device-controller click is never evidence that the Rust CLI itself performed the equivalent production action.
- Do not claim full source parity while a required migration-ledger row is `pending`, `partial` or `blocked`.

## 3. Architecture decision

Use a **recoverable modular monolith** with:

- Hexagonal Architecture / Ports and Adapters;
- deterministic domain/application state machines;
- SQLite WAL durable state and append-only event journal;
- transactional outbox/effect intents for recoverable UI effects;
- actor-style runtime supervision;
- a single-writer desktop UI ownership model;
- semantic desktop observations rather than implementation-specific UI trees in policy code.

This is preferable to microservices for a local desktop automation runtime because process ownership, desktop session ownership, ChatGPT window focus, file pickers, recovery state and idempotency need one strongly coordinated local authority. Crate boundaries provide compile-time isolation; actors provide runtime isolation.

The dependency direction remains:

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

1. `domain` depends on no internal crate and no Tokio, SQLite, D-Bus, AT-SPI, Electron, filesystem, process or UI automation API.
2. `application` depends only on `domain` plus interface-support libraries. It owns policy and use cases.
3. adapters implement application ports and point inward.
4. `runtime` is the composition root and actor supervisor.
5. `cli` depends on `runtime` only among internal crates.
6. selectors, accessibility roles/labels, process/window details and desktop-specific timing quirks remain inside adapters.
7. recovery decisions never live in the desktop adapter.
8. the desktop adapter reports semantic facts and executes requested effects; it does not decide what the task should do next.

## 4. Target crate layout

Target production layout:

```
crates/
  domain/
  application/
  adapters/
    chatgpt-desktop-atspi/
    chatgpt-desktop-process/
    sqlite-store/
    attachment-store/
  runtime/
  cli/
```

Optional adapters may be added behind the same ports when a stronger stable interface becomes available, for example a verified native ChatGPT desktop automation/IPC endpoint. Such an adapter must not change domain/application semantics.

During migration, `chatgpt-cdp` and `linux-browser` may temporarily remain for regression comparison, but they must be feature-gated or removed from the default production composition before Gate G.

### 4.1 `crates/domain`

Pure value types and invariants:

- `TaskId`, `RunId`, `DispatchId`, `Phase`, `Round`, `GoalRevision`;
- `ConversationRef` as an opaque value, never assumed to be a URL;
- `ConversationFingerprint`;
- `AssistantResponseBoundary`;
- `ExecutionProfile` and five-position reasoning preset;
- task/run states;
- terminal evidence;
- approval fingerprints and settlement identity;
- recovery reasons;
- attachment metadata and immutable attachment identity;
- versioned `RecoveryEnvelope`;
- durable event types;
- completion and ownership invariants.

### 4.2 `crates/application`

Use cases and policy:

- enqueue/run/pause/resume/cancel/delete/edit task;
- one-shot and continuous-target orchestration;
- Work -> Review transition;
- strict `MAHAYANA_TASK_REPORT_V1` parsing and current task/round binding;
- prompt dispatch confirmation and deduplication;
- exact authorization policy;
- terminal classification;
- recovery matrix;
- fresh-conversation handoff construction;
- attachment readiness policy;
- multi-task fairness;
- execution-profile verification policy;
- effect idempotency and settlement rules.

Application ports include:

- `ChatSurfacePort`;
- `ChatProcessPort`;
- `TaskStore` / `RunStore`;
- `EventJournal`;
- `EffectOutbox`;
- `AttachmentStore`;
- `Clock`;
- `EvidenceSink`.

### 4.3 `chatgpt-desktop-atspi`

Primary Linux production UI adapter.

It owns:

- AT-SPI2/D-Bus accessibility discovery;
- ChatGPT semantic window/surface discovery;
- current conversation projection into semantic snapshots;
- composer interaction;
- response-local action discovery;
- exact current-conversation authorization interaction;
- model/reasoning controls;
- native attachment/file-picker interaction where deterministic;
- reload/new-conversation/reopen effects exposed by the desktop app;
- bounded popup dismissal;
- adapter-level accessibility fixtures.

It must prefer semantic roles, accessible names, stable relationships and app-provided automation identifiers. It must fail closed when a destructive action cannot be unambiguously bound to the current semantic surface.

### 4.4 `chatgpt-desktop-process`

Owns desktop process/session lifecycle only:

- discover installed ChatGPT application;
- launch/attach without stealing ownership from another active runtime;
- verify process health;
- observe application restart/crash;
- reattach after restart;
- expose app build/version information for evidence;
- graceful shutdown only when the runtime itself owns the process and shutdown is explicitly requested.

It does not inspect conversation content.

### 4.5 `sqlite-store`

SQLite WAL durable source of truth.

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

State snapshot + event + next effect intent must be committed atomically before the effect is exposed for execution.

### 4.6 `attachment-store`

Stores user-selected file bodies or stable local references outside SQLite and stores only metadata/hash/reference in SQLite. It must not silently drop an attachment and send text-only work.

### 4.7 `runtime`

Composition root plus actor supervision.

Target topology:

```
Supervisor
  |
  +-- DesktopSessionActor   # sole writer to ChatGPT desktop UI
  |      |
  |      +-- visible conversation lease
  |      +-- process/window attachment
  |
  +-- RunWorker(task A)
  +-- RunWorker(task B)
  +-- RunWorker(task C)
```

The `DesktopSessionActor` serializes all mutating desktop actions. Server-side ChatGPT work may continue in multiple conversations, but the local UI is a single shared resource and only one actor may mutate it at a time.

Each `RunWorker` owns exactly one run revision and communicates desired semantic effects to the desktop-session actor. No two workers may submit, approve, change the execution profile, attach files or navigate the desktop UI concurrently.

### 4.8 `cli`

Thin operator/API surface. It parses arguments, calls runtime entrypoints, prints machine-readable results and returns meaningful exit codes. It does not contain UI selectors, accessibility text, SQLite SQL or recovery policy.

## 5. Desktop semantic contract

The application must not consume a web-style `PageSnapshot`. Replace it with a platform-neutral `ChatSurfaceSnapshot`.

Minimum semantic facts:

- application health and session identity;
- current conversation `ConversationRef` when a strong reopen identity exists;
- `ConversationFingerprint`;
- composer presence/readiness/current draft fingerprint;
- current user-turn boundary and ownership confidence;
- current assistant-response boundary and ownership confidence;
- visible assistant prose fingerprint;
- visible assistant work/activity trace fingerprint;
- response streaming/busy state;
- Stop/Cancel-generation availability;
- authorization surface presence;
- authorization surface actionability;
- authorization settlement state;
- response-local Copy action evidence;
- strict review-report evidence;
- rate-limit notice;
- retryable network/send error;
- explicit conversation-load failure;
- connection-interrupted state;
- conversation-length limit;
- generic renderer/app-shell hydration state;
- execution-profile picker availability;
- verified selected reasoning preset;
- attachment upload/preview/readiness state;
- blocker/modal state;
- progress fingerprint.

The snapshot may contain sanitized bounded text needed for classification, but logs and evidence default to hashes, lengths and semantic flags rather than full user prompts or private conversation text.

## 6. Conversation and task identity

Web URLs are not a universal desktop identity mechanism.

The durable identity stack is:

1. `TaskId / RunId / Phase / Round / GoalRevision`;
2. a random Fabushi dispatch marker embedded in the authored task message;
3. current user-turn boundary;
4. current assistant-response boundary;
5. `ConversationFingerprint`;
6. optional opaque `ConversationRef` supplied by the desktop adapter when a deterministic reopen mechanism is available.

`ConversationRef` may internally be a deep link, app route, stable accessibility identity or another verified desktop-native reference, but application/domain code must not parse its platform representation.

A conversation is considered strongly owned only when the current semantic observations are consistent with the durable dispatch identity. A title match, recency, visible assistant text or a single sidebar item is insufficient.

If a desktop release does not expose a safe deterministic reopen identity, the runtime must fail closed for workflows that require reopening rather than fabricating one.

## 7. Transaction, outbox and idempotency rules

Desktop UI effects cannot be atomic with SQLite. Therefore:

1. calculate the next state and intended effect;
2. append a durable event;
3. update the materialized run/task state;
4. append an effect-outbox record with an idempotency key;
5. commit all of the above in one SQLite transaction;
6. execute the UI effect;
7. observe semantic postconditions;
8. settle the outbox item with the observed result.

Never assume exactly-once UI effects.

Required examples:

- prompt submission: dispatch fingerprint + observed new authored user boundary;
- authorization: approval fingerprint + 12-second settlement latch + semantic postcondition;
- execution-profile change: requested profile + independently observed selected profile;
- file attachment: attachment hash/ref + observed desktop preview/readiness;
- new conversation: handoff identity + observed new conversation fingerprint;
- reopen/reload: idempotent request + post-recovery conversation ownership check.

Crash recovery replays unsettled effects only after deciding from current observations whether the effect already happened.

## 8. Source-parity behavior contracts

### 8.1 Startup and exclusive ownership

- Only one runtime may own the desktop ChatGPT UI session for mutation.
- Runtime startup acquires a durable, expiring UI-session lease.
- A stale/crashed owner can be recovered using lease expiry plus process/session evidence.
- Restart restores unfinished tasks/runs from SQLite.
- Restart must never blindly repeat a Send, approval or file upload.
- User-owned app login/session state is reused; credentials are never copied into the Rust state store.

### 8.2 Task modes

#### One-shot

Dispatch exactly one task phase and finish only after strong terminal evidence.

#### Continuous target

Preserve the userscript loop:

1. Work conversation receives the current authoritative work prompt.
2. Work final result is captured as the current round result.
3. A separate Review/planning conversation is created.
4. Review is bound to current `taskId` and `round`.
5. Review must return strict `MAHAYANA_TASK_REPORT_V1`.
6. `status=complete` finishes the target.
7. `status=next` persists `next` verbatim as the next Work instruction and starts a new Work conversation.
8. Previous Work result, current `next`, original goal and relevant abnormal carry remain distinct fields; historical report text may never override current report identity.

### 8.3 Execution profile / model and reasoning

The requested profile is task state, not ambient UI state.

Five-position reasoning mapping at the source 2.10.15 baseline:

| Index | Semantic preset |
|---|---|
| 0 | Instant / none |
| 1 | Medium |
| 2 | High |
| 3 | Extra High / Max |
| 4 | Pro |

Before every actual dispatch:

1. inspect the desktop model/reasoning control;
2. select the requested preset;
3. independently re-observe the selected value;
4. only continue to attachment/send when verification succeeds.

Never silently inherit a desktop default.

If the picker/menu/slider equivalent is missing or loses its selected state, preserve the prepared dispatch intent and re-scan. After 60 seconds without a verifiable picker, recover the same desktop conversation/surface and repeat the same 60-second wait-and-recover cycle with no terminal retry cap. This dedicated recovery is independent from the generic renderer-recovery budget and must never duplicate Send.

Default profile for migrated tasks without an explicit preset is Extra High/Max unless a later source baseline explicitly changes that behavior.

### 8.4 Prompt dispatch and 90-second ambiguity rule

- Persist the dispatch intent before touching the UI.
- Prepare/clear the composer only when ownership and current draft safety are proven.
- Submit once.
- Do not mark dispatch confirmed until a new authored user turn is observed and bound to this dispatch marker.
- Preserve an unconfirmed attempt identity across restart.
- If a safe dispatch still cannot be confirmed after 90 seconds, perform the source-equivalent recovery: create a fresh conversation and resend the **original prepared intent** rather than hot-looping same-surface reloads.
- Never send two copies merely because the Send button disappeared or the desktop app changed focus.

### 8.5 Attachments

- Attachments are task-scoped and survive phase/recovery according to source semantics.
- File body/reference is stored outside the prompt text; prompt contains bounded metadata only.
- Use a deterministic desktop-native attachment action.
- Require observed filename/preview/upload readiness before Send.
- Initial readiness wait is 45 seconds.
- Failed/ambiguous upload becomes a recoverable task state; never submit the task without required attachments.
- Retries are bounded per attempt and backed off; no hot loop.
- A fresh-conversation recovery keeps attachment identity and reattaches as needed.

### 8.6 Authorization / auto-confirm

Authorization detection and action are separate concepts.

Required safety structure:

- recognize an authorization surface only from a strong structural/semantic signature equivalent to current "Reject + Allow/Allow once + split options";
- ordinary buttons containing "Allow" or "Allow once" are not enough;
- surface presence remains true even when controls are disabled;
- only an actionable current surface can be clicked;
- open the split options and select only the **current conversation/session** grant;
- never click the persistent/global grant;
- generic popup dismissal must never consume an authorization surface.

After selecting the conversation-scoped grant:

- persist an approval fingerprint;
- arm a 12-second settlement latch bound to task/run/phase/round/conversation fingerprint;
- during the latch, a disabled/remounted/momentarily absent card does not permit abnormal-end, retry, load-failure or fresh-conversation recovery;
- disabled state is not success;
- actual success/failure is decided from later semantic observations.

When Stop disappears at a potentially destructive handoff boundary:

- discard any stale cached authorization result;
- perform a live current-surface authorization scan;
- if that scan sees no authorization surface, wait at least 8 seconds and scan again;
- only after two stable no-authorization observations and all other safety guards pass may abnormal fresh-conversation recovery proceed.

Optional global auto-approval may scan conversations not created by the CLI, but it is still limited to exact current-conversation grants and uses the same settlement safety.

### 8.7 Completion

Successful completion requires evidence owned by the current run's latest assistant response.

At minimum:

- current run ownership is strong;
- response is not actively streaming/busy;
- Stop/generation control is absent;
- no authorization surface or active approval settlement exists;
- no blocking/rate-limit/error condition is taking precedence;
- the latest assistant response boundary owns a current response-local Copy action, or another explicitly source-approved strong terminal signal;
- terminal evidence is stable for the required source-equivalent interval and repeated observations.

Never use:

- Stop disappearance alone;
- any Copy action elsewhere in the window;
- an old assistant response;
- a title/sidebar change;
- static assistant text alone when current response ownership cannot be proven.

Normal terminal stability starts from the source-equivalent 4-second rule. Recovered/static fallback paths retain the stricter 8-second stability guard.

For Review/planning:

- Stop may disappear before final JSON/action UI hydrates;
- keep a bounded two-minute final-settlement window;
- a strict current `MAHAYANA_TASK_REPORT_V1` whose `taskId`, `round`, `status`, `summary` and `next` validate may qualify as strong review evidence before Copy hydration only when the same response boundary and all ownership/safety guards hold;
- after two minutes without valid final evidence or progress, normal abnormal recovery may resume.

### 8.8 Visible work trace and abnormal carry

Visible assistant activity/progress steps are progress evidence, not terminal evidence.

For a fresh-conversation abnormal handoff, capture a bounded ordered carry containing:

1. the authoritative current Work/Review instruction;
2. materially relevant visible assistant prose and desktop-exposed activity/work steps from the interrupted response;
3. the previous completed Work result when applicable;
4. current `next`;
5. original goal;
6. phase/round/task identity;
7. completed work, remaining work and blockers inferable from current durable context.

The handoff explicitly instructs the new conversation to continue rather than redo completed work.

Carry is versioned, bounded and tied to the current run identity. It must not accumulate unbounded transcripts across repeated failures.

### 8.9 Recovery matrix

| Condition | Required desktop behavior |
|---|---|
| Missing/unverifiable reasoning picker | Preserve send intent; after 60s recover same surface; repeat indefinitely at 60s intervals; never Send unverified |
| Generic no visible progress | After 15m, recover/reload the same owned conversation; visible prose/activity changes reset the timer |
| Generic renderer/app-shell hydration | 30s initial window, bounded generic recovery budget of 2 for the same identity; do not create an infinite refresh loop |
| Explicit "unable to load conversation" state | Recover the same conversation every 30s, max 7 attempts; after attempt 7 still failing, fresh conversation with carry |
| Connection interrupted | Immediate fresh-conversation handoff with current visible work carry; preserve task/phase/round/goal/next/attachments |
| Stream recovery polling timed out | Fresh-conversation handoff with carry |
| Stream cache expired with current Retry action | Retry the current failed response once per failure identity; never mark it complete |
| Rate limit / too many requests | Dismiss the explicit notice when safe, wait 5m; first 3 independent cooldown episodes stay with task; 4th episode fresh-conversation recovery preserving task context |
| Conversation length limit | Wait/capture the current bounded assistant work, then fresh conversation; carry max 64,000 chars and preserve task context |
| Stop disappeared, no final yet | Live authorization re-scan + at least 8s stable second check before destructive handoff; Review uses its 2m settlement rule |
| Ambiguous/unconfirmed Send | 90s confirmation window; then safe fresh-conversation resend of original prepared intent, not repeated blind clicks |
| Required attachment not confirmed | Do not Send; retain task and retry attachment according to bounded backoff |
| Popup/modal | Close only explicit harmless Close/Later/Skip equivalents; never consume auth/payment/consent surfaces |
| User closed task/app intentionally | Do not silently resurrect a user-closed task surface unless an explicit runtime resume policy applies |

Every recovery effect is persisted before execution and has a post-effect ownership check.

### 8.10 Rate-limit text safety

Rate-limit detection must classify the desktop app's own current visible notice/alert semantics. Authored prompt text, assistant discussion, logs and task metadata containing the same phrase must not trigger rate-limit policy.

### 8.11 Multi-task fairness

- Multiple conversations may continue server-side while the desktop UI displays one at a time.
- Only one task may mutate the UI at a time.
- Tasks blocked on cooldown, navigation/reopen, attachment retry or authorization settlement expose a next-wake time.
- Scheduler continues servicing other runnable tasks instead of globally sleeping.
- A task switch never sends a new message merely to inspect status.
- Switching must preserve exact run/conversation ownership and must not bind another task's visible response.

### 8.12 Pause, resume, cancel, delete and edit goal

- Per-task pause affects only that task.
- Global pause is a separate explicit action.
- Resume restores durable state and decides from observation whether an in-flight effect already settled.
- Cancel stops future automation effects but does not claim to cancel already server-side generation unless the user/runtime explicitly sends a stop action.
- Active tasks cannot be deleted until safely paused/cancelled/settled.
- Editing a goal increments `GoalRevision`, preserves already-sent current work, invalidates stale future `next` when required and applies the new goal to subsequent orchestration.

### 8.13 Memory/resource policy

The source 2.10.15 direction treats browser memory pressure as diagnostic-only. The desktop Rust runtime likewise:

- may report process/runtime memory diagnostics;
- may compact its own bounded logs/caches safely;
- must not reload/restart ChatGPT or abandon a task solely because a memory threshold was crossed;
- must not pretend a renderer-specific heap metric is whole-app memory.

### 8.14 Popup and consent safety

Automatic generic popup handling is limited to harmless, unambiguous controls such as Close, Later or Skip. Any surface involving authorization, login, account choice, personal data, payment, security verification or consent must fail closed and stay outside generic dismissal.

## 9. Web-to-desktop translation contract

| Userscript mechanism | Desktop Rust equivalent |
|---|---|
| DOM query / data-testid / role selectors | Semantic `ChatSurfaceSnapshot` produced by desktop adapter |
| ChatGPT `/c/<id>` URL | Opaque `ConversationRef` + strong `ConversationFingerprint` |
| Tab/workspace Web Lock | Durable desktop UI-session lease |
| localStorage task state | SQLite WAL task/run materialized state |
| userscript event history | Append-only `run_events` |
| IndexedDB attachment body | `attachment-store` body/ref + SQLite metadata |
| page navigation/reload | Desktop-native reopen/recover effect behind `ChatSurfacePort` |
| pagehide/hot reinjection recovery | Runtime/process restart recovery from durable journal |
| DOM approval scan | Semantic authorization-surface projection |
| DOM Copy/Stop detection | Response-bound semantic desktop action/state evidence |
| single browser tab scheduler | Single-writer `DesktopSessionActor` scheduler |
| userscript workbench UI | CLI JSON/JSONL commands plus durable task store |
| browser-side host capability messages | Internal runtime ports/actors; no browser extension dependency |
| browser JS heap diagnostics | Rust/app process diagnostics only |

A translation is accepted only when it preserves the userscript invariant, not merely when it has a similar name.

## 10. CLI contract

The final CLI should expose stable structured commands similar to:

```bash
fabushi-chatgpt-auto-confirm doctor --json
fabushi-chatgpt-auto-confirm desktop status --json
fabushi-chatgpt-auto-confirm desktop start --json

fabushi-chatgpt-auto-confirm task enqueue \
  --goal "完成这个任务" \
  --mode once \
  --reasoning max \
  --json

fabushi-chatgpt-auto-confirm task enqueue \
  --goal "持续完成这个目标" \
  --mode continuous \
  --reasoning max \
  --attachment /path/to/file \
  --json

fabushi-chatgpt-auto-confirm task status <task-id> --json
fabushi-chatgpt-auto-confirm task watch <task-id> --jsonl
fabushi-chatgpt-auto-confirm task pause <task-id> --json
fabushi-chatgpt-auto-confirm task resume <task-id> --json
fabushi-chatgpt-auto-confirm task cancel <task-id> --json
fabushi-chatgpt-auto-confirm task delete <task-id> --json

fabushi-chatgpt-auto-confirm approve-once --json
fabushi-chatgpt-auto-confirm supervise
```

A foreground `task run` convenience command may enqueue + watch one task, but it must use the same durable runtime path rather than a second simplified implementation.

Exit codes must distinguish at least:

- success;
- user/action required;
- task failed;
- timeout;
- desktop app unavailable;
- unsafe/ambiguous UI;
- storage/runtime corruption.

## 11. Testing and verification policy

**Development builds and tests may run directly on `htch-runtime` to shorten the desktop-integration feedback loop. GitHub Actions remains mandatory for merge/release qualification. Do not treat an `htch-runtime` result as a substitute for exact-HEAD CI evidence.**

Required pyramid:

### Layer 1 — domain unit/property tests

Examples:

- Stop disappeared is not completion;
- stale/foreign Copy is not terminal;
- approval disabled != success;
- 12s settlement latch blocks destructive recovery;
- 8s no-approval double-check;
- Review 2m settlement semantics;
- reasoning preset mapping;
- recovery counters and timer precedence;
- conversation fingerprint ownership;
- recovery-envelope versioning/bounds;
- idempotency keys.

### Layer 2 — application fake ports + virtual clock

Cover every recovery matrix row, Work -> Review orchestration, task fairness, pause/resume/cancel, attachment safety and crash/outbox replay.

### Layer 3 — desktop adapter contract fixtures

Use sanitized accessibility-tree fixtures/mock AT-SPI service for:

- composer;
- model/reasoning control;
- latest user/assistant boundaries;
- response-local Copy;
- Stop;
- approval surface including disabled/remounted/gap states;
- rate-limit modal;
- explicit load failure;
- connection interruption;
- conversation-length limit;
- retryable error;
- file-picker/attachment readiness.

### Layer 4 — packaged desktop integration fixture

GitHub Actions packages the actual CLI and runs it against a deterministic fake desktop surface through the same production port protocol. This proves wiring and process supervision, but not real ChatGPT behavior.

During development, the same integration tests may also be built and run directly on `htch-runtime`; those runs are diagnostic/development evidence only.

### Layer 5 — real desktop acceptance on `htch-runtime`

For release qualification, runs an exact-HEAD GitHub Actions artifact against the installed ChatGPT desktop application. During development, locally built-on-device Rust binaries may also be exercised on `htch-runtime`, but they do not satisfy the release acceptance gate.

A lower layer never substitutes for a higher gate.

## 12. `htch-runtime` real-device protocol

Discovery baseline observed on 2026-09-30:

- device: `htch-runtime`;
- platform: Linux x86_64;
- ChatGPT desktop application is installed with stable device-controller app id `desktop:chatgpt`;
- desktop launcher: `/usr/bin/chatgpt`;
- the launcher executes the installed ChatGPT Electron application;
- no target Rust CLI binary was present in PATH at discovery time.

These facts are reference discovery evidence, not a frozen product contract.

Real acceptance procedure:

1. During development, `htch-runtime` may fetch/checkout the intended commit and run Rust build/test/format/clippy/architecture commands directly for rapid iteration.
2. Device-side development results must record the exact commit SHA and commands used, and must never be reported as release qualification.
3. For release qualification, GitHub Actions builds/tests/packages the exact candidate commit.
4. Record workflow run id, artifact id/name and artifact SHA-256.
5. Download that exact artifact to `htch-runtime` and use it for the formal real-device acceptance run.
6. Ensure ChatGPT desktop uses a user-owned authenticated session. Never inject credentials.
7. Run `doctor` and desktop attachment checks.
8. Dispatch a canary task containing a unique Fabushi marker and a deterministic reasoning preset.
9. Verify from the CLI event stream that preset verification preceded attachment/send and that Send settled to the correct authored user boundary.
10. Use device accessibility inspection as an **independent read-only oracle** to corroborate app state and evidence.
11. Let the Rust CLI, not the device-controller, perform the ChatGPT UI mutation under test.
12. Verify a real final response is bound to the current response boundary and the CLI reaches terminal state only after valid evidence.
13. When a safe real authorization scenario is available, verify current-conversation grant handling and settlement. If no safe authorization card is available, Gate F remains unpassed rather than being replaced by a fixture claim.
14. Persist a sanitized acceptance report and hashes.

During product acceptance, MCP/device `computer_*` actions must not click Send, choose reasoning, choose approval, attach files or perform another product action on behalf of the Rust CLI. They may inspect/read state, start the application when necessary, or assist an explicit user-owned login flow. This separation prevents false-positive E2E evidence.

## 13. Source migration ledger

The ledger must be updated as implementation proceeds. `partial` means useful Rust code exists but source-equivalent desktop behavior is not fully proven.

| Source 2.10.15 responsibility | Desktop Rust destination | Baseline status |
|---|---|---|
| Task/run/round/goal state | domain + sqlite-store | partial |
| One-shot task mode | application | partial browser-era implementation only |
| Continuous Work -> Review loop | application | partial: deterministic phase/round transition exists; durable runtime wiring pending |
| Strict MAHAYANA_TASK_REPORT_V1 parser | application/domain | partial: exact taskId/round/status/summary/next validation has deterministic tests; production Review settlement wiring pending |
| Userscript durable workbench state | sqlite-store | partial: WAL schema and atomic state/event/outbox transaction exist with rollback/idempotency tests; application/runtime wiring and full task CRUD pending |
| Tab/workspace ownership | DesktopSessionActor + UI lease | pending |
| Multi-task fair supervision | runtime Supervisor/RunWorker | pending |
| Prompt marker and ownership boundary | domain + ChatSurfacePort | partial: opaque dispatch/conversation/user/assistant boundary types exist; desktop projection and durable confirmation pending |
| 90s send confirmation and safe resend | application | partial: semantic runner performs fresh-conversation recovery before re-dispatch; durable prepared-intent/outbox settlement pending |
| Five-position reasoning preset | domain + desktop adapter | partial: exact 0..4 semantic model/default Extra High is tested; desktop picker enforcement pending |
| 60s unlimited missing-picker recovery | application | pending |
| Attachment persistence/upload/readiness | attachment-store + desktop adapter | pending |
| Authorization structural detection | desktop adapter semantic projection | pending |
| Exact current-conversation approval | application + desktop adapter | partial browser-era logic only |
| Disabled/remounted approval handling | application/domain | partial: presence/actionability are distinct domain facts; desktop structural projection/remount evidence pending |
| 12s approval settlement latch | application/domain + store | partial: keyed 12s policy has deterministic test; durable store and production wiring pending |
| 8s live no-approval recheck before handoff | application | partial: deterministic two-observation window exists; live desktop scan wiring pending |
| Final latest-response Copy evidence | domain + desktop adapter | partial: strong response ownership + response-local Copy + 4s/8s stability modeled; desktop projection pending |
| Review two-minute final settlement | application | pending |
| Virtualized/missing task marker recovery | conversation fingerprint + response boundary | pending |
| Visible assistant activity progress fingerprint | desktop adapter + application | pending |
| Abnormal visible-work carry | RecoveryEnvelope | pending |
| Generic 15m stall recovery | application | partial browser-era implementation only |
| Generic hydration bounded recovery | application + desktop process/adapter | pending |
| Explicit load failure 30s x7 | application | partial: deterministic 30s x7 recovery state machine exists; durable attempt persistence/effect wiring pending |
| Connection-interrupted fresh handoff | application | partial: immediate fresh-handoff decision is tested; RecoveryEnvelope/runtime wiring pending |
| Stream polling-timeout handoff | application | partial: fresh-handoff decision modeled; runtime carry wiring pending |
| Stream-cache-expired current retry | application | partial: once-per-failure-identity policy modeled; adapter retry action and durable identity pending |
| Rate-limit 5m / 3 episodes / 4th fresh | application | partial: exact episode/cooldown decision policy is tested; fair scheduler/desktop notice provenance pending |
| Conversation-length handoff / 64k carry | application/domain | pending |
| Popup dismissal | desktop adapter + safety policy | pending |
| Pause/resume/cancel/delete/edit goal | application + store + cli | pending |
| Hot update/restart continuity | runtime + durable store/outbox | pending |
| Memory diagnostic-only policy | runtime/observability | pending |
| Web URL identity | redesigned as ConversationRef/fingerprint | partial: opaque ConversationRef/fingerprint and semantic boundaries replace URL types in domain/application; production desktop binding pending |
| localStorage/IndexedDB | redesigned as SQLite + attachment store | partial: SQLite WAL schema covers required durable tables and atomic effect outbox; attachment-store/runtime migration pending |
| Browser/CDP host capability glue | not applicable; replace with runtime ports | partial: core port is now ChatSurfacePort and architecture gate rejects BrowserPort/PageSnapshot in domain/application; default runtime is still legacy CDP |
| Real desktop ChatGPT acceptance | htch-runtime exact-HEAD artifact gate | pending |

No row may be marked `implemented` from type scaffolding alone. It requires shipping production wiring plus the highest applicable evidence layer.

## 14. Implementation order

### Phase 0 — reconcile authority

1. Land this spec.
2. Update `docs/architecture.md` and ADRs to the desktop topology.
3. Update architecture gate to reject new browser-specific leakage.
4. Freeze the userscript source baseline and create a machine-readable parity manifest.

### Phase 1 — domain/application extraction

1. Replace `PageSnapshot` / URL assumptions with semantic desktop-neutral types.
2. Model Task/Run/Phase/Round/Dispatch/ConversationFingerprint.
3. Port recovery policy and current source timer precedence.
4. Port continuous Work -> Review orchestration.
5. Port approval and completion invariants.
6. Add versioned RecoveryEnvelope.
7. Add fake-port/virtual-clock parity tests.

### Phase 2 — durable runtime

1. SQLite WAL store and schema migrations.
2. Append-only event journal.
3. Transactional effect outbox.
4. UI-session lease.
5. Supervisor/DesktopSessionActor/RunWorker.
6. crash/startup recovery and graceful shutdown.

### Phase 3 — desktop adapters

1. ChatGPT process discovery/attach/start.
2. AT-SPI semantic snapshot.
3. composer/send.
4. model/reasoning selection.
5. response-bound terminal evidence.
6. approval flow.
7. attachments.
8. desktop-native recover/reopen/new-conversation actions.

### Phase 4 — CLI

1. doctor/desktop commands;
2. durable task CRUD/control;
3. enqueue/run/watch JSON/JSONL;
4. supervise daemon path;
5. stable exit/error contract.

### Phase 5 — development + CI verification

1. optionally run architecture/fmt/test/clippy/integration on `htch-runtime` during development;
2. run architecture gate in GitHub Actions;
3. run fmt check in GitHub Actions;
4. run domain/application tests in GitHub Actions;
5. run adapter contract fixtures in GitHub Actions;
6. run clippy in GitHub Actions;
7. produce packaged release artifact in GitHub Actions;
8. run fixture integration in GitHub Actions.

### Phase 6 — `htch-runtime` acceptance

1. exact-HEAD artifact only;
2. simple dispatch;
3. reasoning verification;
4. terminal completion;
5. restart/recovery;
6. attachment journey;
7. real approval journey when safely triggerable;
8. evidence bundle.

### Phase 7 — parity closure

1. migration ledger contains no required pending/partial rows;
2. old browser runtime is removed or non-default legacy-only;
3. exact-HEAD GitHub Actions all green;
4. real-device gates pass;
5. final spec-compliance review proves every acceptance criterion.

## 15. Acceptance gates

### Gate A — architecture authority

- this spec is authoritative;
- `docs/architecture.md` and ADRs no longer prescribe browser-process/target ownership for the production desktop path;
- architecture script rejects dependency-direction and desktop-implementation leakage.

### Gate B — GitHub Actions code quality

Required on exact candidate commit:

- architecture gate;
- `cargo fmt --all -- --check`;
- `cargo test --workspace`;
- `cargo clippy --workspace --all-targets -- -D warnings`;
- release/package build.

These commands must pass in GitHub Actions on the exact candidate commit. They may also be run on `htch-runtime` during development.

### Gate C — source parity contract

- machine-readable migration manifest is complete for source 2.10.15;
- every required userscript behavior has deterministic domain/application/adapter coverage;
- no historical superseded source rule is accidentally reintroduced.

### Gate D — packaged desktop wiring

- packaged CLI uses the desktop production composition root;
- browser/CDP adapter is not selected by default;
- fixture integration proves CLI -> runtime -> application -> desktop adapter wiring.

### Gate E — real `htch-runtime` core journey

Exact-HEAD artifact proves:

- app discovery/attach;
- deterministic reasoning verification before Send;
- one canary prompt dispatched once;
- correct authored turn bound;
- response supervised to valid terminal evidence;
- CLI structured report matches independent read-only device evidence.

### Gate F — real authorization journey

A safe real ChatGPT authorization card proves:

- surface detected before destructive recovery;
- only current-conversation/session grant selected;
- settlement gap does not cause fresh handoff;
- no persistent authorization selected.

Fixtures do not satisfy this gate.

### Gate G — full userscript behavioral replacement

May be claimed only when:

- migration ledger has no required `pending`, `partial` or `blocked` rows;
- Gates A-F pass on the same release lineage;
- exact-head artifact/evidence provenance is recorded;
- browser runtime is not the default product path;
- final spec-compliance review records each requirement as passed or justified not-applicable.

## 16. Evidence contract

Every real acceptance record binds:

- target repository and exact commit SHA;
- source baseline SHA/version;
- GitHub Actions workflow/run/job identifiers;
- artifact name/id;
- artifact SHA-256;
- CLI version/build provenance;
- device id;
- OS/kernel/architecture;
- ChatGPT desktop executable/package version;
- task/run/dispatch IDs;
- command-line invocation with sensitive values redacted;
- semantic event timeline;
- requested and verified execution profile;
- attachment hashes/metadata when applicable, not private file content;
- final ownership/terminal-evidence summary;
- approval evidence when applicable;
- sanitized independent accessibility observation;
- result status.

Evidence must never include passwords, OTPs, cookies, tokens, full private prompts by default or unrelated conversation text.

## 17. Definition of done

This migration is done only when a user can install/run the Rust CLI, keep their normal authenticated desktop ChatGPT app, dispatch the same classes of tasks the userscript handled, and receive the same safety/recovery guarantees without relying on a browser userscript or standalone Chromium automation.

Specifically:

- the Rust CLI is the actor causing production desktop ChatGPT actions;
- desktop UI implementation details are isolated in adapters;
- task/recovery policy is deterministic and durable;
- crashes/restarts do not create duplicate destructive effects;
- execution profile is verified before Send;
- attachments cannot be silently omitted;
- authorization is conversation-scoped and fail-closed;
- Stop disappearance is never mistaken for successful completion;
- continuous Work -> Review can run across multiple conversations;
- abnormal handoff preserves useful work context without transcript explosion;
- multiple tasks are fairly supervised with a single UI writer;
- development builds/tests may run on `htch-runtime`, while merge/release qualification still requires exact-HEAD GitHub Actions;
- formal `htch-runtime` release acceptance uses the exact-HEAD packaged GitHub Actions artifact;
- full parity is not claimed until the migration ledger and Gates A-G are closed.

## 18. Baseline implementation status

At target baseline `7e9f2f1625b3ac9dd79ce0f91771e27a4d01c26b`, the repository contains a useful Hexagonal Architecture skeleton and browser/CDP automation, including some recovery and terminal concepts.

That implementation is **not** desktop ChatGPT userscript parity:

- production runtime is still Chromium/CDP;
- domain/application still expose browser/page/URL concepts;
- no desktop AT-SPI adapter exists;
- no SQLite durable event/outbox runtime exists;
- continuous Work -> Review parity is incomplete;
- source 2.10.13-2.10.15 authorization and reasoning-picker races are not ported;
- desktop attachment/model/recovery behavior is not proven;
- no exact-HEAD real desktop acceptance artifact has passed on `htch-runtime`.

Current PR #2 migration progress on 2026-09-30:

- Phase 0 architecture authority is reconciled to the desktop topology in `docs/architecture.md`.
- ADR-0003 is superseded for the production path and ADR-0004 records the single-writer `DesktopSessionActor` topology.
- `scripts/check-architecture.sh` rejects superseded `PageSnapshot`, `BrowserPort`, canonical web-conversation URL and browser/CDP leakage from domain/application.
- `docs/parity/userscript-2.10.15.json` freezes the source baseline and records machine-readable partial/pending responsibilities.
- domain/application now use desktop-neutral `ChatSurfaceSnapshot`, opaque conversation/turn boundaries, five-position reasoning semantics, authorization presence/actionability/settlement identity and deterministic recovery policy.
- deterministic application tests cover the currently extracted 12-second authorization latch, 8-second no-approval confirmation, explicit load-failure 30-second x7 policy, immediate connection-interruption handoff, fourth rate-limit-episode handoff and strict Review identity binding.
- these rows remain `partial`, not `implemented`, because SQLite/outbox persistence, actor/runtime production wiring, desktop AT-SPI/process adapters and real-device evidence are still absent.
- the production CLI/runtime remains legacy CDP at this point and therefore Gate D/G are not satisfied.
- `htch-runtime` became unavailable during this implementation pass, so no device-side build or desktop acceptance result is claimed for the current head. GitHub Actions remains the available exact-head verification authority until that device is online again.

Future implementation work must continue updating this section and the migration ledger from production wiring and evidence rather than changing status by assertion.
