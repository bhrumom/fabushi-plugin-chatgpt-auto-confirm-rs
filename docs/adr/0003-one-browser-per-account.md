# ADR-0003: One Browser Process per Account, One Target per Run

Status: Accepted

## Decision

On Linux, own one authenticated Chromium/Chrome process per account profile and lease a distinct page target to each active run.

## Rationale

A Chromium profile should not be concurrently written by multiple independent browser processes. Copying a live profile per task is fragile and can create stale sessions. A single account browser safely shares authenticated state while target ownership isolates conversations.

## Consequences

- AccountBrowserActor is the sole process/profile owner;
- RunWorker touches only its target lease;
- browser crash recovery uses durable canonical conversation URLs;
- concurrency is bounded by Supervisor policy.
