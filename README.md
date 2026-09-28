# fabushi-plugin-chatgpt-auto-confirm-rs

Rust/Linux rewrite of bhrumom/fabushi-plugin-chatgpt-auto-confirm.

The current implementation can attach to a Chromium or Chrome session through CDP, send a ChatGPT prompt, automatically click Allow once / 允许一次, monitor the latest turn, recover from stalls, and return only after stable Copy-button terminal evidence.

The authoritative migration and acceptance contract is docs/specs/rust-linux-chatgpt-auto-confirm.md.

## Build

cargo build --release -p fabushi-chatgpt-auto-confirm

## First login on Linux

Install Chromium or Google Chrome, then run:

./target/release/fabushi-chatgpt-auto-confirm browser --headed true

A dedicated browser profile is created under the local data directory. Log in to ChatGPT yourself in that browser.

## Run

./target/release/fabushi-chatgpt-auto-confirm status

./target/release/fabushi-chatgpt-auto-confirm send --prompt "继续完成所有" --auto-confirm true

Default recovery policy:
- unconfirmed send after 90s -> resend original prompt;
- no progress for 15m -> reload current conversation;
- rate limit -> dismiss notice, wait 5m, at most 3 times;
- no terminal answer after 30m -> send 继续完成所有;
- completion -> latest assistant turn has stable Copy action evidence.

## Security

This runtime does not request or export your ChatGPT password, OTP, cookies, or API tokens. Authentication remains in the user-owned Chromium profile.
