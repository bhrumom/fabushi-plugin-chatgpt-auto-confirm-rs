use anyhow::{Context, Result, anyhow, bail};
use fabushi_chatgpt_domain::PageSnapshot;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

#[derive(Debug, Clone, Deserialize)]
struct TargetInfo {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    title: String,
    url: String,
    #[serde(rename = "webSocketDebuggerUrl")]
    websocket_debugger_url: Option<String>,
}

pub struct ChatGptCdp {
    endpoint: String,
    socket: Mutex<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    next_id: AtomicU64,
}

impl ChatGptCdp {
    pub async fn connect(endpoint: &str) -> Result<Self> {
        let endpoint = endpoint.trim_end_matches('/').to_owned();
        let targets: Vec<TargetInfo> = reqwest::get(format!("{endpoint}/json/list"))
            .await
            .context("failed to reach Chromium remote debugging endpoint")?
            .error_for_status()?
            .json()
            .await
            .context("invalid /json/list response")?;

        let target = select_chatgpt_target(&targets).ok_or_else(|| {
            anyhow!("no ChatGPT page target found; open https://chatgpt.com first")
        })?;
        let ws_url = target
            .websocket_debugger_url
            .clone()
            .ok_or_else(|| anyhow!("selected ChatGPT target has no websocket debugger URL"))?;
        let (socket, _) = connect_async(&ws_url)
            .await
            .with_context(|| format!("failed to connect CDP websocket {ws_url}"))?;

        Ok(Self {
            endpoint,
            socket: Mutex::new(socket),
            next_id: AtomicU64::new(1),
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    async fn command(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let payload = json!({
            "id": id,
            "method": method,
            "params": params,
        });

        let mut socket = self.socket.lock().await;
        socket
            .send(Message::Text(payload.to_string().into()))
            .await?;

        while let Some(message) = socket.next().await {
            let message = message?;
            let text = match message {
                Message::Text(text) => text,
                Message::Binary(bytes) => String::from_utf8(bytes.to_vec())?.into(),
                Message::Ping(data) => {
                    socket.send(Message::Pong(data)).await?;
                    continue;
                }
                Message::Pong(_) => continue,
                Message::Close(frame) => bail!("CDP websocket closed: {frame:?}"),
                _ => continue,
            };
            let value: Value = serde_json::from_str(&text)?;
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = value.get("error") {
                bail!("CDP command {method} failed: {error}");
            }
            return Ok(value["result"].clone());
        }

        bail!("CDP websocket ended before response to {method}")
    }

    pub async fn evaluate(&self, expression: &str) -> Result<Value> {
        let result = self
            .command(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "awaitPromise": true,
                    "returnByValue": true,
                    "userGesture": true,
                }),
            )
            .await?;

        if let Some(exception) = result.get("exceptionDetails") {
            bail!("JavaScript evaluation failed: {exception}");
        }
        Ok(result
            .pointer("/result/value")
            .cloned()
            .unwrap_or(Value::Null))
    }

    pub async fn snapshot(&self) -> Result<PageSnapshot> {
        let value = self.evaluate(SNAPSHOT_SCRIPT).await?;
        serde_json::from_value(value).context("failed to decode ChatGPT page snapshot")
    }

    pub async fn send_prompt(&self, prompt: &str) -> Result<()> {
        let encoded = serde_json::to_string(prompt)?;
        let script = SEND_PROMPT_SCRIPT.replace("__PROMPT_JSON__", &encoded);
        let value = self.evaluate(&script).await?;
        match value.get("ok").and_then(Value::as_bool) {
            Some(true) => Ok(()),
            _ => bail!("ChatGPT prompt dispatch failed: {value}"),
        }
    }

    pub async fn click_allow_once(&self) -> Result<bool> {
        let value = self.evaluate(APPROVE_ONCE_SCRIPT).await?;
        Ok(value
            .get("clicked")
            .and_then(Value::as_bool)
            .unwrap_or(false))
    }

    pub async fn dismiss_rate_limit_notice(&self) -> Result<bool> {
        let value = self.evaluate(DISMISS_RATE_LIMIT_SCRIPT).await?;
        Ok(value
            .get("clicked")
            .and_then(Value::as_bool)
            .unwrap_or(false))
    }

    pub async fn reload(&self) -> Result<()> {
        self.command("Page.reload", json!({"ignoreCache": false}))
            .await?;
        Ok(())
    }

    pub async fn navigate(&self, url: &str) -> Result<()> {
        self.command("Page.navigate", json!({"url": url})).await?;
        Ok(())
    }
}

fn select_chatgpt_target(targets: &[TargetInfo]) -> Option<&TargetInfo> {
    targets
        .iter()
        .filter(|target| target.kind == "page" && target.websocket_debugger_url.is_some())
        .find(|target| {
            target.url.starts_with("https://chatgpt.com")
                || target.url.starts_with("https://chat.openai.com")
        })
        .or_else(|| {
            targets
                .iter()
                .filter(|target| target.kind == "page" && target.websocket_debugger_url.is_some())
                .find(|target| target.title.to_ascii_lowercase().contains("chatgpt"))
        })
}

const SNAPSHOT_SCRIPT: &str = r#"
(() => {
  const norm = (v) => String(v ?? '').replace(/\s+/g, ' ').trim();
  const textOf = (el) => norm(el?.innerText || el?.textContent || '');
  const aria = (el) => norm(el?.getAttribute?.('aria-label') || '');
  const testid = (el) => norm(el?.getAttribute?.('data-testid') || '');
  const buttons = [...document.querySelectorAll('button')];
  const isStop = (b) => /(^|\b)(stop|停止|停止生成|停止回答)(\b|$)/i.test([textOf(b), aria(b), testid(b)].join(' '));
  const isCopy = (b) => /(copy|复制)/i.test([textOf(b), aria(b), testid(b)].join(' '));
  const isApproval = (b) => /(allow once|允许一次|approve once|仅允许本次|允许本次)/i.test([textOf(b), aria(b)].join(' '));

  const userTurns = [...document.querySelectorAll('[data-message-author-role="user"]')];
  const assistantTurns = [...document.querySelectorAll('[data-message-author-role="assistant"]')];
  const lastAssistant = assistantTurns.at(-1) || null;
  const lastAssistantButtons = lastAssistant ? [...lastAssistant.querySelectorAll('button')] : [];
  const copyAvailable = lastAssistantButtons.some(isCopy);
  const actionButtons = lastAssistantButtons.filter((b) => {
    const s = [textOf(b), aria(b), testid(b)].join(' ');
    return /(copy|复制|good|bad|like|dislike|regenerate|retry|branch|read aloud|朗读|分享|share)/i.test(s);
  });

  const lastUser = userTurns.at(-1);
  const lastAssistantAfterLastUser = !!lastAssistant && !!lastUser &&
    (lastUser.compareDocumentPosition(lastAssistant) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0;

  const composer = document.querySelector('#prompt-textarea, textarea, [contenteditable="true"][data-lexical-editor="true"], [contenteditable="true"]');
  const composerText = norm(composer?.value ?? composer?.innerText ?? composer?.textContent ?? '');

  return {
    url: location.href,
    title: document.title,
    user_turns: userTurns.length,
    assistant_turns: assistantTurns.length,
    stop_available: buttons.some(isStop),
    waiting_for_approval: buttons.some(isApproval),
    copy_available_on_last_assistant: copyAvailable,
    response_actions_complete: copyAvailable && actionButtons.length >= 1,
    response_action_turn_bound_to_last: lastAssistantAfterLastUser && assistantTurns.length >= userTurns.length,
    awaiting_assistant: userTurns.length > assistantTurns.length,
    assistant_text: norm(lastAssistant?.innerText || lastAssistant?.textContent || ''),
    composer_text: composerText,
  };
})()
"#;

const SEND_PROMPT_SCRIPT: &str = r#"
(() => {
  const prompt = __PROMPT_JSON__;
  const norm = (v) => String(v ?? '').replace(/\s+/g, ' ').trim();
  const composer = document.querySelector('#prompt-textarea, textarea, [contenteditable="true"][data-lexical-editor="true"], [contenteditable="true"]');
  if (!composer) return {ok:false, reason:'composer-not-found'};

  composer.focus();
  if ('value' in composer) {
    const proto = Object.getPrototypeOf(composer);
    const descriptor = Object.getOwnPropertyDescriptor(proto, 'value');
    if (descriptor?.set) descriptor.set.call(composer, prompt);
    else composer.value = prompt;
    composer.dispatchEvent(new Event('input', {bubbles:true}));
    composer.dispatchEvent(new Event('change', {bubbles:true}));
  } else {
    composer.replaceChildren();
    const p = document.createElement('p');
    p.textContent = prompt;
    composer.appendChild(p);
    composer.dispatchEvent(new InputEvent('input', {bubbles:true, inputType:'insertText', data:prompt}));
  }

  const buttons = [...document.querySelectorAll('button')];
  const send = buttons.find((b) => {
    const s = [b.getAttribute('aria-label'), b.getAttribute('data-testid'), b.innerText].map(norm).join(' ');
    return /(send|发送)/i.test(s) && !b.disabled;
  }) || document.querySelector('button[data-testid="send-button"]:not(:disabled)');

  if (!send) return {ok:false, reason:'send-button-not-found'};
  send.click();
  return {ok:true};
})()
"#;

const APPROVE_ONCE_SCRIPT: &str = r#"
(() => {
  const norm = (v) => String(v ?? '').replace(/\s+/g, ' ').trim();
  const buttons = [...document.querySelectorAll('button')];
  const exact = buttons.find((b) => /^(allow once|允许一次|approve once|仅允许本次|允许本次)$/i.test(norm(b.innerText || b.textContent || b.getAttribute('aria-label'))));
  if (!exact) return {clicked:false};
  exact.click();
  return {clicked:true, label:norm(exact.innerText || exact.getAttribute('aria-label'))};
})()
"#;

const DISMISS_RATE_LIMIT_SCRIPT: &str = r#"
(() => {
  const norm = (v) => String(v ?? '').replace(/\s+/g, ' ').trim();
  const pageText = norm(document.body?.innerText || '');
  if (!/(too many requests|request.*frequent|请求过于频繁|请求太频繁)/i.test(pageText)) return {clicked:false};
  const button = [...document.querySelectorAll('button')].find((b) => /^(got it|ok|明白了|知道了)$/i.test(norm(b.innerText || b.textContent || b.getAttribute('aria-label'))));
  if (!button) return {clicked:false};
  button.click();
  return {clicked:true};
})()
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_prefers_chatgpt_page() {
        let targets = vec![
            TargetInfo {
                id: "1".into(),
                kind: "page".into(),
                title: "Other".into(),
                url: "https://example.com".into(),
                websocket_debugger_url: Some("ws://localhost/1".into()),
            },
            TargetInfo {
                id: "2".into(),
                kind: "page".into(),
                title: "ChatGPT".into(),
                url: "https://chatgpt.com/c/abc".into(),
                websocket_debugger_url: Some("ws://localhost/2".into()),
            },
        ];
        assert_eq!(select_chatgpt_target(&targets).unwrap().id, "2");
    }
}
