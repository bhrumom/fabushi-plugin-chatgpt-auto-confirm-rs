# ADR-0004: Single-Writer ChatGPT Desktop Session

Status: Accepted

## Context

The production target is the ChatGPT desktop application. Unlike the legacy browser implementation, runs cannot safely assume one independently owned page target per task. The local desktop UI, native dialogs, reasoning control and attachment picker form one shared mutable resource.

At the same time, multiple ChatGPT conversations may continue running server-side while the local UI is focused elsewhere.

## Decision

Use one durable UI-session lease and one `DesktopSessionActor` as the sole owner of all mutating ChatGPT desktop effects.

Topology:

```
Supervisor
  +-- DesktopSessionActor
  +-- RunWorker(task A)
  +-- RunWorker(task B)
  +-- RunWorker(task C)
```

Rules:

1. Run workers never call AT-SPI, process/window APIs or native dialogs directly.
2. Application policy decides which semantic effect should occur.
3. Desktop adapters observe semantic facts and execute one requested effect at a time.
4. Every destructive effect is first persisted as an outbox intent in the same transaction as state/event changes.
5. UI effects are at-least-once; crash recovery must re-observe the semantic postcondition before replay.
6. Conversation identity is `TaskId/RunId/Phase/Round/GoalRevision + dispatch marker + semantic boundaries + ConversationFingerprint`, optionally plus opaque `ConversationRef`.
7. A blocked task yields its next wake time so the supervisor can service other tasks; one task's cooldown, attachment wait, authorization settlement or recovery must not block the entire runtime.
8. Browser/CDP target leases are legacy migration concepts and are not production desktop ownership.

## Consequences

- local UI mutation is race-free by construction;
- server-side task concurrency remains possible;
- authorization and attachment state cannot be corrupted by two workers manipulating the same desktop surface;
- recovery has one durable authority for deciding whether an effect already happened;
- adapters stay replaceable because application/domain do not know AT-SPI roles, labels, PIDs, Electron internals or coordinates.
