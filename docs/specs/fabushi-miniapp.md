# ChatGPT Auto-Confirm in Fabushi — offline package specification

Status: active; release acceptance pending. Date: 2026-10-13.
Baseline: main at 79b432dbe2ba007bb83602c46b759b4490cce17e,
authoritative spec `docs/specs/rust-linux-chatgpt-auto-confirm.md` and
`docs/specs/desktop-tui-userscript-parity.md`.
Parent: Fabushi OMA-001/FPM-001 at 371b72f1cd20de03dc4dff9a15ed761054968093.

## Goal, current state and ownership

Deliver a complete signed offline Web mini app (`.fabapp`) through Fabushi's
existing Marketplace for `bhrumom/fabushi-plugin-chatgpt-auto-confirm-rs`.
Reuse the repository's pure Rust `fabushi-chatgpt-domain` implementation
(`Task`, `TaskMode`, `Phase`, `TaskState`, `ExecutionProfile`, `PageSnapshot`,
`ReviewReport::parse`, `Task::prompt`, `bounded`, `text_hash`) rather than
duplicating task/review logic in JavaScript or inventing a second protocol.
The existing native CLI, TUI, SQLite WAL queue, and CDP adapters remain
authoritative and unchanged.

Offline scope: bounded local task queue planning (up to 64 tasks in memory),
deterministic Work/Review prompt synthesis with `[Fabushi:<token>]` session
markers, `MAHAYANA_TASK_REPORT_V1` independent review report verification,
`PageSnapshot` terminal Copy-button invariant and canonical `/c/<id>` URL
validation, local JSON report export, and source/permission inspection.
Live ChatGPT.app / Chromium CDP attachment, automatic `Allow once / 允许一次`
clicking, and background SQLite supervision require the native Host/Connector
runtime; they are explicitly unavailable in this offline Web sandbox and are
never mocked as executing live browser automation.

Existing `crates/domain` (`fabushi-chatgpt-domain`) owns task invariants,
prompt templates, review report parsing, and terminal snapshot rules. A separate
`miniapp/wasm` Cargo workspace compiles `fabushi-chatgpt-domain` to WebAssembly
and owns only browser bindings. `miniapp/web` owns DOM rendering and local state.
Fabushi Host owns trusted package installation, per-account/per-app origin and
storage isolation, and permissions; Fabushi Backend Marketplace owns publisher
namespace binding, signing, security scan, release review, and catalog listing.

## Requirements and contracts

| ID | Required observable behavior |
| --- | --- |
| CAC-M01 | Cold-launch HTML/CSS/JS/Wasm from installed local bytes; no CDN, remote font, service worker, login, or network request. |
| CAC-M02 | Bounded local task list (at most 64 tasks, goal up to 16 KiB per task); literal `textContent` rendering only; no host filesystem path access or background timer loop. |
| CAC-M03 | Reuse `fabushi-chatgpt-domain` in Wasm for Work/Review prompt generation (`[Fabushi:<token>]`), `MAHAYANA_TASK_REPORT_V1` parsing (`ReviewReport::parse`), and `PageSnapshot` terminal/URL invariants. |
| CAC-M04 | Reject mismatched `taskId`/`round`, empty `summary`, non-empty `next` on `complete`, or empty `next` on `next`; reject `Stop` disappearance alone as terminal completion. |
| CAC-M05 | Offline results represent local prompt/review/snapshot verification only, never live ChatGPT CDP dispatch, automatic approval clicks, or remote completion. |
| CAC-M06 | Export bounded JSON report (`chatgpt-auto-confirm-local-report.json`) only on explicit user action; settings-only `localStorage` with visible quota fallback and delete control. |
| CAC-M07 | Strict `.fabapp` v1/JCS/Ed25519 domain `fabushi.fabapp.manifest.v1\0`, complete file SHA256/size/MIME table, `packageClass: "catalog"`, `requestedCapabilities: []`, `networkPolicy: { mode: "deny", allowedOrigins: [] }`. |
| CAC-M08 | Managed catalog-authorized publisher key (`bhrumom`); scan, moderation, and release review signatures remain separate from package signing. |
| CAC-M09 | Desktop preview (`platforms: ["desktop"]`) with responsive light/dark/narrow layout. |
| CAC-M10 | GitHub Actions only for build, architecture check, Wasm bindgen, package tamper tests, and offline Playwright Chromium verification; preserve existing native Rust/DOM gates. |
| CAC-M11 | Distributed under the repository's `MIT` license (`LICENSE.txt`) with bundled `THIRD-PARTY-NOTICES.txt` and `WebCargoLock.txt`. |

Identity: `appId`: `org.fabushi.chatgpt-auto-confirm`, `publisherId`: `bhrumom`,
`version`: `0.1.0`, `minHostApi`: `1`, `runtime`: `web`, `entrypoint`: `index.html`.
Wasm exports `max_tasks`, `build_task_prompt`, `verify_review_report`, and
`evaluate_snapshot`. Only bounded UTF-8 strings and integers cross the Wasm ABI.

## Acceptance, publication and rollback

| Gate | Evidence needed |
| --- | --- |
| AC-M1 | Exact-head Rust/Wasm/browser tests proving CAC-M01..06 in an offline Playwright context with network denied. |
| AC-M2 | Complete resources/signature/digests plus mutation, traversal, wrong-key, and extra-file rejection tests. |
| AC-M3 | Verified publisher binding (`bhrumom`), protected production signing, candidate security scan, release review approval, and immutable public catalog entry. |
| AC-M4 | Real signed packaged Fabushi Desktop install, offline launch, per-account/per-app isolation, and code removal retaining user data. |
