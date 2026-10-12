# fabushi-plugin-chatgpt-auto-confirm-rs

Rust rewrite of bhrumom/fabushi-plugin-chatgpt-auto-confirm, with macOS ChatGPT desktop and explicit Linux browser backends.

The current implementation can attach to a Chromium or Chrome session through CDP, send a ChatGPT prompt, automatically click Allow once / 允许一次, monitor the latest turn, recover from stalls, and return only after stable Copy-button terminal evidence.

The authoritative migration contract is `docs/specs/rust-linux-chatgpt-auto-confirm.md`. The canonical architecture is `docs/architecture.md`; architecture decisions are recorded under `docs/adr/`.

## Architecture

This is a modular monolith with strict Hexagonal Architecture boundaries:

- `crates/domain` — pure invariants and durable value types;
- `crates/application` — use cases, recovery policy, Browser/Clock ports;
- `crates/adapters/chatgpt-cdp` — ChatGPT/CDP implementation;
- `crates/adapters/linux-browser` — Linux Chromium process implementation;
- `crates/runtime` — composition root, terminal workbench and background supervisor;
- `crates/cli` — thin operator surface.

Run the architecture gate with `./scripts/check-architecture.sh`.

## Build

cargo build --release -p fabushi-chatgpt-auto-confirm

## macOS ChatGPT desktop (default)

Build the binary, then launch the installed ChatGPT.app with its own profile:

```sh
./target/release/fabushi-chatgpt-auto-confirm desktop
./target/release/fabushi-chatgpt-auto-confirm inspect
./target/release/fabushi-chatgpt-auto-confirm new-chat
./target/release/fabushi-chatgpt-auto-confirm send --prompt "完成这个任务"
```

The default backend is desktop, at http://127.0.0.1:9223. It connects only to the main `app://-/index.html` Electron renderer, excluding overlay and auxiliary windows. It never falls back to a browser tab. `send` refuses a Work composer or an existing unrelated draft. `new-chat` opens the client Chat interface. The task workbench verifies the configured model and thinking effort before each dispatch. Sign in interactively if the desktop client asks. Authentication remains in the client; no cookies or credentials are copied.

`desktop --app /Applications/ChatGPT.app --profile /path/to/desktop-profile --port 9223` allows explicit installation/profile selection. An already-running app without debugging can remain open while this separate instance runs.

## Desktop task workbench

```sh
./target/release/fabushi-chatgpt-auto-confirm desktop
./target/release/fabushi-chatgpt-auto-confirm --database ./state/workbench.sqlite tui
```

The TUI accepts multiline goals, task files and attachments. Press `Ctrl+N` for
new task, `i` to enter input, and `Ctrl+S` or `Alt+Enter` to dispatch. Use arrows
or `j`/`k` to select a task, `p` to pause/resume, `P` to pause all, `s` to resume
all, `e` to edit a goal, `m`/`r` for model/effort, and `v` for combined activity.
Press `?` for all controls. `q` closes the TUI while its background worker
continues. Start supervision without the TUI using `supervise`.

The dashboard shows the attempted task separately from the last confirmed
conversation, scan age, confirmed switch count, phase/round, authorization and
recovery counts, errors, and retry countdowns. Specific operations from the past hour are persisted in SQLite and shown
before scan detail; `v` opens the full hour across tasks and PageDown scrolls
older operations. `logs --limit 10000` reads that same one-hour action journal. The TUI-started worker appends operational
logs alongside the database as `workbench.supervisor.log`.

Continuous goals alternate execution and fresh independent review. A task is
complete only after a valid identity-bound review report. Review rounds are
separate from individual conversation recovery attempts.

Native authorization opens the split menu and prefers the exact current
conversation grant, including `Allow GitHub for this conversation`; allow-once
is a fallback when that grant is unavailable. Persistent grants are excluded.
Load failures retry the same error card every 30 seconds, including failures
before the first stable conversation id. Unconfirmed sends keep their intent
after 90 seconds and are not automatically replayed. Work-conversion cards
with countdown badges are answered with Stay in chat. Connection
interruptions carry the original goal and saved public progress into a fresh
conversation after an 8-second guard and the normal 60-second send interval.
Drafts and pending authorizations block destructive recovery.

Tasks, session markers, dispatch intents, settings, private attachment copies
and endpoint leases survive worker restarts. Only one worker controls each
local endpoint. Browser-specific media previews and extension tab APIs are
adapted to terminal file metadata and desktop/SQLite ownership. Full userscript
or authenticated Linux replacement acceptance is not claimed; see
`docs/specs/desktop-tui-userscript-parity.md` for the implemented contract.

## First login on Linux

Install Chromium or Google Chrome, then run:

./target/release/fabushi-chatgpt-auto-confirm --backend browser browser

A dedicated browser profile is created under the local data directory. Log in to ChatGPT yourself in that browser.

## Run

./target/release/fabushi-chatgpt-auto-confirm --backend browser status

./target/release/fabushi-chatgpt-auto-confirm --backend browser send --prompt "继续完成所有"

Legacy single-prompt browser recovery policy:
- unconfirmed send after 90s -> resend original prompt;
- no progress for 15m -> reload current conversation;
- rate limit -> dismiss notice, wait 5m, at most 3 times;
- no terminal answer after 30m -> send 继续完成所有;
- completion -> latest assistant turn has stable Copy action evidence.

## Security

This runtime does not request or export your ChatGPT password, OTP, cookies, or API tokens. Authentication remains in the user-owned Chromium profile.

Task-owned sidebar recovery also handles collapsed/virtualized lists, with
bounded waits when desktop rendering is backgrounded. Missing navigation
retries every 30 seconds without replaying a task or opening unrelated chats.
The r3 TUI shows recovery counts instead of treating an online lease as healthy
progress, and periodically restores a worker whose lease has expired.

For supervision independent of an open TUI on macOS, install the per-user launch agent after stopping the previous worker:

```sh
python3 scripts/install-supervisor-launchagent.py --binary /absolute/path/fabushi-chatgpt-auto-confirm --database /absolute/path/state.sqlite --log /absolute/path/supervisor.log
```

Place the database in `~/Library/Application Support/FabushiSupervisor` before installation; macOS background workers can be blocked from accessing Documents. Stop existing workers before migrating an existing database and point the TUI to the same path. The installer copies its executable and writes its log in Application Support, with the requested log path linked to that journal.

The agent starts at login and restarts an exited worker with a 30-second throttle. It shares the endpoint lease with the TUI, so only one worker controls the desktop. ChatGPT must remain available at the configured CDP endpoint; this does not bypass login or network failures. The installer prints inspection and stop commands.
