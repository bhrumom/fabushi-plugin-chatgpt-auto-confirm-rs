use super::ChatGptCdp;
use anyhow::{Result, bail};
use async_trait::async_trait;
use fabushi_chatgpt_application::TaskDesktop;
use fabushi_chatgpt_domain::*;
use serde_json::{Value, json};

impl ChatGptCdp {
    pub async fn conversations(&self) -> Result<Value> {
        self.evaluate(CONVERSATIONS_SCRIPT).await
    }
    pub async fn task_observation(&self, token: &str) -> Result<TaskObservation> {
        let context = CONTEXT_SCRIPT.replace(
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
        self.evaluate(SCOPED_APPROVE_SCRIPT).await
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
                || b.sidebar_id.starts_with("local-chatgpt:"))
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
    async fn approve(&self) -> Result<bool> {
        Ok(self.evaluate(SCOPED_APPROVE_SCRIPT).await?["clicked"] == true)
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
  const userText=node=>{
    if(!node)return '';
    const texts=[node.textContent || node.innerText || ''];
    const add=value=>{if(typeof value==='string')texts.push(value);else if(Array.isArray(value)){for(const part of value){if(typeof part==='string')texts.push(part);else if(typeof part?.text==='string')texts.push(part.text);}}};
    const key=Object.keys(node).find(k=>k.startsWith('__reactFiber$'));let fiber=key?node[key]:null;
    for(let i=0;fiber && i<10;i++,fiber=fiber.return){
      const p=fiber.memoizedProps || {};
      add(p.text);add(p.content);add(p.message?.text);add(p.message?.content);add(p.message?.content?.parts);add(p.message?.parts);add(p.userMessage?.text);add(p.userMessage?.content);
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
  const decisionParents=[...document.querySelectorAll('button[aria-haspopup="menu"]')].flatMap(b=>{const a=[];for(let e=b.parentElement,n=0;e && n<6 && !e.matches('main, body');e=e.parentElement,n++)a.push(e);return a;});
  const candidates=[...decisionParents,...document.querySelectorAll('[data-testid*="approval"], [data-testid*="authorization"], [data-approval-card], [data-tool-approval], [role="dialog"], [role="group"]')].filter(e=>visible(e) && !e.closest(excluded));
  const cards=candidates.filter(e=> {
    const metadata=[...e.attributes].some(a=>/approval|authorization|permission/i.test(a.name+' '+a.value));
    const labels=[...e.querySelectorAll('button, [role="button"]')].filter(visible).map(approvalLabel);
    return metadata || (labels.some(l=>/^(allow|approve|允许|允许一次|allow once|approve once)$/i.test(l)) && labels.some(l=>/^(reject|deny|拒绝|拒绝授权)$/i.test(l)));
  });
  const exact=[...document.querySelectorAll('button, [role="button"]')].filter(b=>visible(b) && !b.disabled && !b.closest(excluded)).find(b=>/^(allow once|允许一次|approve once|仅允许本次|允许本次|允许本次会话|allow(?: .+)? for this session|allow(?: .+)? for this conversation)$/i.test(approvalLabel(b)));
  const cardText=cards.map(e=>normalize(e.innerText)).join(' ').slice(0,1000);
  const statusNodes=[...main.querySelectorAll('[role="alert"], [role="status"], [data-testid*="error"]')].filter(e=>visible(e) && !e.closest('blockquote, pre, code, [data-content-search-unit-key$=":user"]'));
  const statusText=statusNodes.map(e=>normalize(e.innerText)).join(' ');
  const latest=[...main.querySelectorAll('[data-content-search-unit-key$=":assistant"], [data-message-author-role="assistant"], [data-local-conversation-final-assistant]')].at(-1);
  const latestProjection=latest?.cloneNode(true);latestProjection?.querySelectorAll('blockquote, pre, code').forEach(e=>e.remove());
  const latestLines=(latestProjection?.textContent || '').split('\n').map(normalize);
  const interruption=/^(?:connection (?:was )?(?:interrupted|lost)|连接(?:已)?中断|连接断开)(?:[，。,.：:]?\s*(?:waiting for (?:the )?(?:complete|full) answer|正在等待完整(?:答复|回复)))?[。.!]?$/i;
  const turn=latest?.closest('article, [data-content-search-turn-key]') || latest;
  const interruptionNodes=[...(turn || main).querySelectorAll('p, span, div')].filter(e=>visible(e) && !e.closest('blockquote, pre, code, [data-content-search-unit-key$=":user"]') && e.children.length===0);
  const interrupted=latestLines.some(l=>interruption.test(l)) || interruptionNodes.some(e=>interruption.test(normalize(e.innerText))) || /connection (?:was )?(?:interrupted|lost)|连接(?:已)?中断|连接断开/i.test(statusText);
  const publicTurns=[...main.querySelectorAll('[data-content-search-unit-key$=":assistant"], [data-message-author-role="assistant"]')].filter(visible).slice(-3);
  const visible_work=publicTurns.map(e=>(e.closest('article, [data-content-search-turn-key]') || e).innerText || '').join('\n\n').slice(-64000);
  const length_limit=/reached.*(?:conversation|chat).*limit|此对话.*长度上限|达到.*对话.*长度上限/i.test(statusText) || latestLines.some(l=>/^(you.*reached.*(?:conversation|chat).*limit.*|你.*(?:达到|已达到).*对话.*长度上限.*)$/i.test(l));
  const loadLabel=e=>{const c=e.cloneNode(true);c.querySelectorAll('button, [role="button"], a').forEach(n=>n.remove());return normalize(c.textContent);};
  const loadError=/^(?:unable to load (?:this )?(?:ChatGPT )?conversation|无法加载.*(?:会话|对话))[。.!]?$/i;
  const load_failed=/unable to load (?:this )?(?:ChatGPT )?conversation|无法加载.*(?:会话|对话)/i.test(statusText) || [...document.querySelectorAll('p, span, div')].some(e=>visible(e) && !e.closest(excluded) && !e.querySelector(excluded) && normalize(e.innerText).length<180 && loadError.test(loadLabel(e)));

  const stayButtons=[...document.querySelectorAll('button, [role="button"]')].filter(e=>visible(e) && !e.disabled && !e.closest(excluded) && /^(?:留在聊天(?:模式|中)|stay in chat)(?:\s*\d+.*)?$/i.test(normalize(e.innerText || e.getAttribute('aria-label'))));
  const offer=stayButtons.find(button=>{for(let e=button.parentElement,n=0;e && n<7 && !e.matches('main, body');e=e.parentElement,n++){if(/在\s*ChatGPT\s*Work\s*中继续|Continue in ChatGPT Work/i.test(e.innerText) && /在\s*Work\s*中继续|Continue in Work/i.test(e.innerText))return true;}return false;});
  return {binding:{identity,sidebar_id:sidebarId,title:document.title.slice(0,160)},marker_present:lastText.includes(marker),user_text_chars:fullUserText.length,observed_markers:[...lastText.matchAll(/\[Fabushi:([^\]]+)\]/g)].map(m=>m[1]),foreign_marker:/\[Fabushi:[^\]]+\]/.test(lastText) && !lastText.includes(marker),interrupted,visible_work,length_limit,load_failed,approval_present:cards.length>0 || !!exact,approval_actionable:!!exact || cards.some(e=>e.querySelector('button[aria-haspopup="menu"]:not(:disabled)')),approval_fingerprint:identity+'|'+cardText,chat_work_offer:!!offer,empty_final:!!latest && page.response_actions_complete && page.response_action_turn_bound_to_last && !page.stop_available && !page.awaiting_assistant && !normalize(latest.querySelector('[data-markdown-text-style="assistant-message"], .markdown, [data-selected-text-overlay-target]')?.innerText || (/^(?:ChatGPT.*?[：:]\s*)?(?:Thinking|Thought|思考|正在思考)/i.test(normalize(latest.innerText))?'':latest.innerText))};
})()
"#;
const SWITCH_SCRIPT: &str = r#"
(async () => {
  const binding=__BINDING__;
  const identity=()=> ([...document.querySelectorAll('[data-above-composer-conversation-id]')].find(e=>e.getClientRects().length)?.getAttribute('data-above-composer-conversation-id') || '').replace(/^chatgpt:/,'');
  const activeSidebarId=()=>{
    const row=[...document.querySelectorAll('[data-thread-title="true"]')].map(t=>t.closest('[role="button"], a')).find(r=>r?.getAttribute('aria-current')==='page');
    if(!row)return '';
    const key=Object.keys(row).find(key=>key.startsWith('__reactFiber$'));let fiber=key?row[key]:null;
    for(let i=0;fiber && i<10;i++,fiber=fiber.return){const p=fiber.memoizedProps||{};const id=p.conversation?.id || p.conversationId || p.thread?.id || p.threadId;if(typeof id==='string')return id;}
    return decodeURIComponent(row.getAttribute('href')?.match(/\/(?:c|work\/conversation)\/([^/?#]+)/)?.[1] || '');
  };
  if(activeSidebarId()===binding.sidebar_id) return {ok:true};
  if(!binding.sidebar_id) return {ok:false,reason:'sidebar-binding-not-confirmed'};
  const rows=[...document.querySelectorAll('[data-thread-title="true"]')].map(t=>t.closest('[role="button"], a')).filter(Boolean);
  const row=rows.find(row=>{
    const ids=[];const key=Object.keys(row).find(key=>key.startsWith('__reactFiber$'));let fiber=key?row[key]:null;
    for(let i=0;fiber && i<10;i++,fiber=fiber.return) {const p=fiber.memoizedProps||{};ids.push(p.conversation?.id,p.conversationId,p.thread?.id,p.threadId);}
    const match=row.getAttribute('href')?.match(/\/(?:c|work\/conversation)\/([^/?#]+)/);if(match)ids.push(decodeURIComponent(match[1]));
    return ids.includes(binding.sidebar_id) || ids.includes(binding.identity);
  });
  if(!row) return {ok:false,reason:'recorded-conversation-not-in-sidebar'};
  const composer=[...document.querySelectorAll('textarea, [contenteditable="true"]')].find(e=>e.getClientRects().length);
  if((composer?.value || composer?.innerText || '').trim()) return {ok:false,reason:'unsaved-composer-draft'};
  row.scrollIntoView({block:'nearest',behavior:'instant'});
  await new Promise(requestAnimationFrame);
  const rect=row.getBoundingClientRect();
  return {ok:false,needs_click:true,x:rect.x+rect.width/2,y:rect.y+rect.height/2};
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
  const button=[...document.querySelectorAll('button')].filter(visible).find(e=>labels(e).some(l=>/select chatgpt model|选择 ChatGPT 模型|思考强度|thinking effort/i.test(l)));
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
  const existing=choose();if(existing){existing.click();return {clicked:true,scope:'conversation'};}
  // Native cards need not have a role. Require both decision controls within
  // the same small ancestor, and ignore keyboard shortcut child text.
  let card;
  for(const button of buttons().filter(b=>once.test(label(b)) || /^(?:allow|approve|允许)$/i.test(label(b)))) {
    for(let e=button.parentElement,n=0;e && n<6;n++,e=e.parentElement){
      if(e.matches('main, body') || e.closest(excluded))break;
      const labels=[...e.querySelectorAll('button, [role="button"]')].filter(visible).map(label);
      if(labels.some(l=>/^(?:reject|deny|拒绝|拒绝授权)$/i.test(l))){card=e;break;}
    }
    if(card)break;
  }
  const split=card?.querySelector('button[aria-haspopup="menu"]:not(:disabled), [role="button"][aria-haspopup="menu"]');
  if(split){
    if(split.getAttribute('aria-expanded')!=='true'){
      split.dispatchEvent(new PointerEvent('pointerdown',{bubbles:true,button:0,pointerId:1,pointerType:'mouse'}));
      if(split.getAttribute('aria-expanded')!=='true')split.click();
    }
    await new Promise(r=>setTimeout(r,700));
    const choice=choose();if(choice){choice.click();return {clicked:true,scope:'conversation'};}
    document.dispatchEvent(new KeyboardEvent('keydown',{key:'Escape',bubbles:true}));
  }
  const fallback=buttons().find(b=>once.test(label(b)));
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
  for(const text of [...document.querySelectorAll('p, span, div')].filter(e=>visible(e) && !e.closest(excluded) && !e.querySelector(excluded) && norm(e.innerText).length<180 && error.test(errorLabel(e)))) {
    for(let card=text,n=0;card && n<4 && !card.matches('main, body');card=card.parentElement,n++) {
      const retry=[...card.querySelectorAll('button, [role="button"], a')].find(e=>visible(e) && !e.disabled && e.getAttribute('aria-disabled')!=='true' && /^(?:重试|重载|重新加载|retry|reload|try again)$/i.test(norm(e.innerText || e.getAttribute('aria-label'))));
      if(retry){retry.click();return {clicked:true};}
    }
  }
  return {clicked:false};
})()
"#;
