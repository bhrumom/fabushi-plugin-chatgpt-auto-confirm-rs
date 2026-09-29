# Canonical Architecture

Status: authoritative

## 1. Decision

Use a recoverable modular monolith with Hexagonal Architecture (Ports and Adapters), deterministic application state machines, and actor-style runtime supervision.

Do not use microservices by default. Browser automation needs strong local ownership of browser processes, profiles, page targets, task leases, and recovery state. Crate boundaries provide compile-time modularity; actors provide runtime isolation.

## 2. Dependency direction

Allowed dependency direction:

    cli
     |
     v
   runtime  -----------------------------+
     |                                   |
     v                                   v
 application <---------------------- adapters
     |                            /     |      \
     v                           /      |       \
   domain                 chatgpt-cdp linux  sqlite

Rules:

1. domain depends on no internal crate and no async/browser/OS/database runtime.
2. application depends only on domain and interface-support libraries.
3. adapters depend inward on application/domain and implement ports.
4. runtime is the composition root and chooses Tokio plus concrete adapters.
5. cli depends on runtime only among internal crates.
6. selectors/CDP scripts never appear in domain/application.
7. recovery decisions never live in browser adapters.

## 3. Crates

### crates/domain

Pure deterministic invariants and value types:
- PageSnapshot semantic facts;
- completion invariants;
- canonical conversation URL validation;
- TaskId, RunId, AccountId, ConversationId;
- task/run states;
- approval fingerprints;
- recovery reasons and durable event types.

Forbidden: Tokio, CDP, Chromium, filesystem, SQLite, HTTP and OS APIs.

### crates/application

Use cases and ports:
- RunPrompt;
- recovery policy;
- BrowserPort;
- Clock;
- RunJournal and QueueStore ports; future ProfileManager and EvidenceSink ports.

Application decides what effect should happen. It never knows selectors, PIDs, SQLite schemas or profile paths.

### crates/adapters/chatgpt-cdp

Owns:
- CDP target discovery and WebSocket transport;
- ChatGPT DOM -> PageSnapshot projection;
- prompt composer interaction;
- exact Allow once action;
- rate-limit notice dismissal;
- verified model/thinking selection with fail-closed observation;
- sanitized DOM fixture contract tests.

The adapter reports facts and executes requested effects; it does not choose recovery policy.

### crates/adapters/linux-browser

Owns:
- Chromium/Chrome discovery;
- localhost-only CDP configuration;
- process launch/restart/shutdown;
- profile permissions;
- future dynamic DevToolsActivePort discovery.

It never inspects ChatGPT DOM.

### crates/adapters/sqlite-store

Use SQLite WAL for durable state:
- tasks;
- runs;
- append-only run events;
- approval fingerprints;
- account/browser metadata;
- worker leases;
- acceptance evidence metadata.

SQLite is preferred to ad-hoc JSON because crash recovery needs transactions, revisions, indexes and atomic lease ownership.

### crates/runtime

Composition root and actor runtime:
- concrete adapter wiring;
- Tokio clock;
- Supervisor;
- AccountBrowserActor;
- RunWorker lifecycle;
- bounded concurrency;
- cancellation;
- startup recovery;
- graceful shutdown.

Runtime must not become a second business-policy layer.

### crates/cli

Thin operator surface. Parse arguments, call runtime entrypoints, print structured results and return meaningful exit codes. It must not import concrete adapters directly.

## 4. Production runtime topology

    Supervisor
      |
      +-- AccountBrowserActor(account A)
      |      |
      |      +-- TargetLease(run 1)
      |      +-- TargetLease(run 2)
      |
      +-- AccountBrowserActor(account B)
             |
             +-- TargetLease(run 3)

Use one authenticated browser process per account profile and one leased page target per active run.

Why:
- Chromium profiles are not safe for concurrent writers from independent processes.
- cloning a live profile per task is fragile and can create stale auth/session state.
- one process per account preserves authenticated state;
- target leases isolate conversations;
- on process crash, workers recover from durable state and canonical conversation URLs.

A worker must never navigate or close a target leased by another worker.

## 5. Actor ownership

### Supervisor

Owns global runtime topology:
- restore unfinished runs at startup;
- enforce max concurrency;
- route a run to an account browser;
- maintain worker leases;
- restart failed account browser actors;
- cancel/drain workers on shutdown.

It never manipulates DOM.

### AccountBrowserActor

Sole owner of one account browser process and profile:
- launch/attach;
- verify CDP health;
- create/find targets;
- lease one target to one RunWorker;
- restart process on fatal transport failure;
- rebind workers via canonical conversation URLs.

### RunWorker

Sole owner of one run:
- execute application use cases;
- persist meaningful transitions;
- operate only its target lease;
- obey cancellation;
- settle exactly once.

No two workers may concurrently own the same run or target.

## 6. Durable state model

The renderer is ephemeral. Conversation and run state are durable.

Target tables:

### tasks
- task_id
- account_id
- original_prompt
- current_revision
- status
- priority
- created_at
- updated_at

### runs
- run_id
- task_id
- state
- revision
- canonical_conversation_url
- target identity
- last_activity_fingerprint
- last_progress_at
- continuation_count
- dispatch_retry_count
- recovery_count
- rate_limit_pause_count
- started_at
- finished_at

### run_events

Append-only journal:
- sequence
- run_id
- event_type
- payload_json
- created_at

Examples:
- RunStarted
- PromptDispatchRequested
- PromptDispatchConfirmed
- ApprovalObserved
- ApprovalApplied
- RateLimitObserved
- RecoveryReloadRequested
- RecoveryReloadApplied
- ContinuationRequested
- CanonicalConversationBound
- TerminalEvidenceObserved
- RunCompleted
- RunFailed
- RunCancelled

