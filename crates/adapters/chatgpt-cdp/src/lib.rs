use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use fabushi_chatgpt_application::BrowserPort;
use fabushi_chatgpt_domain::{ExecutionProfile, ObservedExecutionProfile, PageSnapshot};
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
    target_id: String,
    socket: Mutex<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    next_id: AtomicU64,
}

impl ChatGptCdp {
    pub async fn connect(endpoint: &str) -> Result<Self> {
        let targets = fetch_targets(endpoint).await?;
        let target = select_chatgpt_target(&targets).ok_or_else(|| {
            anyhow!("no ChatGPT page target found; open https://chatgpt.com first")
        })?;
        Self::connect_target(endpoint, target).await
    }

    pub async fn create_target(endpoint: &str, initial_url: &str) -> Result<Self> {
        let endpoint = endpoint.trim_end_matches('/');
        let url = format!("{endpoint}/json/new?{}", urlencoding::encode(initial_url));
        let response = reqwest::Client::new()
            .put(url)
            .send()
            .await
            .context("failed to create CDP page target")?
            .error_for_status()?;
        let target: TargetInfo = response
            .json()
            .await
            .context("invalid /json/new target response")?;
        Self::connect_target(endpoint, &target).await
    }

    pub async fn connect_target_id(endpoint: &str, target_id: &str) -> Result<Self> {
        let targets = fetch_targets(endpoint).await?;
        let target = targets
            .iter()
            .find(|target| target.id == target_id)
            .ok_or_else(|| anyhow!("CDP target {target_id} not found"))?;
        Self::connect_target(endpoint, target).await
    }

