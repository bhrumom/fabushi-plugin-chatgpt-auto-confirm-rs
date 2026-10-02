#!/bin/sh
set -eu

fail() {
  echo "architecture gate: $1" >&2
  exit 1
}

contains() {
  file="$1"
  pattern="$2"
  grep -Eq "$pattern" "$file"
}

DOMAIN_CARGO="crates/domain/Cargo.toml"
APP_CARGO="crates/application/Cargo.toml"
CDP_CARGO="crates/adapters/chatgpt-cdp/Cargo.toml"
LINUX_CARGO="crates/adapters/linux-browser/Cargo.toml"
SQLITE_CARGO="crates/adapters/sqlite-store/Cargo.toml"
ATTACHMENT_CARGO="crates/adapters/attachment-store/Cargo.toml"
PROCESS_CARGO="crates/adapters/chatgpt-desktop-process/Cargo.toml"
ATSPI_CARGO="crates/adapters/chatgpt-desktop-atspi/Cargo.toml"
MACOS_CARGO="crates/adapters/chatgpt-desktop-macos/Cargo.toml"
CLI_CARGO="crates/cli/Cargo.toml"

if contains "$DOMAIN_CARGO" 'fabushi-chatgpt-(application|cdp|runtime|linux-browser)|tokio|reqwest|tungstenite|rusqlite|sqlx'; then
  fail "domain depends on an outer layer or runtime/infrastructure library"
fi

if contains "$APP_CARGO" 'fabushi-chatgpt-(cdp|runtime|linux-browser)|reqwest|tungstenite|rusqlite|sqlx'; then
  fail "application depends on a concrete adapter/runtime"
fi

if contains "$CDP_CARGO" 'fabushi-chatgpt-(runtime|linux-browser|auto-confirm)'; then
  fail "CDP adapter depends on runtime, Linux adapter, or CLI"
fi

if contains "$LINUX_CARGO" 'fabushi-chatgpt-(runtime|cdp|auto-confirm)'; then
  fail "Linux browser adapter depends on another outer adapter/runtime"
fi

if contains "$SQLITE_CARGO" 'fabushi-chatgpt-(runtime|cdp|linux-browser|auto-confirm)'; then
  fail "SQLite adapter depends on runtime, another outer adapter, or CLI"
fi

if contains "$ATTACHMENT_CARGO" 'fabushi-chatgpt-(runtime|cdp|linux-browser|sqlite-store|auto-confirm)'; then
  fail "attachment adapter depends on runtime, another outer adapter, or CLI"
fi

if contains "$ATSPI_CARGO" 'fabushi-chatgpt-(runtime|cdp|linux-browser|sqlite-store|attachment-store|auto-confirm)'; then
  fail "desktop AT-SPI adapter depends on runtime, another outer adapter, or CLI"
fi

if contains "$PROCESS_CARGO" 'fabushi-chatgpt-(runtime|cdp|linux-browser|sqlite-store|attachment-store|auto-confirm)'; then
  fail "desktop process adapter depends on runtime, another outer adapter, or CLI"
fi

if contains "$MACOS_CARGO" 'fabushi-chatgpt-(runtime|cdp|linux-browser|sqlite-store|attachment-store|auto-confirm)'; then
  fail "desktop macOS adapter depends on runtime, another outer adapter, or CLI"
fi

if contains "$CLI_CARGO" 'fabushi-chatgpt-(application|domain|cdp|linux-browser)'; then
  fail "CLI bypasses the runtime composition root"
fi

if grep -R -n -E 'querySelector|data-testid|Runtime\.evaluate|Page\.reload|remote-debugging-port|PageSnapshot|BrowserPort|canonical_conversation_url|https://chatgpt\.com/c/' \
  crates/domain/src crates/application/src >/tmp/fabushi-architecture-selector-leaks.txt 2>/dev/null; then
  cat /tmp/fabushi-architecture-selector-leaks.txt >&2
  fail "browser/desktop implementation detail or superseded web abstraction leaked into domain/application"
fi

if grep -R -n -E 'Allow once|允许一次|Approve once|仅允许本次|允许本次' crates/domain/src >/tmp/fabushi-domain-ui-labels.txt 2>/dev/null; then
  cat /tmp/fabushi-domain-ui-labels.txt >&2
  fail "localized approval UI labels leaked into domain"
fi

echo "architecture gate: PASS"
