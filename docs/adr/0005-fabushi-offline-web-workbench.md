# ADR 0005: Fabushi Offline Web/Wasm Workbench Adapter

Status: accepted
Date: 2026-10-13

## Context

The user requested packaging `bhrumom/fabushi-plugin-chatgpt-auto-confirm-rs` as
an offline mini app (`.fabapp`) conforming to Fabushi's OMA-001 and FPM-001
specifications and listing it in the Fabushi Plugins Marketplace.

Our canonical Hexagonal Architecture (`docs/architecture.md` and
`scripts/check-architecture.sh`) requires `crates/domain` to remain pure and
free of browser, CDP, Tokio, or OS dependencies, while live CDP supervision
requires local loopback/process privileges that an OMA-001 `networkPolicy: deny`,
`requestedCapabilities: []` Web sandbox intentionally forbids.

## Decision

1. Create a separate Cargo workspace at `miniapp/wasm` (`chatgpt-auto-confirm-web-adapter`,
   `crate-type = ["cdylib", "rlib"]`, `unsafe_code = "forbid"`) that depends inward on
   `fabushi-chatgpt-domain` (`crates/domain`).
2. Expose pure domain operations (`Task::prompt`, `ReviewReport::parse`,
   `PageSnapshot::is_terminal`, `PageSnapshot::canonical_conversation_url`,
   `PageSnapshot::activity_fingerprint`, `text_hash`) over a narrow `wasm-bindgen`
   string/integer ABI.
3. Deliver the offline Task & Review Workbench (`miniapp/web`) as a deterministic
   OMA-001 resource bundle (`appId`: `org.fabushi.chatgpt-auto-confirm`,
   `publisherId`: `bhrumom`, `version`: `0.1.0`) with inline base64 Wasm bytes
   (`wasm-bytes.js`) so cold startup requires zero `fetch` or network access.
4. Explicitly disclose in the UI, manifest metadata, and specs that live CDP
   attachment and automatic `Allow once / 允许一次` clicking require the native
   Host/Connector runtime and are not executed by the offline Web surface.

## Consequences

- `crates/domain`, `crates/application`, and `./scripts/check-architecture.sh`
  remain 100% unchanged and pure.
- The offline mini app reuses the exact Rust domain implementation for task
  prompts, `MAHAYANA_TASK_REPORT_V1` review verification, and terminal snapshot
  checks without code duplication.
- Unsigned preparation archives (`.zip`) are built and verified in GitHub Actions,
  while production `.fabapp` signing and release approval occur only through the
  protected Fabushi Marketplace workflows.
