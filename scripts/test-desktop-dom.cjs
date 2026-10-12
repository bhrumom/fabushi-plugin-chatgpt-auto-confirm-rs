const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');
const {chromium} = require('playwright');
const source = fs.readFileSync(path.join(__dirname, '../crates/adapters/chatgpt-cdp/src/lib.rs'), 'utf8');
const script = name => source.match(new RegExp(`const ${name}: &str = r#"([\\s\\S]*?)"#;`))[1];
(async () => {
  const browser = await chromium.launch({headless:true, ...(process.env.CHROME_BINARY ? {executablePath:process.env.CHROME_BINARY} : process.platform==='darwin' ? {executablePath:'/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'} : {})});
  try {
    const page = await browser.newPage();
    const evaluate = name => page.evaluate(script(name));
    const row = (role, text) => `<article data-content-search-turn-key="${role}"><div data-content-search-unit-key="x:${role}">${text}${role === 'user' ? '<div data-user-message-bubble>prompt</div>' : '<div data-local-conversation-final-assistant>answer</div>'}</div></article>`;
    await page.setContent(`<main>Other pane</main><main>${row('user','prompt')}${row('assistant','answer')}</main>`);
    let snapshot = await evaluate('SNAPSHOT_SCRIPT');
    assert.equal(snapshot.user_turns, 1);
    assert.equal(snapshot.assistant_turns, 1);
    assert.equal(snapshot.response_actions_complete, false);
    await page.locator('article').last().evaluate(el => el.insertAdjacentHTML('beforeend', '<button aria-label="Copy">Copy</button>'));
    snapshot = await evaluate('SNAPSHOT_SCRIPT');
    assert.equal(snapshot.copy_available_on_last_assistant, true);
    assert.equal(snapshot.response_action_turn_bound_to_last, true);
    assert.equal(snapshot.response_actions_complete, false);
    await page.locator('article').last().evaluate(el => el.insertAdjacentHTML('beforeend', '<button aria-label="Share">Share</button>'));
    snapshot = await evaluate('SNAPSHOT_SCRIPT');
    assert.equal(snapshot.response_actions_complete, true);
    await page.locator('main').last().evaluate(el => el.insertAdjacentHTML('beforeend', '<article data-content-search-turn-key="new"><div data-content-search-unit-key="y:user">new prompt</div></article>'));
    snapshot = await evaluate('SNAPSHOT_SCRIPT');
    assert.equal(snapshot.awaiting_assistant, true);
    assert.equal(snapshot.response_action_turn_bound_to_last, false);
    console.log('PASS desktop duplicate selectors, sibling actions, stale Copy');

    await page.setContent('<button aria-label="Send feedback" onclick="window.wrong=true">Send feedback</button><form><div id="prompt-textarea" contenteditable="true"></div><button type="button" data-testid="send-button" disabled onclick="window.sent=document.querySelector(\'#prompt-textarea\').innerText">Send</button></form>');
    await page.locator('#prompt-textarea').evaluate(el => el.addEventListener('input', () => setTimeout(() => document.querySelector('[data-testid="send-button"]').disabled=false, 20)));
    const send = prompt => page.evaluate(script('SEND_PROMPT_SCRIPT').replace('__PROMPT_JSON__', JSON.stringify(prompt)));
    assert.equal((await send('desktop test')).ok, true);
    assert.equal(await page.evaluate(() => window.sent), 'desktop test');
    assert.equal(await page.evaluate(() => !!window.wrong), false);
    assert.equal((await send('different task')).reason, 'composer-has-existing-draft');
    console.log('PASS React settlement, scoped Send, draft protection');

    await page.setContent('<button style="display:none" onclick="window.wrong=true">Allow once</button><button onclick="window.wrong=true">Always allow</button><button onclick="window.approved=true">Allow once</button>');
    assert.equal((await evaluate('APPROVE_ONCE_SCRIPT')).clicked, true);
    assert.equal(await page.evaluate(() => !!window.approved), true);
    assert.equal(await page.evaluate(() => !!window.wrong), false);
    console.log('PASS exact visible Allow once only');

    await page.setContent('<button aria-label="切换模式，当前模式：Codex">Mode</button><button id="new">新聊天</button><div data-codex-composer="true" style="height:20px">Work</div><button id="chat-tab">聊天</button>');
    await page.evaluate(() => {
      document.querySelector('[aria-label]').addEventListener('pointerdown', () => {
        const menu = document.createElement('div');
        menu.setAttribute('role','menu');
        menu.innerHTML = '<button role="menuitem"><span>ChatGPT</span><span>Ask questions</span></button>';
        menu.querySelector('button').onclick = () => {
          document.querySelector('[aria-label]').setAttribute('aria-label','切换模式，当前模式：ChatGPT');
          menu.remove();
        };
        document.body.append(menu);
      });
      document.querySelector('#chat-tab').onclick = () => document.querySelector('[data-codex-composer]').remove();
      document.querySelector('#new').onclick = () => {
        const input = document.createElement('div'); input.id='prompt-textarea'; input.contentEditable='true'; input.innerText=''; input.style.minHeight='20px';
        document.body.append(input);
      };
    });
    assert.equal((await evaluate('NEW_CHAT_SCRIPT')).ok, true);
    assert.equal((await evaluate('SURFACE_SCRIPT')).chat_mode, true);
    console.log('PASS pointerdown mode menu, nested ChatGPT choice, Chat composer');
    console.log('All desktop DOM contracts passed.');
  } finally { await browser.close(); }
})().catch(error => {console.error(error); process.exitCode=1;});