    async fn connect_target(endpoint: &str, target: &TargetInfo) -> Result<Self> {
        let ws_url = target
            .websocket_debugger_url
            .clone()
            .ok_or_else(|| anyhow!("selected target has no websocket debugger URL"))?;
        let (socket, _) = connect_async(&ws_url)
            .await
            .with_context(|| format!("failed to connect CDP websocket {ws_url}"))?;
        Ok(Self {
            endpoint: endpoint.trim_end_matches('/').to_owned(),
            target_id: target.id.clone(),
            socket: Mutex::new(socket),
            next_id: AtomicU64::new(1),
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn target_id(&self) -> &str {
        &self.target_id
    }

    async fn command(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let payload = json!({"id": id, "method": method, "params": params});
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
                    "userGesture": true
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

    pub async fn ensure_profile(
        &self,
        requested: &ExecutionProfile,
    ) -> Result<ObservedExecutionProfile> {
        let requested_model = serde_json::to_string(&requested.model)?;
        let requested_thinking = serde_json::to_string(&requested.thinking_effort)?;
        let script = ENSURE_EXECUTION_PROFILE_SCRIPT
            .replace("__MODEL_JSON__", &requested_model)
            .replace("__THINKING_JSON__", &requested_thinking);
        let value = self.evaluate(&script).await?;
        serde_json::from_value(value).context("failed to decode execution profile observation")
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

    pub async fn close_owned_target(&self) -> Result<()> {
        let url = format!(
            "{}/json/close/{}",
            self.endpoint.trim_end_matches('/'),
            self.target_id
        );
        reqwest::Client::new()
            .get(url)
            .send()
            .await
            .context("failed to close CDP target")?
            .error_for_status()?;
        Ok(())
    }
}

#[async_trait]
impl BrowserPort for ChatGptCdp {
    async fn snapshot(&self) -> Result<PageSnapshot> {
        ChatGptCdp::snapshot(self).await
    }

    async fn send_prompt(&self, prompt: &str) -> Result<()> {
        ChatGptCdp::send_prompt(self, prompt).await
    }

    async fn approve_once(&self) -> Result<bool> {
        ChatGptCdp::click_allow_once(self).await
    }

    async fn dismiss_rate_limit_notice(&self) -> Result<bool> {
        ChatGptCdp::dismiss_rate_limit_notice(self).await
    }

    async fn reload(&self) -> Result<()> {
        ChatGptCdp::reload(self).await
    }

    async fn navigate(&self, url: &str) -> Result<()> {
        ChatGptCdp::navigate(self, url).await
    }

    async fn ensure_execution_profile(
        &self,
        requested: &ExecutionProfile,
    ) -> Result<ObservedExecutionProfile> {
        self.ensure_profile(requested).await
    }

    async fn target_identity(&self) -> Result<Option<String>> {
        Ok(Some(self.target_id.clone()))
    }
}

async fn fetch_targets(endpoint: &str) -> Result<Vec<TargetInfo>> {
    reqwest::get(format!("{}/json/list", endpoint.trim_end_matches('/')))
        .await
        .context("failed to reach Chromium remote debugging endpoint")?
        .error_for_status()?
        .json()
        .await
        .context("invalid /json/list response")
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
  const pageText = norm(document.body?.innerText || '');
  const isStop = (b) => /(^|\b)(stop|停止|停止生成|停止回答)(\b|$)/i.test([textOf(b), aria(b), testid(b)].join(' '));
  const isCopy = (b) => /(copy|复制)/i.test([textOf(b), aria(b), testid(b)].join(' '));
  const isApproval = (b) => /^(allow once|允许一次|approve once|仅允许本次|允许本次)$/i.test(norm(textOf(b) || aria(b)));

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
  const approvalButton = buttons.find(isApproval) || null;
  const approvalCard = approvalButton?.closest('[role="dialog"], [data-testid], article, section, div') || null;
  const approvalKey = approvalButton
    ? norm([
        approvalCard?.getAttribute?.('data-testid'),
        approvalCard?.getAttribute?.('data-message-id'),
        textOf(approvalCard).slice(0, 300),
        textOf(approvalButton)
      ].join('|'))
    : null;
  const activeModel = [...document.querySelectorAll('button,[role="button"]')]
    .map((el) => textOf(el) || aria(el))
    .find((text) => /gpt[- ]?5|gpt[- ]?4|o[134]|model/i.test(text)) || null;
  const activeThinking = [...document.querySelectorAll('button,[role="button"]')]
    .map((el) => textOf(el) || aria(el))
    .find((text) => /extra high|极高|high|高|medium|中|low|低/i.test(text)) || null;

  return {
    url: location.href,
    title: document.title,
    user_turns: userTurns.length,
    assistant_turns: assistantTurns.length,
    stop_available: buttons.some(isStop),
    waiting_for_approval: buttons.some(isApproval),
    rate_limit_notice: /(too many requests|request.*frequent|请求过于频繁|请求太频繁)/i.test(pageText),
    connection_interrupted: /(connection interrupted|network error|连接中断|网络错误)/i.test(pageText),
    copy_available_on_last_assistant: copyAvailable,
    response_actions_complete: copyAvailable && actionButtons.length >= 1,
    response_action_turn_bound_to_last: lastAssistantAfterLastUser && assistantTurns.length >= userTurns.length,
    awaiting_assistant: userTurns.length > assistantTurns.length,
    assistant_text: norm(lastAssistant?.innerText || lastAssistant?.textContent || ''),
    composer_text: composerText,
    approval_card_key: approvalKey,
    observed_model: activeModel,
    observed_thinking_effort: activeThinking,
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

const ENSURE_EXECUTION_PROFILE_SCRIPT: &str = r#"
(async () => {
  const requestedModel = __MODEL_JSON__;
  const requestedThinking = __THINKING_JSON__;
  const norm = (v) => String(v ?? '').replace(/\s+/g, ' ').trim();
  const compact = (v) => norm(v).toLowerCase().replace(/\s+/g, ' ');
  const all = () => [...document.querySelectorAll('button,[role="button"],[role="menuitem"],[role="option"]')];
  const label = (el) => norm(el?.innerText || el?.textContent || el?.getAttribute?.('aria-label') || el?.getAttribute?.('data-testid') || '');
  const exactish = (observed, requested) => {
    const o = compact(observed);
    const r = compact(requested);
    return !!o && (o === r || o.includes(r));
  };
  const visible = (el) => !!el && el.getClientRects().length > 0;
  const findVisible = (predicate) => all().find((el) => visible(el) && predicate(label(el)));
  const observed = () => {
    const labels = all().filter(visible).map(label);
    return {
      model: labels.find((text) => /gpt[- ]?5|gpt[- ]?4|o[134]/i.test(text)) || null,
      thinking_effort: labels.find((text) => /extra high|极高|high|高|medium|中|low|低/i.test(text)) || null,
    };
  };
  const settle = () => new Promise((resolve) => setTimeout(resolve, 180));

  let state = observed();
  if (!exactish(state.model, requestedModel)) {
    const opener = findVisible((text) => /model|模型|gpt[- ]?5|gpt[- ]?4|o[134]/i.test(text));
    if (!opener) return state;
    opener.click();
    await settle();
    const option = findVisible((text) => exactish(text, requestedModel));
    if (!option) return observed();
    option.click();
    await settle();
  }

  state = observed();
  if (!exactish(state.thinking_effort, requestedThinking)) {
    const opener = findVisible((text) => /thinking|reasoning|思考|推理|extra high|极高|high|高|medium|中|low|低/i.test(text));
    if (!opener) return state;
    opener.click();
    await settle();
    const option = findVisible((text) => exactish(text, requestedThinking));
    if (!option) return observed();
    option.click();
    await settle();
  }
  return observed();
})()
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn target(id: &str, title: &str, url: &str) -> TargetInfo {
        TargetInfo {
            id: id.into(),
            kind: "page".into(),
            title: title.into(),
            url: url.into(),
            websocket_debugger_url: Some(format!("ws://localhost/{id}")),
        }
    }

    #[test]
    fn target_prefers_chatgpt_page() {
        let targets = vec![
            target("1", "Other", "https://example.com"),
            target("2", "ChatGPT", "https://chatgpt.com/c/abc"),
        ];
        assert_eq!(select_chatgpt_target(&targets).unwrap().id, "2");
    }

    #[test]
    fn approval_script_is_exact_and_never_mentions_always_allow() {
        assert!(
            APPROVE_ONCE_SCRIPT
                .contains("^(allow once|允许一次|approve once|仅允许本次|允许本次)$")
        );
        assert!(
            !APPROVE_ONCE_SCRIPT
                .to_ascii_lowercase()
                .contains("always allow")
        );
    }

    #[test]
    fn profile_script_is_fail_closed_when_picker_option_is_missing() {
        assert!(ENSURE_EXECUTION_PROFILE_SCRIPT.contains("if (!option) return observed()"));
    }
}
