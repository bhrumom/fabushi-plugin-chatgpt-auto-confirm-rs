# Repository agent instructions

1. Read `docs/specs/desktop-chat-userscript-parity-rust-cli.md` first. It is the authoritative product/migration contract for the desktop ChatGPT Rust CLI.
2. Treat `docs/specs/rust-linux-chatgpt-auto-confirm.md` and browser/CDP-specific architecture text as historical migration context wherever they conflict with the authoritative desktop spec.
3. Before changing behavior or structure, also read `docs/architecture.md` and the ADRs under `docs/adr/`; reconcile/update them to the authoritative desktop spec before implementing conflicting production topology.
4. Preserve the dependency direction: `cli -> runtime -> application -> domain`; adapters point inward and never own policy.
5. Development build/test/format/clippy/architecture validation may run directly on `htch-runtime`. GitHub Actions remains mandatory on the exact candidate commit for merge/release qualification; device-side results do not replace CI evidence.
6. `htch-runtime` is both the preferred desktop development/test host and the real-device acceptance host. Development may compile/test the intended commit directly on-device. Formal release acceptance must use the exact-HEAD packaged GitHub Actions artifact. During product acceptance, device-control may start/inspect the app as an independent oracle, but must not perform Send, reasoning selection, approval, attachment, or other product actions instead of the Rust CLI.
7. Keep `./scripts/check-architecture.sh` as a required GitHub Actions structural gate and update it when architecture boundaries change.
8. Do not claim full source replacement while any required migration-ledger item remains partial, pending, or blocked.
9. Do not weaken terminal completion to Stop-button disappearance.
10. Automatic authorization must remain limited to exact current-conversation/current-session actions; disabled authorization controls are never success evidence.
11. Never add credential/cookie/OTP/API-key exfiltration or plaintext secret logging.
12. Keep browser/desktop/platform/database details out of `crates/domain` and concrete adapters out of `crates/application`.
13. CLI must depend on runtime only among internal crates.
14. Every behavior change needs a regression test and an update to the authoritative spec implementation-status/migration-ledger evidence.