(async () => {
  const adapter = fs.readFileSync(path.join(__dirname, '../crates/adapters/chatgpt-cdp/src/desktop.rs'), 'utf8');
  const native = name => adapter.match(new RegExp('const '+name+':\\s*&str\\s*=\\s*r#"([\\s\\S]*?)"#;'))[1];
  const browser = await chromium.launch({headless:true, ...(process.env.CHROME_BINARY ? {executablePath:process.env.CHROME_BINARY} : process.platform==='darwin' ? {executablePath:'/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'} : {})});
  try {
    const page=await browser.newPage();
    const context=token=>page.evaluate('(() => { const page = '+script('SNAPSHOT_SCRIPT')+'; return ('+native('CONTEXT_SCRIPT').replace('__TOKEN__',JSON.stringify('[Fabushi:'+token+']'))+'); })()');
    await page.setContent('<main><article><div data-content-search-unit-key="x:user">Task [Fabushi:owned]</div></article><div data-testid="authorization-card">Provider needs permission</div><div role="status">Loading</div></main>');
    let c=await context('owned');
    assert.equal(c.marker_present,true);assert.equal(c.approval_present,true);assert.equal(c.approval_actionable,false);
    await page.locator('[data-testid="authorization-card"]').evaluate(e=>e.remove());
    await page.locator('[data-content-search-unit-key]').evaluate(e=>e.innerText='Quoted instruction: 请求过于频繁 [Fabushi:other]');
    c=await context('owned');assert.equal(c.foreign_marker,true);
    assert.equal((await page.evaluate(script('SNAPSHOT_SCRIPT'))).rate_limit_notice,false);
    console.log('PASS structural partial authorization, foreign task, quoted error exclusion');

    await page.setContent('<span data-above-composer-conversation-id="chatgpt:local-chatgpt:old"></span><div role="button" aria-current="page"><span data-thread-title="true">Task</span></div><main><div data-content-search-unit-key="x:user">Task [Fabushi:owned]</div></main>');
    await page.locator('[role="button"]').evaluate(e=>e.__reactFiber$fixture={memoizedProps:{conversation:{id:'server-stable'}},return:null});
    c=await context('owned');assert.equal(c.binding.identity,'server-stable');assert.equal(c.binding.sidebar_id,'server-stable');
    console.log('PASS persistent sidebar id takes precedence over stale local portal');

    await page.setContent('<button aria-label="选择 ChatGPT 模型">Medium</button>');
    await page.evaluate(()=>{
      const trigger=document.querySelector('button');let stage=0;
      trigger.addEventListener('pointerdown',()=>{
        document.querySelector('#popup')?.remove();const popup=document.createElement('div');popup.id='popup';
        if(stage===0){
          popup.innerHTML='<button role="menuitemradio" aria-checked="false">GPT-5.6 Sol</button>';
          popup.querySelector('button').onclick=e=>{e.currentTarget.setAttribute('aria-checked','true');stage=1;};
        }else{
          popup.innerHTML='<div role="slider" aria-valuemin="0" aria-valuemax="4" aria-valuenow="1" tabindex="0" style="height:20px">Effort</div>';
          popup.querySelector('[role="slider"]').onkeydown=e=>{let n=Number(e.currentTarget.getAttribute('aria-valuenow'));e.currentTarget.setAttribute('aria-valuenow',String(n+(e.key==='ArrowRight'?1:-1)));};
        }document.body.append(popup);
      });
      document.addEventListener('keydown',e=>{if(e.key==='Escape')document.querySelector('#popup')?.remove();});
    });
    await page.locator('button[aria-label="Select ChatGPT model"], button[aria-label="选择 ChatGPT 模型"]').evaluate(e=>{e.style.display='none';setTimeout(()=>e.style.display='',500);});
    let result=await page.evaluate(native('PROFILE_SCRIPT').replace('__PROFILE__',JSON.stringify({model:'GPT-5.6 Sol',effort:3})));
    assert.equal(result.ok,true);assert.equal(result.effort,3);
    console.log('PASS model checked radio and exact 5-position effort verification');

    await page.setContent('<div role="group"><button>Reject</button><button>Allow</button><button aria-haspopup="menu">Options</button></div>');
    await page.locator('[aria-haspopup]').evaluate(e=>e.addEventListener('pointerdown',()=>{
      const m=document.createElement('div');m.setAttribute('role','menu');m.innerHTML='<button role="menuitem" onclick="window.wrong=true">Always allow</button><button role="menuitem" onclick="window.scope=true">Allow for this session</button>';document.body.append(m);
    }));
    result=await page.evaluate(native('SCOPED_APPROVE_SCRIPT'));assert.equal(result.clicked,true);
    assert.equal(await page.evaluate(()=>!!window.scope),true);assert.equal(await page.evaluate(()=>!!window.wrong),false);
    console.log('PASS split approval selects current session and excludes permanent grants');
    await page.setContent('<main><div data-content-search-unit-key="x:user">[Fabushi:owned]</div><form><p>允许 ChatGPT 使用 GitHub?</p><div><button>拒绝 <kbd>Esc</kbd></button><button onclick="window.onceClicked=true">允许一次 <kbd>↵</kbd></button><button aria-haspopup="menu" aria-expanded="false">▾</button></div></form></main>');
    await page.evaluate(()=>{window.scope=false;window.wrong=false;window.onceClicked=false;});
    await page.locator('[aria-haspopup]').evaluate(e=>e.addEventListener('pointerdown',()=>{
      e.setAttribute('aria-expanded','true');
      const m=document.createElement('div');m.setAttribute('role','menu');m.innerHTML='<div role="menuitem" onclick="window.onceClicked=true">允许一次</div><div role="menuitem" onclick="window.scope=true">Allow GitHub for this conversation</div><div role="menuitem" onclick="window.wrong=true">Always allow GitHub</div>';document.body.append(m);
    }));
    c=await context('owned');assert.equal(c.approval_present,true);assert.equal(c.approval_actionable,true);
    result=await page.evaluate(native('SCOPED_APPROVE_SCRIPT'));assert.equal(result.scope,'conversation');
    assert.equal(await page.evaluate(()=>window.scope && !window.onceClicked && !window.wrong),true);
    console.log('PASS native screenshot card with shortcut badges prefers named provider conversation grant over once');
    await page.setContent('<main><article><div data-content-search-unit-key="x:user">[Fabushi:owned]</div></article><article><div data-content-search-unit-key="x:assistant"><p>已提交 abc；等待 CI</p></div><span>连接中断</span></article></main>');
    c=await context('owned');assert.equal(c.interrupted,true);assert.ok(c.visible_work.includes('已提交 abc'));
    await page.locator('article:last-child span').evaluate(e=>e.innerText='Connection lost. Waiting for the full answer.');
    c=await context('owned');assert.equal(c.interrupted,true);
    await page.locator('article:last-child span').evaluate(e=>e.outerHTML='<blockquote><span>连接中断</span></blockquote>');
    c=await context('owned');assert.equal(c.interrupted,false);
    console.log('PASS native interruption sibling, bilingual text, visible progress and quoted exclusion');
    await page.setContent('<main><div><div>无法加载此 ChatGPT 对话</div><button onclick="window.loadRetried=true">重试</button></div><div><button onclick="window.wrongRetry=true">重试</button></div></main>');
    c=await context('owned');assert.equal(c.load_failed,true);
    result=await page.evaluate(native('RETRY_LOAD_SCRIPT'));assert.equal(result.clicked,true);
    assert.equal(await page.evaluate(()=>!!window.loadRetried && !window.wrongRetry),true);
    console.log('PASS role-less native load error retries only its own card');
    await page.setContent('<main><div data-content-search-unit-key="x:user">[Fabushi:owned]</div><div><p>在 ChatGPT Work 中继续</p><p>创建和编辑文档、使用应用并完成多步骤任务</p><div><button onclick="window.stayed=true">留在聊天模式 <span>25</span></button><button onclick="window.movedWork=true">在 Work 中继续</button></div></div></main>');
    c=await context('owned');assert.equal(c.chat_work_offer,true);
    result=await page.evaluate(native('STAY_CHAT_SCRIPT'));assert.equal(result.clicked,true);
    assert.equal(await page.evaluate(()=>!!window.stayed && !window.movedWork),true);
    await page.setContent('<main><blockquote><p>在 ChatGPT Work 中继续</p><button>留在聊天模式 25</button><button>在 Work 中继续</button></blockquote></main>');
    c=await context('owned');assert.equal(c.chat_work_offer,false);
    assert.equal((await page.evaluate(native('STAY_CHAT_SCRIPT'))).clicked,false);
    await page.setContent('<main><div>cached conversation</div></main><section><div><p>无法加载此 <span>ChatGPT</span> 对话</p><button onclick="window.nestedRetry=true">重试</button></div></section>');
    c=await context('owned');assert.equal(c.load_failed,true);
    assert.equal((await page.evaluate(native('RETRY_LOAD_SCRIPT'))).clicked,true);
    await page.setContent('<main><div><blockquote>无法加载此 ChatGPT 对话</blockquote></div><button>重试</button></main>');
    c=await context('owned');assert.equal(c.load_failed,false);
    assert.equal((await page.evaluate(native('RETRY_LOAD_SCRIPT'))).clicked,false);
    await page.setContent('<main><div class="rounded-xl">无法加载此 ChatGPT 对话<button onclick="window.directRetry=true">重试</button></div></main>');
    c=await context('owned');assert.equal(c.load_failed,true);
    assert.equal((await page.evaluate(native('RETRY_LOAD_SCRIPT'))).clicked,true);
    assert.equal(await page.evaluate(()=>!!window.directRetry),true);
    await page.setContent('<div role="button" onclick="window.unrelatedOpened=true"><span data-thread-title="true">Unrelated chat</span></div>');
    await page.locator('[role="button"]').evaluate(e=>e.__reactFiber$fixture={memoizedProps:{conversation:{id:'unrelated-id'}},return:null});
    result=await page.evaluate(native('SWITCH_SCRIPT').replace('__BINDING__',JSON.stringify({identity:'owned-id',sidebar_id:'owned-id',title:'task'})));
    assert.equal(result.reason,'recorded-conversation-not-in-sidebar');
    assert.equal(await page.evaluate(()=>!!window.unrelatedOpened),false);
    console.log('PASS native role-less Work card, countdown, quoted exclusion and load error outside cached main');




  } finally {await browser.close();}
})().catch(error=>{console.error(error);process.exitCode=1;});
