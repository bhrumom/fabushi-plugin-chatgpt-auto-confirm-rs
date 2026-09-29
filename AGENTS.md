# Repository agent instructions

1. Read `docs/specs/rust-linux-chatgpt-auto-confirm.md`, `docs/architecture.md`, and the ADRs under `docs/adr/` before changing behavior or structure.
2. Preserve the dependency direction: `cli -> runtime -> application -> domain`; adapters point inward and never own policy.
3. Run `./scripts/check-architecture.sh` for every structural change.
4. Do not claim full source replacement while any required migration-ledger item remains partial or pending.
5. Do not weaken terminal completion to Stop-button disappearance.
6. Automatic authorization must remain limited to exact Allow once/current-session actions.
7. Never add credential/cookie exfiltration or plaintext secret logging.
8. Keep browser/platform/database details out of `crates/domain` and concrete adapters out of `crates/application`.
9. CLI must depend on runtime only among internal crates.
10. Every behavior change needs a regression test and an update to the implementation-status section of the spec.