### approval_fingerprints

Deduplicate authorization actions across renderer remounts and process restarts.

### worker_leases

Prevent duplicate workers after crash/restart using owner identity, revision and expiry.

## 7. Transaction and idempotency rule

For each durable transition:

1. calculate next state;
2. append event;
3. update run snapshot/revision;
4. commit both in one SQLite transaction;
5. then expose the new durable state.

Browser effects cannot be atomic with SQLite, so each effect needs idempotency plus post-effect observation.

Examples:
- prompt send: attempt fingerprint + user-turn confirmation;
- approval: approval fingerprint + post-click settlement;
- reload: safe repeatable effect;
- continuation: distinct continuation marker + user-turn confirmation.

Do not assume exactly-once browser effects. Design for at-least-once effects with deduplication and observable settlement.

## 8. Browser contract

Application sees semantic facts only.

BrowserPort evolves toward:
- observe;
- send_prompt;
- approve_once;
- dismiss_notice;
- reload;
- navigate;
- ensure_execution_profile;
- create_target;
- close_owned_target.

Selector policy:
1. stable data-testid or explicit role/aria contract;
2. structural relation inside the latest message/card;
3. localized exact labels only when semantic identifiers do not exist;
4. never broad historical-transcript text search for destructive actions.

Selectors are centralized and covered by sanitized DOM fixtures.

## 9. Completion invariant

A run is terminal only when:
- response is not in flight;
- latest assistant turn belongs to latest user turn;
- latest assistant turn owns a stable response action row;
- Copy/复制 exists on that row;
- terminal evidence is stable across repeated observations.

Stop-button disappearance alone is never completion evidence.

Only validated canonical /c/<conversation-id> URLs are durable recovery URLs.

## 10. Execution profile

Introduce ExecutionProfile:
- model;
- thinking_effort;
- connector requirements;
- optional tool mode.

Before dispatch, application asks BrowserPort to ensure and verify the profile. The adapter returns observed values. If requested model/thinking cannot be verified, fail closed rather than inheriting the prior UI selection.

## 11. Recovery state machine

Recovery timers are policy inputs, never hidden inside CDP code:
- dispatch confirmation window;
- stale-progress reload window;
- rate-limit backoff;
- continuation window;
- global timeout.

Possible application effects:
- ResendOriginalPrompt;
- ReloadCurrentConversation;
- DismissRateLimitAndBackoff;
- ContinueAll;
- ReattachCanonicalConversation;
- StartFreshConversationWithRecoveryEnvelope;
- FailRun.

### RecoveryEnvelope

When a fresh conversation is necessary, build the new prompt from durable state, not just the last visible fragment.

It should include:
- original goal;
- latest authoritative acceptance/planning prompt when applicable;
- materially relevant assistant progress/status messages from the interrupted run;
- completed work;
- remaining work;
- blockers;
- exact repository/commit/task identity;
- explicit instruction to continue rather than redo completed work.

The envelope is versioned, serializable and testable.

## 12. Concurrency

Use bounded actor-style concurrency:
- Tokio tasks are runtime implementation details;
- mpsc commands between actors;
- cancellation propagation;
- Semaphore limits active RunWorkers;
- AccountBrowserActor serializes browser-level resource mutation;
- each RunWorker owns one TargetLease.

Do not guard the whole runtime with one global Mutex.

## 13. Security

- CDP binds to 127.0.0.1 only.
- profiles are user-owned local state.
- do not collect passwords, OTPs, API tokens or payment data.
- cookie export is not a normal runtime workflow.
- profile directories should be mode 0700 on Linux.
- logs contain task/run IDs and hashes; prompt bodies and sensitive page text are excluded by default.
- auto approval remains exact Allow once/current-session only.
- uncertain authorization UI fails closed.

## 14. Observability

Structured tracing fields:
- task_id;
- run_id;
- account_id;
- target_id;
- state;
- transition;
- recovery_count;
- dispatch_attempt;
- duration.

Metrics later:
- dispatch confirmation latency;
- first assistant activity latency;
- completion latency;
- approval count;
- reload count;
- continuation count;
- browser restart count;
- terminal failure reason.

## 15. Testing pyramid

Layer 1: domain unit/property tests.
- Stop disappeared is not completion.
- stale Copy from old turn is not completion.
- canonical URL rejects transient routes.
- approval fingerprint stability.

Layer 2: application fake ports + virtual clock.
- 90-second resend.
- 15-minute reload.
- 5-minute rate-limit backoff.
- 30-minute continuation.
- timeout.
- stable terminal evidence.
- idempotency decisions.

Layer 3: adapter contract with sanitized DOM fixtures and mock CDP.

Layer 4: real Chromium against local fixture page.

Layer 5: authenticated Linux ChatGPT with user-owned login and exact commit/environment/evidence.

A lower layer never substitutes for a higher acceptance layer.

## 16. Architecture gates

CI rejects:
- Tokio/HTTP/CDP/SQLite/browser dependencies in domain;
- concrete adapters imported by application;
- concrete adapters imported directly by cli;
- browser selector strings in domain/application;
- recovery policy duplicated in adapters/runtime;
- automatic approval broader than Allow once/current session.

Architecture is a release gate, not documentation advice.

## 17. Implementation order

1. establish dependency boundaries and architecture CI;
2. put recovery orchestration behind application ports;
3. add SQLite durable journal and leases;
4. add Supervisor/AccountBrowserActor/RunWorker;
5. add target ownership and crash reattachment;
6. port queue/task-report semantics;
7. implement verified model/thinking selection;
8. add RecoveryEnvelope and fresh-chat handoff;
9. run local Chromium contract suite;
10. run real authenticated Linux acceptance.

Do not split into distributed services unless profiling or isolation evidence proves the modular monolith insufficient.
