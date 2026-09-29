# ADR-0001: Modular Monolith with Hexagonal Boundaries

Status: Accepted

## Decision

Use one Rust application composed of domain, application, adapters, runtime and CLI crates. Do not use microservices for browser automation orchestration.

## Rationale

The critical invariants are local ownership of browser processes, page targets, account profiles, run leases and recovery state. A distributed design adds network partitions, duplicated ownership and cross-service transactions without a current benefit.

Crate boundaries provide compile-time modularity; actor ownership provides runtime isolation.

## Consequences

- one deployable runtime;
- modules remain independently testable;
- adapters can be replaced without changing recovery policy;
- extraction into services remains possible later if evidence justifies it.
