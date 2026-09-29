# ADR-0002: Durable Event Journal plus Materialized State

Status: Accepted

## Decision

Use SQLite WAL with an append-only run event journal and a materialized run snapshot updated in the same transaction.

## Rationale

Browser effects are not transactional. Crash recovery must know both the last intended effect and the last observed settlement. Plain JSON snapshots lose transition history and are unsafe under concurrent recovery.

## Consequences

- every durable transition is auditable;
- startup can reconstruct and resume runs;
- revision checks and worker leases prevent duplicate ownership;
- browser effects are treated as at-least-once and use idempotency fingerprints.
