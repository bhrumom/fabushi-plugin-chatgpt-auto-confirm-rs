# Repository agent instructions

1. Read docs/specs/rust-linux-chatgpt-auto-confirm.md before changing behavior.
2. Do not claim full source replacement while any required migration-ledger item remains partial or pending.
3. Do not weaken terminal completion to Stop button disappeared.
4. Automatic authorization must remain limited to exact Allow once/current-session actions.
5. Never add credential/cookie exfiltration or plaintext secret logging.
6. Keep browser/platform details out of crates/domain.
7. Every behavior change needs a regression test and an update to the implementation-status section of the spec.
