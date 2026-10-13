mod desktop;
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use fabushi_chatgpt_application::BrowserPort;
use fabushi_chatgpt_domain::PageSnapshot;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};

#[derive(Debug, Clone, Deserialize)]
struct TargetInfo {
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
    desktop: bool,
}

impl ChatGptCdp {
    pub async fn connect(endpoint: &str) -> Result<Self> {
        Self::connect_mode(endpoint, false).await
    }

    pub async fn connect_desktop(endpoint: &str) -> Result<Self> {
        let url = reqwest::Url::parse(endpoint)?;
        if !matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")) {
            bail!("desktop CDP must use a loopback endpoint");
        }
        Self::connect_mode(endpoint, true).await
    }

    async fn connect_mode(endpoint: &str, desktop: bool) -> Result<Self> {
        let endpoint = endpoint.trim_end_matches('/').to_owned();
        let targets: Vec<TargetInfo> = reqwest::get(format!("{endpoint}/json/list"))
            .await
            .context("failed to reach Chromium remote debugging endpoint")?
            .error_for_status()?
            .json()
            .await
            .context("invalid /json/list response")?;

        let target = if desktop {
            select_desktop_target(&targets)
        } else {
            select_chatgpt_target(&targets)
        }
        .ok_or_else(|| {
            anyhow!(if desktop {
                "no desktop ChatGPT main renderer found; run desktop first"
            } else {
                "no ChatGPT page target found; open https://chatgpt.com first"
            })
        })?;
        let ws_url = target
            .websocket_debugger_url
            .clone()
            .ok_or_else(|| anyhow!("selected ChatGPT target has no websocket debugger URL"))?;
        let (socket, _) = connect_async_with_config(&ws_url, Some(cdp_socket_config()), false)
            .await
            .with_context(|| format!("failed to connect CDP websocket {ws_url}"))?;

        Ok(Self {
            endpoint,
            socket: Mutex::new(socket),
            next_id: AtomicU64::new(1),
            desktop,
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    async fn command(&self, method: &str, params: Value) -> Result<Value> {
        tokio::time::timeout(
            std::time::Duration::from_secs(60),
            self.command_inner(method, params),
        )
        .await
        .context("desktop CDP command timed out")?
    }
    async fn command_inner(&self, method: &str, params: Value) -> Result<Value> {
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
        if self.desktop {
            let surface = self.surface().await?;
            if surface["chat_mode"] != true || surface["composer"] != true {
                bail!("desktop Chat composer is not ready; run new-chat first: {surface}");
            }
        }
        let encoded = serde_json::to_string(prompt)?;
        let script = SEND_PROMPT_SCRIPT.replace("__PROMPT_JSON__", &encoded);
        let value = self.evaluate(&script).await?;
        match value.get("ok").and_then(Value::as_bool) {
            Some(true) => Ok(()),
            _ => bail!("ChatGPT prompt dispatch failed: {value}"),
        }
    }

    pub async fn surface(&self) -> Result<Value> {
        self.evaluate(SURFACE_SCRIPT).await
    }

    pub async fn new_desktop_chat(&self) -> Result<()> {
        if !self.desktop {
            bail!("new-chat requires the desktop backend");
        }
        let value = self.evaluate(NEW_CHAT_SCRIPT).await?;
        if value["ok"] != true {
            bail!("desktop Chat could not be opened: {value}");
        }
        Ok(())
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
        if self.desktop {
            bail!("desktop renderer navigation is disabled; use new-chat in the client");
        }
        self.command("Page.navigate", json!({"url": url})).await?;
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
}

fn cdp_socket_config() -> WebSocketConfig {
    // A 50 MiB attachment batch becomes ~67 MiB of base64 in one CDP frame.
    WebSocketConfig::default()
        .max_message_size(Some(96 * 1024 * 1024))
        .max_frame_size(Some(96 * 1024 * 1024))
}

fn select_desktop_target(targets: &[TargetInfo]) -> Option<&TargetInfo> {
    // Auxiliary page and avatar-overlay renderers are never dispatch targets.
    targets.iter().find(|target| {
        target.kind == "page"
            && target.websocket_debugger_url.is_some()
            && target.url == "app://-/index.html"
    })
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
  const aria = (el) => norm(el?.getAttribute?.('aria-label') || el?.getAttribute?.('title') || '');
  const testid = (el) => norm(el?.getAttribute?.('data-testid') || '');
  const buttons = [...document.querySelectorAll('button')].filter(e => e.getClientRects().length > 0);
  const isStop = (b) => /(^|\b)(stop|停止|停止生成|停止回答)(\b|$)/i.test([textOf(b), aria(b), testid(b)].join(' '));
  const isCopy = (b) => /(copy|复制)/i.test([textOf(b), aria(b), testid(b)].join(' '));
  const isApproval = (b) => /(allow once|允许一次|approve once|仅允许本次|允许本次)/i.test([textOf(b), aria(b)].join(' '));

  const messageSelector = '[data-message-author-role], [data-content-search-unit-key], [data-user-message-bubble]';
  const mains = [...document.querySelectorAll('main')].filter(e => e.getClientRects().length).reverse();
  const main = mains.find(e => e.querySelector(messageSelector)) || mains.find(e => e.querySelector('textarea, [contenteditable="true"]')) || document.body;
  const preferred = (selectors) => {
    for (const selector of selectors) {
      const nodes = [...main.querySelectorAll(selector)];
      if (nodes.length) return nodes;
    }
    return [];
  };
  const userTurns = preferred(['[data-message-author-role="user"]', '[data-content-search-unit-key$=":user"]', '[data-user-message-bubble]']);
  const assistantTurns = preferred(['[data-message-author-role="assistant"]', '[data-content-search-unit-key$=":assistant"]', '[data-local-conversation-final-assistant]']);
  const lastAssistant = assistantTurns.at(-1) || null;
  const responseTurn = lastAssistant?.closest('article, [data-testid^="conversation-turn-"], [data-content-search-turn-key], [data-turn-key], [data-local-conversation-final-assistant]') || lastAssistant;
  const lastAssistantButtons = responseTurn ? [...responseTurn.querySelectorAll('button')] : [];
  const copyAvailable = lastAssistantButtons.some(isCopy);
  const actionButtons = lastAssistantButtons.filter((b) => {
    const s = [textOf(b), aria(b), testid(b)].join(' ');
    return /(copy|复制|good|bad|like|dislike|regenerate|retry|branch|read aloud|朗读|分享|share|rate response|评价回复|更多|more actions)/i.test(s);
  });

  const lastUser = userTurns.at(-1);
  const lastAssistantAfterLastUser = !!lastAssistant && !!lastUser &&
    (lastUser.compareDocumentPosition(lastAssistant) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0;

  const composer = [...document.querySelectorAll('#prompt-textarea, textarea, [contenteditable="true"][data-lexical-editor="true"], [contenteditable="true"]')].find(e => e.getClientRects().length > 0);
  const composerText = norm(composer?.value ?? composer?.innerText ?? composer?.textContent ?? '');

  return {
    url: (() => {
      if (location.protocol !== 'app:') return location.href;
      const raw = document.querySelector('[data-above-composer-conversation-id]')?.getAttribute('data-above-composer-conversation-id') || '';
      const id = raw.replace(/^chatgpt:/, '');
      return /^[a-zA-Z0-9-]+$/.test(id) ? `https://chatgpt.com/c/${id}` : location.href;
    })(),
    title: document.title,
    user_turns: userTurns.length,
    assistant_turns: assistantTurns.length,
    stop_available: buttons.some(isStop) || !!lastAssistant?.matches('[data-is-streaming="true"], [aria-busy="true"]') || !!responseTurn?.querySelector('[data-is-streaming="true"], [aria-busy="true"]'),
    waiting_for_approval: buttons.some(isApproval),
    rate_limit_notice: [...document.querySelectorAll('[role="alert"], [role="status"], [role="dialog"]')].filter(e => e.getClientRects().length && !e.closest('[data-message-author-role="user"], [data-content-search-unit-key$=":user"], blockquote, pre, code')).some(e => /(too many requests|request.*frequent|请求过于频繁|请求太频繁)/i.test(e.innerText || '')),
    copy_available_on_last_assistant: copyAvailable,
    response_actions_complete: copyAvailable && (actionButtons.some(b => !isCopy(b)) || lastAssistant?.getAttribute('data-is-streaming') === 'false'),
    response_action_turn_bound_to_last: lastAssistantAfterLastUser && assistantTurns.length >= userTurns.length,
    awaiting_assistant: userTurns.length > assistantTurns.length,
    assistant_text: norm(lastAssistant?.innerText || lastAssistant?.textContent || ''),
    composer_text: composerText,
  };
})()
"#;

const SEND_PROMPT_SCRIPT: &str = r#"
(async () => {
  const prompt = __PROMPT_JSON__;
  const norm = (v) => String(v ?? '').replace(/\s+/g, ' ').trim();
  const composer = [...document.querySelectorAll('#prompt-textarea, textarea, [contenteditable="true"][data-lexical-editor="true"], [contenteditable="true"]')].find(e => e.getClientRects().length > 0);
  if (!composer) return {ok:false, reason:'composer-not-found'};

  const existing = norm(composer.value ?? composer.innerText ?? composer.textContent ?? '');
  if (existing && existing !== norm(prompt)) return {ok:false, reason:'composer-has-existing-draft'};
  composer.focus();
  if ('value' in composer) {
    const proto = Object.getPrototypeOf(composer);
    const descriptor = Object.getOwnPropertyDescriptor(proto, 'value');
    if (descriptor?.set) descriptor.set.call(composer, prompt);
    else composer.value = prompt;
    composer.dispatchEvent(new Event('input', {bubbles:true}));
    composer.dispatchEvent(new Event('change', {bubbles:true}));
  } else {
    const selection = window.getSelection();
    const range = document.createRange();
    range.selectNodeContents(composer);
    selection?.removeAllRanges();
    selection?.addRange(range);
    if (!document.execCommand('insertText', false, prompt)) composer.textContent = prompt;
    composer.dispatchEvent(new InputEvent('input', {bubbles:true, inputType:'insertText', data:prompt}));
  }

  await new Promise(resolve => setTimeout(resolve, 100));
  const scope = composer.closest('form') || composer.parentElement?.parentElement || document;
  const send = scope.querySelector('button[data-testid="send-button"]:not(:disabled)') || [...scope.querySelectorAll('button')].find((b) => {
    const label = norm(b.getAttribute('aria-label') || b.innerText || '');
    return /^(send|send prompt|发送|发送提示)$/i.test(label) && !b.disabled;
  });

  if (!send) return {ok:false, reason:'send-button-not-found'};
  send.click();
  return {ok:true};
})()
"#;

const APPROVE_ONCE_SCRIPT: &str = r#"
(() => {
  const norm = (v) => String(v ?? '').replace(/\s+/g, ' ').trim();
  const buttons = [...document.querySelectorAll('button')].filter(e => e.getClientRects().length > 0);
  const exact = buttons.find((b) => !b.disabled && b.getClientRects().length > 0 && /^(allow once|允许一次|approve once|仅允许本次|允许本次)$/i.test(norm(b.innerText || b.textContent || b.getAttribute('aria-label'))));
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

const SURFACE_SCRIPT: &str = r#"
(() => {
  const visible = e => !!(e && e.getClientRects().length);
  const buttons = [...document.querySelectorAll('button')].filter(visible);
  const composer = [...document.querySelectorAll('#prompt-textarea, textarea, [contenteditable="true"]')].find(visible);
  return {
    url: location.href,
    title: document.title,
    desktop: location.protocol === 'app:',
    composer: visible(composer),
    workbench: (()=>{const b=window.__fabushiWorkbench;return {entry_present:!!b?.host?.isConnected,panel_open:!!b?.root?.getElementById("panel")?.classList.contains("open"),version:b?.version || 0,task_count:b?.root?.querySelectorAll(".task-row").length || 0,updated_at:b?.updated || 0,unsaved:!!(b?.root?.getElementById("goal")?.value.trim() || b?.files?.length)};})(),
    conversation_identity: document.querySelector('[data-above-composer-conversation-id]')?.getAttribute('data-above-composer-conversation-id') || document.querySelector('[data-chatgpt-selection-conversation-id]')?.getAttribute('data-chatgpt-selection-conversation-id') || null,
    inputs: [...document.querySelectorAll('textarea, [contenteditable="true"]')].filter(visible).map(e => ({tag:e.tagName, id:e.id, role:e.getAttribute('role'), placeholder:e.getAttribute('data-placeholder') || e.getAttribute('placeholder')})),
    work_composer: [...document.querySelectorAll('[data-codex-composer="true"]')].some(visible),
    chat_mode: visible(composer) && ![...document.querySelectorAll('[data-codex-composer="true"]')].some(visible),
    mode_controls: buttons.map(b => ({label:(b.getAttribute('aria-label') || b.innerText || '').trim(), role:b.getAttribute('role')})).filter(b => /chatgpt|codex|chat|work|模式|聊天|工作/i.test(b.label)).filter(b => !/置顶|归档|聊天操作|项目操作|在 .*中/.test(b.label)).slice(-30),
    menus: [...document.querySelectorAll('[role="menu"], [role="listbox"], [role="dialog"]')].filter(visible).map(e => e.innerText.slice(0, 1200)),
    load_controls: [...document.querySelectorAll("p, span, div, button, a")].filter(e=>visible(e) && /^(?:无法加载.*(?:会话|对话)|unable to load.*conversation|重试|retry)$/i.test((e.innerText || "").trim())).map(e=>({tag:e.tagName,role:e.getAttribute("role"),html:e.parentElement?.outerHTML.slice(0,4000)})).slice(-8),
    approval_controls: buttons.filter(b=>/allow|approve|允许|reject|拒绝/i.test(b.innerText || b.getAttribute('aria-label') || '')).map(b=>({ancestors:(()=>{const out=[];for(let e=b,n=0;e && n<12;e=e.parentElement,n++)out.push({tag:e.tagName,role:e.getAttribute('role'),unit:e.getAttribute('data-content-search-unit-key'),author:e.getAttribute('data-message-author-role')});return out;})(),label:b.innerText,aria_label:b.getAttribute('aria-label'),haspopup:b.getAttribute('aria-haspopup'),tag:b.tagName,role:b.getAttribute('role'),parent:b.parentElement?.outerHTML.slice(0,5000)})).slice(-12),
    controls: buttons.map(b => (b.getAttribute('aria-label') || b.innerText || '').trim()).filter(Boolean).slice(0, 60)
  };
})()
"#;

const NEW_CHAT_SCRIPT: &str = r#"
(async () => {
  const visible = e => !!(e && e.getClientRects().length);
  const norm = v => String(v || '').replace(/\s+/g, ' ').trim();
  const pointerClick = b => {
    const rect = b.getBoundingClientRect();
    const event = {bubbles:true, cancelable:true, composed:true, button:0, buttons:1, clientX:rect.x + rect.width / 2, clientY:rect.y + rect.height / 2};
    b.dispatchEvent(new PointerEvent('pointerdown', {...event, pointerId:1, pointerType:'mouse', isPrimary:true}));
    b.dispatchEvent(new MouseEvent('mousedown', event));
    b.dispatchEvent(new PointerEvent('pointerup', {...event, buttons:0, pointerId:1, pointerType:'mouse', isPrimary:true}));
    b.dispatchEvent(new MouseEvent('mouseup', {...event, buttons:0}));
    b.dispatchEvent(new MouseEvent('click', {...event, buttons:0}));
  };
  const switcher = [...document.querySelectorAll('button')].filter(visible).find(b => /current mode|当前模式/i.test(b.getAttribute('aria-label') || ''));
  if (switcher && !/chatgpt/i.test(switcher.getAttribute('aria-label') || '')) {
    if (switcher.getAttribute('aria-expanded') !== 'true' && switcher.getAttribute('data-state') !== 'open') pointerClick(switcher);
    await new Promise(r => setTimeout(r, 150));
    const options = [...document.querySelectorAll('[role="menuitem"], [role="option"], [role="menuitemradio"]')].filter(visible);
    const chat = options.find(b => /^(ChatGPT|Chat|聊天)$/i.test(norm(b.innerText || b.getAttribute('aria-label'))) || [...b.querySelectorAll('*')].some(c => !c.children.length && norm(c.textContent) === 'ChatGPT'));
    if (!chat) return {ok:false, reason:'chatgpt-mode-option-not-found', options:options.map(b => norm(b.innerText))};
    pointerClick(chat);
    await new Promise(r => setTimeout(r, 300));
  }
  const labels = ['New chat', '新聊天', '新建聊天', 'New Chat'];
  const buttons = [...document.querySelectorAll('button, a, [role="button"]')].filter(visible);
  const button = buttons.find(b => labels.includes(norm(b.getAttribute('aria-label') || b.innerText || b.getAttribute('title'))));
  if (!button) return {ok:false, reason:'new-chat-control-not-found'};
  pointerClick(button);
  await new Promise(r => setTimeout(r, 300));
  if ([...document.querySelectorAll('[data-codex-composer="true"]')].some(visible)) {
    const chatTab = [...document.querySelectorAll('button, [role="tab"]')].filter(visible).find(b => /^(Chat|聊天)$/i.test(norm(b.getAttribute('aria-label') || b.innerText)));
    if (!chatTab) return {ok:false, reason:'chat-tab-not-found'};
    pointerClick(chatTab);
  }
  for (let i = 0; i < 40; i++) {
    const composer = [...document.querySelectorAll('#prompt-textarea, textarea, [contenteditable="true"]')].find(visible);
    if (visible(composer) && ![...document.querySelectorAll('[data-codex-composer="true"]')].some(visible)) return {ok:true};
    await new Promise(r => setTimeout(r, 100));
  }
  return {ok:false, reason:'desktop-chat-composer-not-ready'};
})()
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_batch_fits_bounded_cdp_frame() {
        let bytes = 50usize * 1024 * 1024;
        let envelope = bytes.div_ceil(3) * 4 + 1024 * 1024;
        let config = cdp_socket_config();
        assert!(config.max_frame_size.unwrap() >= envelope);
        assert!(config.max_message_size.unwrap() >= envelope);
        assert!(config.max_message_size.unwrap() <= 100 * 1024 * 1024);
    }

    #[test]
    fn desktop_target_excludes_browser_overlay_and_auxiliary_windows() {
        let make = |url: &str| TargetInfo {
            kind: "page".into(),
            title: "ChatGPT".into(),
            url: url.into(),
            websocket_debugger_url: Some("ws://localhost/1".into()),
        };
        let targets = vec![
            make("https://chatgpt.com/"),
            make("app://-/index.html?initialRoute=%2Favatar-overlay"),
            make("app://-/index.html?initialRoute=%2Fspace%2Fpage"),
            make("app://-/index.html"),
        ];
        assert_eq!(
            select_desktop_target(&targets).unwrap().url,
            "app://-/index.html"
        );
        assert!(select_desktop_target(&targets[..3]).is_none());
    }

    #[test]
    fn target_prefers_chatgpt_page() {
        let targets = vec![
            TargetInfo {
                kind: "page".into(),
                title: "Other".into(),
                url: "https://example.com".into(),
                websocket_debugger_url: Some("ws://localhost/1".into()),
            },
            TargetInfo {
                kind: "page".into(),
                title: "ChatGPT".into(),
                url: "https://chatgpt.com/c/abc".into(),
                websocket_debugger_url: Some("ws://localhost/2".into()),
            },
        ];
        assert_eq!(
            select_chatgpt_target(&targets).unwrap().url,
            "https://chatgpt.com/c/abc"
        );
    }
}
