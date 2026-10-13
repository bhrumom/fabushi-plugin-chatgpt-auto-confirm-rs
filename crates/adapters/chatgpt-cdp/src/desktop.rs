use super::ChatGptCdp;
use anyhow::{Result, bail};
use async_trait::async_trait;
use fabushi_chatgpt_application::{NavigationUnavailable, TaskDesktop};
use fabushi_chatgpt_domain::*;
use serde_json::{Value, json};

impl ChatGptCdp {
    pub async fn profile_renderer(&self) -> Result<Value> {
        use std::collections::HashMap;
        self.command("Profiler.enable", serde_json::json!({}))
            .await?;
        self.command("Profiler.start", serde_json::json!({}))
            .await?;
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let result = self.command("Profiler.stop", serde_json::json!({})).await?;
        let mut counts = HashMap::<u64, usize>::new();
        if let Some(samples) = result["profile"]["samples"].as_array() {
            for sample in samples {
                if let Some(id) = sample.as_u64() {
                    *counts.entry(id).or_default() += 1;
                }
            }
        }
        let mut rows = vec![];
        if let Some(nodes) = result["profile"]["nodes"].as_array() {
            for node in nodes {
                let id = node["id"].as_u64().unwrap_or(0);
                let count = counts.get(&id).copied().unwrap_or(0);
                if count > 0 {
                    rows.push((count,serde_json::json!({"samples":count,"function":node["callFrame"]["functionName"],"script":node["callFrame"]["scriptId"],"line":node["callFrame"]["lineNumber"],"url":node["callFrame"]["url"].as_str().unwrap_or("").split('?').next().unwrap_or("")})));
                }
            }
        }
        rows.sort_by_key(|row| std::cmp::Reverse(row.0));
        Ok(serde_json::json!(
            rows.into_iter()
                .take(15)
                .map(|row| row.1)
                .collect::<Vec<_>>()
        ))
    }
    pub async fn suspend_stalled_workbench(&self) -> Result<Value> {
        self.command("Runtime.terminateExecution", serde_json::json!({}))
            .await?;
        self.evaluate(r#"(()=>{const b=window.__fabushiWorkbench;if(b){b.observer?.disconnect();clearInterval(b.timer);clearTimeout(b.mountRepairTimer);b.host.remove();}const composer=document.querySelector('#prompt-textarea,textarea,[contenteditable="true"]');return {suspended:!!b,unsaved:!!(b?.root?.getElementById('goal')?.value.trim()||b?.files?.length),native_draft:!!(composer?.value||composer?.textContent||'').trim()};})()"#).await
    }
    pub async fn workbench_probe(&self) -> Result<Value> {
        self.evaluate(r#"(()=>{const b=window.__fabushiWorkbench;const composer=document.querySelector('#prompt-textarea,textarea,[contenteditable="true"]');return {version:b?.version||0,task_count:b?.snapshot?.tasks?.length||0,rendered_rows:b?.root?.querySelectorAll(".task-row").length||0,mounted:!!b?.host?.isConnected,open:!!b?.root?.getElementById('panel')?.classList.contains('open'),pending:!!b?.commands?.length,unsaved:!!(b?.root?.getElementById('goal')?.value.trim()||b?.files?.length),native_draft:!!(composer?.value||composer?.textContent||'').trim()};})()"#).await
    }
    pub async fn workbench_panel(&self, snapshot: &Value) -> Result<Value> {
        let script = include_str!("workbench-panel.js")
            .replace("__PANEL_CSS__", include_str!("workbench-panel.css"))
            .replace("__SNAPSHOT_JSON__", &serde_json::to_string(snapshot)?);
        self.evaluate(&script).await
    }
    pub async fn conversations(&self) -> Result<Value> {
        self.evaluate(CONVERSATIONS_SCRIPT).await
    }
    pub async fn task_observation(&self, token: &str) -> Result<TaskObservation> {
        self.pending_task_observation(token, &[]).await
    }
    async fn pending_task_observation(
        &self,
        token: &str,
        receipts: &[String],
    ) -> Result<TaskObservation> {
        let context = CONTEXT_SCRIPT
            .replace("__APPROVAL_SCAN__", include_str!("approval-scan.js"))
            .replace("__APPROVAL_RECEIPTS__", &serde_json::to_string(receipts)?)
            .replace(
                "__TOKEN__",
                &serde_json::to_string(&format!("[Fabushi:{token}]"))?,
            );
        let script = format!(
            "(() => {{ const page = {}; const context = {}; return {{ page, ...context }}; }})()",
            super::SNAPSHOT_SCRIPT,
            context
        );
        Ok(serde_json::from_value(self.evaluate(&script).await?)?)
    }
    pub async fn select_profile(&self, profile: &ExecutionProfile) -> Result<()> {
        let script = PROFILE_SCRIPT.replace("__PROFILE__", &serde_json::to_string(profile)?);
        let result = self.evaluate(&script).await?;
        if result["ok"] != true {
            bail!("模型/思考强度未确认，保留发送意图: {result}");
        }
        Ok(())
    }
    pub async fn switch_conversation(&self, binding: &SessionBinding) -> Result<()> {
        let script = SWITCH_SCRIPT.replace("__BINDING__", &serde_json::to_string(binding)?);
        let result = self.evaluate(&script).await?;
        if result["ok"] == true {
            return Ok(());
        }
        if result["needs_click"] == true {
            let x = result["x"]
                .as_f64()
                .ok_or_else(|| anyhow::anyhow!("row x missing"))?;
            let y = result["y"]
                .as_f64()
                .ok_or_else(|| anyhow::anyhow!("row y missing"))?;
            self.command(
                "Input.dispatchMouseEvent",
                json!({"type":"mousePressed","x":x,"y":y,"button":"left","clickCount":1}),
            )
            .await?;
            self.command(
                "Input.dispatchMouseEvent",
                json!({"type":"mouseReleased","x":x,"y":y,"button":"left","clickCount":1}),
            )
            .await?;
            for _ in 0..30 {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                let observation = self.task_observation("").await?;
                if observation.binding.sidebar_id == binding.sidebar_id
                    && (observation.page.user_turns > 0
                        || observation.page.assistant_turns > 0
                        || observation.load_failed)
                {
                    return Ok(());
                }
            }
        }
        if result["reason"] == "recorded-conversation-not-in-sidebar" {
            return Err(NavigationUnavailable(result.to_string()).into());
        }
        bail!("desktop conversation switch failed: {result}");
    }
    pub async fn upload_files(&self, attachments: &[Attachment]) -> Result<()> {
        if attachments.is_empty() {
            return Ok(());
        }
        for file in attachments {
            let data = std::fs::read(&file.path)?;
            if text_hash_bytes(&data) != file.sha256 {
                bail!("附件内容已改变: {}", file.name);
            }
        }
        let result = self
            .command("DOM.getDocument", json!({"depth":-1,"pierce":true}))
            .await?;
        let root = result["root"]["nodeId"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("desktop DOM root unavailable"))?;
        let found = self
            .command(
                "DOM.querySelector",
                json!({"nodeId":root,"selector":"main form input[type=file], main input[type=file][data-testid*=upload], input[type=file][aria-label=\"Upload files\"], input[type=file][aria-label=\"上传文件\"]"}),
            )
            .await?;
        let node = found["nodeId"]
            .as_u64()
            .filter(|v| *v > 0)
            .ok_or_else(|| anyhow::anyhow!("附件输入控件尚未加载"))?;
        self.command(
            "DOM.setFileInputFiles",
            json!({"nodeId":node,"files":attachments.iter().map(|f|&f.path).collect::<Vec<_>>()}),
        )
        .await?;
        let script = UPLOAD_READY_SCRIPT.replace("__FILES__", &serde_json::to_string(attachments)?);
        if self.evaluate(&script).await?["ok"] != true {
            bail!("附件未完成上传，等待后重试，不发送任务");
        }
        Ok(())
    }
    pub async fn clear_saved_draft(&self, expected: &str) -> Result<()> {
        let script = CLEAR_DRAFT_SCRIPT.replace("__EXPECTED__", &serde_json::to_string(expected)?);
        let result = self.evaluate(&script).await?;
        if result["ok"] != true {
            bail!("草稿已变化或未找到输入框，保留当前内容：{result}");
        }
        Ok(())
    }
    pub async fn stop_response(&self) -> Result<bool> {
        Ok(self.evaluate(STOP_SCRIPT).await?["clicked"] == true)
    }
    pub async fn retry_loading_conversation(&self) -> Result<bool> {
        Ok(self.evaluate(RETRY_LOAD_SCRIPT).await?["clicked"] == true)
    }
    pub async fn approve_current_session(&self) -> Result<Value> {
        self.evaluate(
            &SCOPED_APPROVE_SCRIPT
                .replace("__APPROVAL_SCAN__", include_str!("approval-scan.js"))
                .replace("__APPROVAL_TARGET__", r#""""#),
        )
        .await
    }
    pub async fn dismiss_safe_modals(&self) -> Result<bool> {
        Ok(self.evaluate(SAFE_MODALS_SCRIPT).await?["clicked"] == true)
    }
}
fn text_hash_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}
#[async_trait]
impl TaskDesktop for ChatGptCdp {
    async fn retry_load(&self) -> Result<bool> {
        self.retry_loading_conversation().await
    }
    async fn new_chat(&self) -> Result<()> {
        self.new_desktop_chat().await
    }
    async fn activate(&self, b: &SessionBinding, token: &str) -> Result<()> {
        let current = self.task_observation(token).await?;
        // A transient id may resolve while the dispatched conversation stays
        // open. The exact owned token is sufficient; never search other chats.
        if current.marker_present
            && !current.foreign_marker
            && (current.binding.sidebar_id == b.sidebar_id
                || b.sidebar_id.starts_with("local-chatgpt:")
                || current.binding.sidebar_id.is_empty()
                || current.binding.sidebar_id.starts_with("local-chatgpt:"))
        {
            return Ok(());
        }
        match self.switch_conversation(b).await {
            Ok(()) => {
                for _ in 0..32 {
                    let o = self.task_observation(token).await?;
                    if o.binding.sidebar_id == b.sidebar_id
                        && ((o.marker_present && !o.foreign_marker) || o.load_failed)
                    {
                        return Ok(());
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
                let o = self.task_observation(token).await?;
                if o.binding.sidebar_id == b.sidebar_id {
                    return Ok(());
                }
                bail!("侧栏与内容归属未确认；未推进其他会话")
            }
            Err(error) => Err(error),
        }
    }
    async fn observe(&self, token: &str) -> Result<TaskObservation> {
        self.task_observation(token).await
    }
    async fn ensure_profile(&self, p: &ExecutionProfile) -> Result<()> {
        self.select_profile(p).await
    }
    async fn upload(&self, files: &[Attachment]) -> Result<()> {
        self.upload_files(files).await
    }
    async fn dispatch(&self, prompt: &str) -> Result<()> {
        self.send_prompt(prompt).await
    }
    async fn observe_pending(&self, token: &str, receipts: &[String]) -> Result<TaskObservation> {
        self.pending_task_observation(token, receipts).await
    }
    async fn approve(&self, request: &str) -> Result<bool> {
        let script = SCOPED_APPROVE_SCRIPT
            .replace("__APPROVAL_SCAN__", include_str!("approval-scan.js"))
            .replace("__APPROVAL_TARGET__", &serde_json::to_string(request)?);
        Ok(self.evaluate(&script).await?["clicked"] == true)
    }
    async fn stay_chat(&self) -> Result<bool> {
        Ok(self.evaluate(STAY_CHAT_SCRIPT).await?["clicked"] == true)
    }
    async fn reload(&self) -> Result<()> {
        ChatGptCdp::reload(self).await
    }
}

const CONVERSATIONS_SCRIPT: &str = r#"
(() => {
  const rows = [...document.querySelectorAll('[data-thread-title="true"]')].map(title => {
    const row=title.closest('[role="button"], a');
    if (!row) return null;
    const ids=[];
    const href=row.getAttribute('href') || '';
    const match=href.match(/\/(?:c|work\/conversation)\/([^/?#]+)/);
    if(match) ids.push(decodeURIComponent(match[1]));
    const key=Object.keys(row).find(key=>key.startsWith('__reactFiber$'));
    let fiber=key?row[key]:null;
    for(let i=0;fiber && i<10;i++,fiber=fiber.return) {
      const p=fiber.memoizedProps || {};
      for(const value of [p.conversation?.id,p.conversationId,p.thread?.id,p.threadId]) if(typeof value==='string') ids.push(value);
    }
    return {ids:[...new Set(ids)],title:(title.innerText || title.textContent || '').trim(),current:row.getAttribute('aria-current')==='page'};
  }).filter(Boolean);
  return rows;
})()
"#;
const CONTEXT_SCRIPT: &str = r#"
(() => {
  const marker=__TOKEN__;
  const visible=e=>!!(e && e.getClientRects().length);
  const normalize=v=>String(v || '').replace(/\s+/g,' ').trim();
  const mains=[...document.querySelectorAll('main')].filter(visible).reverse();
  const main=mains.find(e=>e.querySelector('[data-content-search-unit-key], [data-message-author-role]')) || mains[0] || document.body;
  const userNodes=[...main.querySelectorAll('[data-message-author-role="user"], [data-content-search-unit-key$=":user"]')];
  const users=userNodes.length?userNodes:[...main.querySelectorAll('[data-user-message-bubble]')];
  const last=users.at(-1);
  const data=(object,key)=>{const descriptor=Object.getOwnPropertyDescriptor(object || {},key);return descriptor && 'value' in descriptor?descriptor.value:undefined;};
  const userText=node=>{
    if(!node)return '';
    const direct=node.textContent || '';
    if(/\[Fabushi:[^\]]+\]/.test(direct))return direct;
    const texts=[direct];
    const add=value=>{if(typeof value==='string')texts.push(value);else if(Array.isArray(value)){for(const part of value){if(typeof part==='string')texts.push(part);else if(typeof data(part,'text')==='string')texts.push(data(part,'text'));}}};
    const key=Object.keys(node).find(k=>k.startsWith('__reactFiber$'));let fiber=key?data(node,key):null;
    for(let i=0;fiber && i<10;i++,fiber=data(fiber,'return')){
      const props=data(fiber,'memoizedProps') || {};
      const message=data(props,'message'),content=data(message,'content'),user=data(props,'userMessage');
      add(data(props,'text'));add(data(props,'content'));add(data(message,'text'));add(content);add(data(content,'parts'));add(data(message,'parts'));add(data(user,'text'));add(data(user,'content'));
    }
    return texts.sort((a,b)=>b.length-a.length)[0] || '';
  };
  const fullUserText=normalize(userText(last));
  const lastText=fullUserText.slice(-8000);

  const raw=[...document.querySelectorAll('[data-above-composer-conversation-id]')].find(visible)?.getAttribute('data-above-composer-conversation-id') || document.querySelector('[data-chatgpt-selection-conversation-id]')?.getAttribute('data-chatgpt-selection-conversation-id') || '';
  const nativeIdentity=raw.replace(/^chatgpt:/,'');
  const titleNodes=[...document.querySelectorAll('[data-thread-title="true"]')];
  let sidebarId='';
  for(const title of titleNodes) {
    const row=title.closest('[role="button"], a');
    if(row?.getAttribute('aria-current')!=='page') continue;
    const key=Object.keys(row).find(key=>key.startsWith('__reactFiber$'));let fiber=key?row[key]:null;
    for(let i=0;fiber && i<10;i++,fiber=fiber.return) {
      const p=fiber.memoizedProps || {};const id=p.conversation?.id || p.conversationId || p.thread?.id || p.threadId;
      if(typeof id==='string' && !sidebarId) sidebarId=id;
    }
    const match=row.getAttribute('href')?.match(/\/(?:c|work\/conversation)\/([^/?#]+)/);
    if(!sidebarId && match) sidebarId=decodeURIComponent(match[1]);
  }
  const identity=sidebarId || nativeIdentity;
  // Structural metadata is primary. Text fallback requires card topology.
  const excluded='[data-message-author-role="user"], [data-content-search-unit-key$=":user"], blockquote, pre, code, nav, aside';
  const approvalLabel=b=>{const c=b.cloneNode(true);c.querySelectorAll('kbd, [aria-hidden="true"]').forEach(n=>n.remove());return normalize(b.getAttribute('aria-label') || c.textContent).replace(/\s+(?:Esc|Enter|↵|⏎|⌄|▾)$/i,'').trim();};
  __APPROVAL_SCAN__
  const receipts=__APPROVAL_RECEIPTS__;
  const pending=approvalScan().filter(c=>!c.resolved && !receipts.includes(c.key));
  const approval=pending[0];
  page.waiting_for_approval=pending.length>0;
  const statusNodes=[...main.querySelectorAll('[role="alert"], [role="status"], [data-testid*="error"]')].filter(e=>visible(e) && !e.closest('blockquote, pre, code, [data-content-search-unit-key$=":user"]'));
  const statusText=statusNodes.map(e=>normalize(e.innerText)).join(' ');
  const latest=[...main.querySelectorAll('[data-content-search-unit-key$=":assistant"], [data-message-author-role="assistant"], [data-local-conversation-final-assistant]')].at(-1);
  const interruption=/^(?:connection (?:was )?(?:interrupted|lost)|连接(?:已)?中断|连接断开)(?:[，。,.：:]?\s*(?:waiting for (?:the )?(?:complete|full) answer|正在等待完整(?:答复|回复)))?[。.!]?$/i;
  const turn=latest?.closest('article, [data-content-search-turn-key]') || latest;
  const interruptionNodes=[...(turn || main).querySelectorAll('p, span, div')].filter(e=>e.childElementCount===0 && !e.closest('blockquote, pre, code, [data-content-search-unit-key$=":user"]'));
  const paragraphs=[...(turn || main).querySelectorAll('p, [role="alert"], [role="status"]')].filter(e=>!e.closest('blockquote, pre, code, [data-content-search-unit-key$=":user"]') && !e.querySelector('blockquote,pre,code'));
  const latestLines=[...paragraphs,...interruptionNodes].map(e=>e.textContent || '').filter(t=>t.length<500).map(normalize);
  const interrupted=latestLines.some(l=>interruption.test(l)) || /connection (?:was )?(?:interrupted|lost)|连接(?:已)?中断|连接断开/i.test(statusText);
  const publicTurns=[...main.querySelectorAll('[data-content-search-unit-key$=":assistant"], [data-message-author-role="assistant"]')].filter(visible).slice(-3);
  const visible_work=publicTurns.map(e=>(e.closest('article, [data-content-search-turn-key]') || e).innerText || '').join('\n\n').slice(-64000);
  const length_limit=/reached.*(?:conversation|chat).*limit|此对话.*长度上限|达到.*对话.*长度上限/i.test(statusText) || latestLines.some(l=>/^(you.*reached.*(?:conversation|chat).*limit.*|你.*(?:达到|已达到).*对话.*长度上限.*)$/i.test(l));
  const loadLabel=e=>{const c=e.cloneNode(true);c.querySelectorAll('button, [role="button"], a').forEach(n=>n.remove());return normalize(c.textContent);};
  const loadError=/^(?:unable to load (?:this )?(?:ChatGPT )?conversation|无法加载.*(?:会话|对话))[。.!]?$/i;
  const retries=[...document.querySelectorAll('button, [role="button"], a')].filter(e=>/^(?:重试|重载|重新加载|retry|reload|try again)$/i.test(normalize(e.getAttribute('aria-label') || e.textContent)) && visible(e) && !e.closest(excluded));
  const load_failed=/unable to load (?:this )?(?:ChatGPT )?conversation|无法加载.*(?:会话|对话)/i.test(statusText) || retries.some(button=>{
    for(let card=button.parentElement,n=0;card && n<5 && !card.matches('main,body');card=card.parentElement,n++){
      const text=card.textContent || '';
      if(text.length<250 && !card.closest(excluded) && !card.querySelector(excluded) && loadError.test(loadLabel(card)))return true;
    }return false;
  });

  const stayButtons=[...document.querySelectorAll('button, [role="button"]')].filter(e=>visible(e) && !e.disabled && !e.closest(excluded) && /^(?:留在聊天(?:模式|中)|stay in chat)(?:\s*\d+.*)?$/i.test(normalize(e.innerText || e.getAttribute('aria-label'))));
  const offer=stayButtons.find(button=>{for(let e=button.parentElement,n=0;e && n<7 && !e.matches('main, body');e=e.parentElement,n++){if(/在\s*ChatGPT\s*Work\s*中继续|Continue in ChatGPT Work/i.test(e.innerText) && /在\s*Work\s*中继续|Continue in Work/i.test(e.innerText))return true;}return false;});
  return {binding:{identity,sidebar_id:sidebarId,title:document.title.slice(0,160)},marker_present:lastText.includes(marker),user_text_chars:fullUserText.length,observed_markers:[...lastText.matchAll(/\[Fabushi:([^\]]+)\]/g)].map(m=>m[1]),foreign_marker:/\[Fabushi:[^\]]+\]/.test(lastText) && !lastText.includes(marker),interrupted,visible_work,length_limit,load_failed,approval_present:!!approval,approval_actionable:!!approval?.actionable,approval_fingerprint:approval ? approval.key : '',chat_work_offer:!!offer,empty_final:!!latest && page.response_actions_complete && page.response_action_turn_bound_to_last && !page.stop_available && !page.awaiting_assistant && !normalize(latest.querySelector('[data-markdown-text-style="assistant-message"], .markdown, [data-selected-text-overlay-target]')?.innerText || (/^(?:ChatGPT.*?[：:]\s*)?(?:Thinking|Thought|思考|正在思考)/i.test(normalize(latest.innerText))?'':latest.innerText))};
})()
"#;
const SWITCH_SCRIPT: &str = r#"
(async () => {
  const binding=__BINDING__;
  const visible=e=>!!(e && e.getClientRects().length);
  const norm=v=>String(v || '').replace(/\s+/g,' ').trim();
  // Background windows may never service requestAnimationFrame. Use a bounded
  // timer while locating mounted rows; no rendering callback can hang the visit.
  const settle=()=>new Promise(r=>setTimeout(r,150));
  const rows=()=>[...document.querySelectorAll('[data-thread-title="true"]')].filter(visible).map(t=>t.closest('[role="button"], a')).filter(e=>e && visible(e) && !e.closest('[data-message-author-role], [data-content-search-unit-key], pre, code, blockquote'));
  const ids=row=>{
    const match=row.getAttribute('href')?.match(/\/(?:c|work\/conversation)\/([^/?#]+)/);
    if(match)return [decodeURIComponent(match[1])];
    const key=Object.keys(row).find(key=>key.startsWith('__reactFiber$'));let fiber=key?row[key]:null;
    for(let i=0;fiber && i<20;i++,fiber=fiber.return){
      const p=fiber.memoizedProps||{};const own=[p.conversation?.id,p.conversationId,p.thread?.id,p.threadId].filter(v=>typeof v==='string');
      if(own.length)return own;
    }return [];
  };
  const active=()=>rows().find(r=>r.getAttribute('aria-current')==='page');
  if(active() && ids(active()).includes(binding.sidebar_id))return {ok:true};
  if(!binding.sidebar_id)return {ok:false,reason:'sidebar-binding-not-confirmed'};
  const composer=[...document.querySelectorAll('textarea, [contenteditable="true"]')].find(visible);
  if((composer?.value || composer?.innerText || '').trim())return {ok:false,reason:'unsaved-composer-draft'};
  let openedSidebar=false,scrolled=0;
  if(!rows().length){
    const toggle=[...document.querySelectorAll('button')].filter(visible).find(b=>/^(?:show sidebar|open sidebar|toggle sidebar|显示侧边栏|打开侧边栏|展开侧边栏|切换侧边栏|显示边栏|打开边栏|切换边栏|切换侧栏)$/i.test(norm(b.getAttribute('aria-label') || b.getAttribute('title') || b.innerText)));
    if(toggle){toggle.click();openedSidebar=true;await settle();}
    for(let n=0;!rows().length && n<8;n++)await settle();
  }
  const find=()=>rows().find(row=>ids(row).includes(binding.sidebar_id) || ids(row).includes(binding.identity));
  let row=find();
  if(!row){
    const containers=[];
    for(const item of rows()){
      for(let e=item.parentElement;e && !e.matches('body, main');e=e.parentElement){
        if(/auto|scroll/.test(getComputedStyle(e).overflowY) && e.scrollHeight>e.clientHeight+1){
          if(!containers.includes(e))containers.push(e);break;
        }
      }
    }
    // Scroll sidebar lists only. Opening a chat is reserved for the exact id.
    for(const list of containers.slice(0,3)){
      const original=list.scrollTop;
      await settle();row=find();let wrapped=false;
      for(let n=0;!row && n<24 && scrolled<24;n++){
        const before=list.scrollTop;
        list.scrollTop=Math.min(list.scrollHeight-list.clientHeight,before+Math.max(160,list.clientHeight*.75));
        if(list.scrollTop===before){
          if(wrapped || before===0)break;
          list.scrollTop=0;wrapped=true;
        }
        scrolled++;await settle();row=find();
      }
      if(row)break;
      // Keep the bounded scan position so a later retry can continue through
      // an older virtual list, without opening any unrelated conversation.
      if(wrapped && list.scrollTop>=original){list.scrollTop=original;await settle();}
    }
  }
  if(!row)return {ok:false,reason:'recorded-conversation-not-in-sidebar',mounted_rows:rows().length,opened_sidebar:openedSidebar,scroll_steps:scrolled};
  row.scrollIntoView({block:'nearest',behavior:'instant'});await settle();
  const rect=row.getBoundingClientRect();
  if(rect.width<=0 || rect.height<=0)return {ok:false,reason:'recorded-conversation-not-in-sidebar',mounted_rows:rows().length};
  return {ok:false,needs_click:true,x:rect.x+rect.width/2,y:rect.y+rect.height/2,opened_sidebar:openedSidebar,scroll_steps:scrolled};
})()
"#;
const PROFILE_SCRIPT: &str = r#"
(async () => {
  const profile=__PROFILE__;
  const visible=e=>!!(e && e.getClientRects().length);
  const normalize=v=>String(v || '').replace(/\s+/g,' ').trim();
  const sleep=ms=>new Promise(r=>setTimeout(r,ms));
  const labels=e=>[e.getAttribute('aria-label'),e.getAttribute('title'),e.innerText].map(normalize).filter(Boolean);
  const pointerClick=e=>{const r=e.getBoundingClientRect();const p={bubbles:true,cancelable:true,composed:true,button:0,buttons:1,clientX:r.x+r.width/2,clientY:r.y+r.height/2};e.dispatchEvent(new PointerEvent('pointerdown',{...p,pointerId:1,pointerType:'mouse',isPrimary:true}));e.dispatchEvent(new MouseEvent('mousedown',p));e.dispatchEvent(new PointerEvent('pointerup',{...p,buttons:0,pointerId:1,pointerType:'mouse',isPrimary:true}));e.dispatchEvent(new MouseEvent('mouseup',{...p,buttons:0}));e.dispatchEvent(new MouseEvent('click',{...p,buttons:0}));};
  const findTrigger=()=>[...document.querySelectorAll('button')].filter(visible).find(e=>labels(e).some(l=>/select chatgpt model|选择 ChatGPT 模型|思考强度|thinking effort/i.test(l)));
  let button=findTrigger();
  for(let n=0;!button && n<12;n++){await sleep(250);button=findTrigger();}
  if(!button) return {ok:false,reason:'model-trigger-unavailable'};
  pointerClick(button);await sleep(700);
  const radios=()=>[...document.querySelectorAll('[role="menuitemradio"], [role="radio"]')].filter(visible);
  let items=radios();
  if(!items.length){
    const choose=[...document.querySelectorAll('[role="menuitem"], button')].filter(visible).find(e=>labels(e).some(l=>/^(选择模型|select model|choose model)$/i.test(l)));
    if(choose){pointerClick(choose);await sleep(700);items=radios();}
  }
  const matches=e=>labels(e).some(l=>l===profile.model || l.startsWith(profile.model+' ') || l.startsWith(profile.model+'\n'));
  const model=items.find(matches);
  if(!model) return {ok:false,reason:'requested-model-unavailable',models:items.map(e=>labels(e))};
  if(model.getAttribute('aria-checked')!=='true'){pointerClick(model);await sleep(700);}
  // Read the checked radio after selection, reopening the submenu if needed.
  let verified=radios().find(e=>matches(e) && e.getAttribute('aria-checked')==='true');
  if(!verified){
    const choose=[...document.querySelectorAll('[role="menuitem"], button')].filter(visible).find(e=>labels(e).some(l=>/选择模型|select model|choose model/i.test(l)));
    if(choose){pointerClick(choose);await sleep(700);verified=radios().find(e=>matches(e) && e.getAttribute('aria-checked')==='true');}
  }
  if(!verified) return {ok:false,reason:'model-radio-not-verified'};
  document.dispatchEvent(new KeyboardEvent('keydown',{key:'Escape',code:'Escape',bubbles:true}));
  await sleep(700);
  const trigger=[...document.querySelectorAll('button')].filter(visible).find(e=>labels(e).some(l=>/select chatgpt model|选择 ChatGPT 模型|思考强度|thinking effort/i.test(l)));
  if(trigger){pointerClick(trigger);await sleep(700);}
  const sliders=[...document.querySelectorAll('[role="slider"], input[type="range"]')].filter(visible);
  const slider=sliders[0];
  const effortNames=[['instant','即时'],['medium','中'],['high','高'],['extra high','极高'],['pro']];
  const names=effortNames[profile.effort] || [];
  let effortVerified=false;
  if(slider){
    const min=Number(slider.getAttribute('aria-valuemin') ?? slider.min ?? 0);
    const max=Number(slider.getAttribute('aria-valuemax') ?? slider.max ?? 4);
    if(max-min!==4)return {ok:false,reason:'unknown-effort-slider-range',min,max};
    const target=min+profile.effort;
    slider.focus();
    const now=Number(slider.getAttribute('aria-valuenow') ?? slider.value);
    for(let i=0;i<Math.abs(target-now);i++){const key=target>now?'ArrowRight':'ArrowLeft';slider.dispatchEvent(new KeyboardEvent('keydown',{key,code:key,bubbles:true}));slider.dispatchEvent(new KeyboardEvent('keyup',{key,code:key,bubbles:true}));await sleep(100);}
    await sleep(700);
    effortVerified=Number(slider.getAttribute('aria-valuenow') ?? slider.value)===target;
  } else {
    const controls=[...document.querySelectorAll('[role="menuitemradio"], [role="radio"], [role="menuitem"], button')].filter(visible);
    const choice=controls.find(e=>labels(e).some(l=>names.includes(l.toLowerCase())));
    if(choice){if(choice.getAttribute('aria-checked')!=='true' && choice.getAttribute('aria-pressed')!=='true') {pointerClick(choice);await sleep(700);}
      effortVerified=choice.getAttribute('aria-checked')==='true' || choice.getAttribute('aria-pressed')==='true';}
    if(!effortVerified) effortVerified=labels(trigger || button).some(l=>names.some(n=>l.toLowerCase()===n || l.toLowerCase().endsWith(' '+n)));
  }
  if(!effortVerified)return {ok:false,reason:'reasoning-effort-not-verified'};
  document.dispatchEvent(new KeyboardEvent('keydown',{key:'Escape',code:'Escape',bubbles:true}));
  return {ok:true,model:profile.model,effort:profile.effort};
})()
"#;
const UPLOAD_READY_SCRIPT: &str = r#"
(async () => {
  const files=__FILES__;
  for(let i=0;i<90;i++){
    const composer=[...document.querySelectorAll('textarea, [contenteditable="true"]')].find(e=>e.getClientRects().length);
    const scope=composer?.closest('form') || composer?.parentElement?.parentElement;
    if(scope){
      const text=scope.innerText || '';
      const processing=scope.querySelector('[role="progressbar"], [aria-busy="true"]');
      if(!processing && files.every(f=>text.includes(f.name)))return {ok:true};
    }
    await new Promise(r=>setTimeout(r,500));
  }
  return {ok:false};
})()
"#;
const SCOPED_APPROVE_SCRIPT: &str = r#"
(async () => {
  const visible=e=>!!(e && e.getClientRects().length);
  const norm=v=>String(v || '').replace(/\s+/g,' ').trim();
  const label=e=>{const c=e.cloneNode(true);c.querySelectorAll('kbd, [aria-hidden="true"]').forEach(n=>n.remove());return norm(e.getAttribute('aria-label') || c.textContent).replace(/\s+(?:Esc|Enter|↵|⏎|⌄|▾)$/i,'').trim();};
  const session=/^(?:允许本次会话|允许此会话|仅允许本次会话|allow(?: .+)? for this (?:session|conversation))$/i;
  const once=/^(?:allow once|允许一次|approve once|仅允许本次|允许本次)$/i;
  const excluded='[data-message-author-role="user"], [data-content-search-unit-key$=":user"], blockquote, pre, code, nav, aside';
  const buttons=()=>[...document.querySelectorAll('button, [role="button"], [role="menuitem"]')].filter(e=>visible(e) && !e.disabled && e.getAttribute('aria-disabled')!=='true' && !e.closest(excluded));
  const choose=()=>buttons().find(b=>session.test(label(b)));
  __APPROVAL_SCAN__
  const target=__APPROVAL_TARGET__;
  const selected=approvalScan().find(c=>!c.resolved && (!target || target===c.key));
  const card=selected?.card;
  if(!card)return {clicked:false,reason:'request-no-longer-pending'};
  const existing=[...card.querySelectorAll('button,[role="button"]')].find(b=>visible(b) && !b.disabled && session.test(label(b)));
  if(existing){existing.click();return {clicked:true,scope:'conversation'};}
  const split=card?.querySelector('button[aria-haspopup="menu"]:not(:disabled), [role="button"][aria-haspopup="menu"]');
  if(split){
    if(split.getAttribute('aria-expanded')!=='true'){
      split.dispatchEvent(new PointerEvent('pointerdown',{bubbles:true,button:0,pointerId:1,pointerType:'mouse'}));
      if(split.getAttribute('aria-expanded')!=='true')split.click();
    }
    await new Promise(r=>setTimeout(r,700));
    if(!approvalScan().some(c=>!c.resolved && c.key===selected.key))return {clicked:false,reason:'request-changed-during-menu'};
    const choice=choose();if(choice){choice.click();return {clicked:true,scope:'conversation'};}
    document.dispatchEvent(new KeyboardEvent('keydown',{key:'Escape',bubbles:true}));
  }
  if(!card.isConnected || !approvalScan().some(c=>!c.resolved && c.key===selected.key))return {clicked:false,reason:'request-no-longer-pending'};
  const fallback=[...card.querySelectorAll('button,[role="button"]')].find(b=>visible(b) && !b.disabled && once.test(label(b)));
  if(fallback){fallback.click();return {clicked:true,scope:'once'};}
  return {clicked:false,reason:'no-matching-approval-action',card_found:!!card,split_found:!!split};
})()
"#;
const STAY_CHAT_SCRIPT: &str = r#"
(() => {
  const visible=e=>!!(e && e.getClientRects().length);
  const norm=v=>String(v || '').replace(/\s+/g,' ').trim();
  const excluded='[data-message-author-role="user"], [data-content-search-unit-key$=":user"], blockquote, pre, code, nav, aside';
  const buttons=[...document.querySelectorAll('button, [role="button"]')].filter(e=>visible(e) && !e.disabled && e.getAttribute('aria-disabled')!=='true' && !e.closest(excluded));
  for(const button of buttons.filter(e=>/^(?:留在聊天(?:模式|中)|stay in chat)(?:\s*\d+.*)?$/i.test(norm(e.innerText || e.getAttribute('aria-label'))))){
    for(let card=button.parentElement,n=0;card && n<7 && !card.matches('main, body');card=card.parentElement,n++){
      if(/在\s*ChatGPT\s*Work\s*中继续|Continue in ChatGPT Work/i.test(card.innerText) && /在\s*Work\s*中继续|Continue in Work/i.test(card.innerText)){
        button.click();return {clicked:true,action:'stay-in-chat'};
      }
    }
  }
  return {clicked:false};
})()
"#;
const SAFE_MODALS_SCRIPT: &str = r#"
(() => {
  for(const dialog of document.querySelectorAll('[role="dialog"]')){
    if(/approval|authorize|授权|permission|允许|terms|agreement|条款|协议|payment|purchase|付款|支付|delete|永久|删除/i.test(dialog.innerText))continue;
    const button=[...dialog.querySelectorAll('button')].find(b=>/^(Got it|OK|知道了|明白了)$/i.test((b.innerText || b.getAttribute('aria-label') || '').trim()) && !b.disabled);
    if(button){button.click();return {clicked:true};}
  }return {clicked:false};
})()
"#;

const STOP_SCRIPT: &str = r#"
(() => {
 const visible=e=>!!(e && e.getClientRects().length);
 const button=[...document.querySelectorAll('button')].filter(visible).find(b=>/^(stop|stop responding|stop streaming|stop generating|停止|停止回答|停止生成|停止输出)$/i.test((b.getAttribute('aria-label') || b.innerText || '').trim()) || b.getAttribute('data-testid')==='stop-button');
 if(button && !button.disabled){button.click();return {clicked:true};}return {clicked:false};
})()
"#;

const CLEAR_DRAFT_SCRIPT: &str = r#"
(() => {
 const norm=v=>String(v || '').replace(/\s+/g,' ').trim();
 const expected=__EXPECTED__;
 const composer=[...document.querySelectorAll('textarea, [contenteditable="true"]')].find(e=>e.getClientRects().length);
 if(!composer)return {ok:false,reason:'composer-not-found'};
 if(norm(composer.value ?? composer.innerText ?? composer.textContent)!==norm(expected))return {ok:false,reason:'draft-changed'};
 composer.focus();
 if('value' in composer){const setter=Object.getOwnPropertyDescriptor(Object.getPrototypeOf(composer),'value')?.set;if(setter)setter.call(composer,'');else composer.value='';}
 else{const selection=window.getSelection();const range=document.createRange();range.selectNodeContents(composer);selection?.removeAllRanges();selection?.addRange(range);document.execCommand('delete',false);composer.replaceChildren();}
 composer.dispatchEvent(new InputEvent('input',{bubbles:true,inputType:'deleteContentBackward',data:null}));
 return {ok:norm(composer.value ?? composer.innerText ?? composer.textContent)===''};
})()
"#;

const RETRY_LOAD_SCRIPT: &str = r#"
(() => {
  const visible=e=>!!(e && e.getClientRects().length);
  const norm=v=>String(v || '').replace(/\s+/g,' ').trim();
  const excluded='[data-message-author-role="user"], [data-content-search-unit-key$=":user"], blockquote, pre, code, nav, aside';
  const errorLabel=e=>{const c=e.cloneNode(true);c.querySelectorAll('button, [role="button"], a').forEach(n=>n.remove());return norm(c.textContent);};
  const error=/^(?:unable to load (?:this )?(?:ChatGPT )?conversation|无法加载.*(?:会话|对话))[。.!]?$/i;
  const retries=[...document.querySelectorAll('button, [role="button"], a')].filter(e=>/^(?:重试|重载|重新加载|retry|reload|try again)$/i.test(norm(e.getAttribute('aria-label') || e.textContent)) && visible(e) && !e.disabled && e.getAttribute('aria-disabled')!=='true' && !e.closest(excluded));
  for(const retry of retries){
    for(let card=retry.parentElement,n=0;card && n<5 && !card.matches('main,body');card=card.parentElement,n++){
      const text=card.textContent || '';
      if(text.length<250 && !card.closest(excluded) && !card.querySelector(excluded) && error.test(errorLabel(card))){retry.click();return {clicked:true};}
    }
  }
  return {clicked:false};
})()
"#;
