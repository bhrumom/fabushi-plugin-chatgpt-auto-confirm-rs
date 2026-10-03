# ADR-0003: One Browser Process per Account, One Target per Run

Status: Superseded by ADR-0004

## Historical decision

The previous browser/CDP implementation owned one authenticated Chromium/Chrome process per account profile and leased a distinct page target to each active run.

## Supersession

The authoritative desktop migration spec no longer uses browser process/target ownership as the production topology.

For the ChatGPT desktop product path, ownership is defined by ADR-0004:

- one runtime UI-session lease;
- one `DesktopSessionActor` as the sole desktop UI mutation owner;
- multiple `RunWorker` instances may supervise independent server-side conversations;
- desktop conversation identity is semantic and opaque rather than a canonical web URL;
- browser/CDP code is migration-only scaffolding.

This ADR remains only as historical context for the legacy comparison adapter.
