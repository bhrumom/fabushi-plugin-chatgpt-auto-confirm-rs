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
    await page.setViewportSize({width:1280,height:900});
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

    await page.setContent('<button aria-label="切换模式，当前模式：Codex">Mode</button><button id="new" onclick="window.newChats=(window.newChats||0)+1">新聊天</button><div data-codex-composer="true" style="height:20px">Work</div><button id="chat-tab">聊天</button><div data-conversation-id="original">original conversation</div>');
    await page.evaluate(() => {
      document.querySelector('[aria-label]').addEventListener('pointerdown', () => setTimeout(() => {
        const menu=document.createElement('div');menu.setAttribute('role','menu');
        menu.innerHTML='<button role="menuitem"><span>ChatGPT</span><span>Ask questions</span></button>';
        menu.querySelector('button').onclick=()=>{document.querySelector('[aria-label]').setAttribute('aria-label','切换模式，当前模式：ChatGPT');menu.remove();};
        document.body.append(menu);
      },250));
      document.querySelector('#chat-tab').onclick=()=>{
        document.querySelector('[data-codex-composer]').remove();
        const composer=document.createElement('textarea');composer.id='prompt-textarea';document.body.append(composer);
      };
    });
    assert.equal((await evaluate('CHAT_MODE_SCRIPT')).ok,true);
    assert.equal(await page.evaluate(()=>window.newChats||0),0);
    assert.equal(await page.locator('[data-conversation-id]').getAttribute('data-conversation-id'),'original');
    assert.deepEqual(await evaluate('CHAT_MODE_SCRIPT'),{ok:true,changed:false});
    await page.setContent('<button aria-label="切换模式，当前模式：Codex" onclick="window.changed=true">Mode</button><div data-codex-composer="true"><textarea>unsent draft</textarea></div>');
    assert.equal((await evaluate('CHAT_MODE_SCRIPT')).reason,'mode-switch-would-hide-draft-or-attachments');
    assert.equal(await page.evaluate(()=>!!window.changed),false);
    console.log('PASS standalone chat-mode, delayed menu, retained conversation, no new chat/send, idempotence and draft protection');
    await page.setContent('<button aria-label="切换模式，当前模式：ChatGPT">ChatGPT</button><main><div>无法加载此 ChatGPT 对话<button onclick="window.loadRetried=true">重试</button></div></main>');
    assert.deepEqual(await evaluate('CHAT_MODE_SCRIPT'),{ok:true,changed:false});
    assert.equal((await evaluate('SURFACE_SCRIPT')).composer,false);
    const loadSource=fs.readFileSync(path.join(__dirname,'../crates/adapters/chatgpt-cdp/src/desktop.rs'),'utf8');
    const loadRetry=loadSource.match(/const RETRY_LOAD_SCRIPT: &str = r#"([\s\S]*?)"#;/)[1];
    assert.equal((await page.evaluate(loadRetry)).clicked,true);
    assert.equal(await page.evaluate(()=>window.loadRetried),true);
    assert.equal((await send('must not send from error page')).reason,'composer-not-found');
    console.log('PASS composer-less ChatGPT load error reaches scoped Retry while Send remains blocked');

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
    await page.setContent('<main><div contenteditable="true" id="native-composer"></div></main>');
    const panelScript = fs.readFileSync(path.join(__dirname, '../crates/adapters/chatgpt-cdp/src/workbench-panel.js'), 'utf8');
    const panelTask={id:'owned',title:'<img src=x onerror="window.injected=true">',goal:'原始完整目标',round:4,phase:'work',mode:'goal',state:'generating',workspace:'桌面工作区',profile:{model:'GPT-5.6 Sol',effort:3},auto_approve:true,approvals:2,recoveries:1,last_error:'',updated_at:1,attachments:[],latest_reply:'公开回复',binding:{sidebar_id:'exact-owned'}};
    const panelSnapshot={version:2,running:true,scans:9,now:Date.now(),tasks:[panelTask],activity:[{task_id:'owned',at:Date.now(),role:'status',text:'扫描并确认任务归属'}],operations:[{at:Date.now(),task_id:'owned',text:'已确认切换任务'}]};
    const panelCSS=fs.readFileSync(path.join(__dirname,'../crates/adapters/chatgpt-cdp/src/workbench-panel.css'),'utf8');
    const renderPanel=state=>page.evaluate(panelScript.replace('__PANEL_CSS__',panelCSS).replace('__SNAPSHOT_JSON__',JSON.stringify(state)));
    assert.deepEqual((await renderPanel(panelSnapshot)).commands,[]);
    await page.getByRole('button',{name:'打开 Fabushi 脚本工作台'}).click();
    assert.equal(await page.locator('#fabushi-workbench-entry #panel').isVisible(),true);
    assert.equal(Math.round((await page.locator('#fabushi-workbench-entry #panel').boundingBox()).width),880);
    assert.equal(Math.round((await page.locator('#fabushi-workbench-entry aside').boundingBox()).width),250);
    await page.mouse.move(0,0);
    assert.equal(await page.locator('#fabushi-workbench-entry #entry').evaluate(el=>getComputedStyle(el).backgroundColor),'rgb(96, 72, 220)');
    assert.equal((await evaluate('SURFACE_SCRIPT')).workbench.panel_open,true);
    assert.equal(await page.locator('#fabushi-workbench-entry .task img').count(),0);
    await page.getByRole('button',{name:'查看任务详情：'+panelTask.title}).click();
    assert.match(await page.locator('#fabushi-workbench-entry #feed').textContent(),/原始完整目标/);
    assert.match(await page.locator('#fabushi-workbench-entry #feed').textContent(),/公开回复/);
    assert.equal(await page.locator('#fabushi-workbench-entry #effort').inputValue(),'3');
    await page.locator('#fabushi-workbench-entry #model').selectOption('GPT-6');
    let actions=(await renderPanel(panelSnapshot)).commands;assert.equal(actions.find(a=>a.action==='model').id,'owned');assert.equal(actions.find(a=>a.action==='select').id,'owned');
    await page.locator('#fabushi-workbench-entry #edit').click();
    await page.locator('#fabushi-workbench-entry #goal').fill('编辑之后的目标');
    await page.locator('#fabushi-workbench-entry #send').click();
    await page.waitForTimeout(30);actions=(await renderPanel(panelSnapshot)).commands;
    assert.equal(actions[0].action,'edit');assert.equal(actions[0].goal,'编辑之后的目标');
    await renderPanel({...panelSnapshot,notice:{request_id:actions[0].request_id,action:'edit',ok:false,text:'版本冲突'}});
    assert.equal(await page.locator('#fabushi-workbench-entry #goal').inputValue(),'编辑之后的目标');
    await page.locator('#fabushi-workbench-entry #cancel-edit').click();
    await page.getByRole('button',{name:'＋ 新任务',exact:true}).click();
    assert.equal((await renderPanel(panelSnapshot)).commands[0].action,'select');
    await page.locator('#fabushi-workbench-entry #goal').fill('用户尚未发送的目标');
    const tinyPNG=Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/ScLbtAAAAABJRU5ErkJggg==','base64');
    await page.locator('#fabushi-workbench-entry #file-input').setInputFiles({name:'paste.png',mimeType:'image/png',buffer:tinyPNG});
    assert.equal(await page.locator('#fabushi-workbench-entry #files img').count(),1);
    await page.evaluate(()=>{const dt=new DataTransfer();dt.items.add(window.__fabushiWorkbench.files[0]);const e=new ClipboardEvent('paste',{clipboardData:dt,bubbles:true,cancelable:true});window.__fabushiWorkbench.q('compose').dispatchEvent(e);});
    assert.equal(await page.locator('#fabushi-workbench-entry #files img').count(),1);
    await renderPanel(panelSnapshot);
    assert.equal(await page.locator('#fabushi-workbench-entry #goal').inputValue(),'用户尚未发送的目标');
    assert.equal(await page.evaluate(()=>document.querySelectorAll('textarea,input').length),0);
    assert.equal((await evaluate('SURFACE_SCRIPT')).workbench.unsaved,true);
    await page.locator('#fabushi-workbench-entry #settings-toggle').click();
    await page.locator('#fabushi-workbench-entry #cleanup').click();
    assert.deepEqual((await renderPanel(panelSnapshot)).commands,[]);
    assert.match(await page.locator('#fabushi-workbench-entry #notice').textContent(),/清空工作台草稿/);
    await page.locator('#fabushi-workbench-entry #send').click();
    await page.waitForTimeout(50);actions=(await renderPanel(panelSnapshot)).commands;
    assert.equal(actions.length,1);assert.equal(actions[0].action,'add');assert.equal(actions[0].files[0].name,'paste.png');assert.equal(actions[0].files[0].data,tinyPNG.toString('base64'));
    await page.locator('#fabushi-workbench-entry #goal').fill('提交后新写的草稿');
    await renderPanel({...panelSnapshot,notice:{request_id:actions[0].request_id,action:'add',ok:true,text:'已保存',result:{task_id:'owned'}}});
    assert.equal(await page.locator('#fabushi-workbench-entry #goal').inputValue(),'提交后新写的草稿');
    assert.equal(await page.locator('#fabushi-workbench-entry #files img').count(),0);
    await page.locator('#fabushi-workbench-entry [data-global="pause_all"]').click();
    assert.equal((await renderPanel(panelSnapshot)).commands[0].action,'pause_all');
    await page.locator('#fabushi-workbench-entry #global-auto').uncheck();
    assert.equal((await renderPanel(panelSnapshot)).commands[0].auto,false);
    await page.evaluate(()=>{const dt=new DataTransfer();dt.setData('application/x-fabushi-task','owned');window.__fabushiWorkbench.root.querySelector('.task-group').dispatchEvent(new DragEvent('drop',{dataTransfer:dt,bubbles:true,cancelable:true}));});
    const moved=(await renderPanel(panelSnapshot)).commands[0];assert.equal(moved.action,'workspace');assert.equal(moved.id,'owned');assert.equal(moved.value,'桌面工作区');
    assert.equal(browser.contexts()[0].pages().length,1);
    await page.locator('#fabushi-workbench-entry .task-action').filter({hasText:'删除'}).first().evaluate(el=>window.deleteDisabled=el.disabled);
    assert.equal(await page.evaluate(()=>window.deleteDisabled),true);
    const hostBefore=await page.locator('#fabushi-workbench-entry').evaluate(el=>{window.oldWorkbench=el;el.remove();return true;});
    await page.waitForTimeout(350);assert.equal(hostBefore,true);
    assert.equal(await page.locator('#fabushi-workbench-entry').evaluate(el=>el===window.oldWorkbench),true);
    assert.equal(await page.locator('#fabushi-workbench-entry #goal').inputValue(),'提交后新写的草稿');
    const repairs=await page.evaluate(()=>window.__fabushiWorkbench.repairs);
    await page.evaluate(()=>{for(let i=0;i<1000;i++){const n=document.createElement('div');document.body.append(n);n.remove();}});
    await page.waitForTimeout(350);
    assert.equal(await page.evaluate(()=>window.__fabushiWorkbench.repairs),repairs);
    assert.equal(await page.locator('#fabushi-workbench-entry').evaluate(el=>el.parentNode===document.documentElement),true);
    console.log('PASS userscript-style workbench layout, selected feed, future model edit, goal edit, private attachments, previews, acknowledgements, cleanup guard and hydration preservation');
  } finally { await browser.close(); }
})().catch(error => {console.error(error); process.exitCode=1;});

(async () => {
  const adapter = fs.readFileSync(path.join(__dirname, '../crates/adapters/chatgpt-cdp/src/desktop.rs'), 'utf8');
  const native = (name,receipts=[],target='') => adapter.match(new RegExp('const '+name+':\\s*&str\\s*=\\s*r#"([\\s\\S]*?)"#;'))[1].replace('__APPROVAL_SCAN__',()=>fs.readFileSync(path.join(__dirname,'../crates/adapters/chatgpt-cdp/src/approval-scan.js'),'utf8')).replace('__APPROVAL_RECEIPTS__',()=>JSON.stringify(receipts)).replace('__APPROVAL_TARGET__',()=>JSON.stringify(target));
  const browser = await chromium.launch({headless:true, ...(process.env.CHROME_BINARY ? {executablePath:process.env.CHROME_BINARY} : process.platform==='darwin' ? {executablePath:'/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'} : {})});
  try {
    const page=await browser.newPage();
    const context=(token,receipts=[])=>page.evaluate('(() => { const page = '+script('SNAPSHOT_SCRIPT')+'; return ('+native('CONTEXT_SCRIPT',receipts).replace('__TOKEN__',JSON.stringify('[Fabushi:'+token+']'))+'); })()');
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
    const approvalCard=id=>`<section data-approval-request-id="${id}"><p>GitHub permission</p><button>Reject</button><button onclick="window.clickedRequest='${id}'">Allow once</button></section>`;
    await page.setContent('<main><div data-content-search-unit-key="u:user">[Fabushi:owned]</div><article data-content-search-turn-key="a">'+approvalCard('first')+'<p id="reply">Working</p></article></main>');
    const first=(await context('owned')).approval_fingerprint;
    assert.equal((await page.evaluate(native('SCOPED_APPROVE_SCRIPT',[],first))).clicked,true);
    await page.locator('#reply').evaluate(e=>e.textContent='Final answer with much more text');
    await page.locator('section').evaluate(e=>e.outerHTML=e.outerHTML);
    assert.equal((await context('owned')).approval_fingerprint,first);
    assert.equal((await context('owned',[first])).approval_present,true);
    await page.locator('article').evaluate(e=>e.insertAdjacentHTML('beforeend','<div data-content-search-unit-key="a:assistant" data-is-streaming="false">Completed reply<button aria-label="Copy">Copy</button><button aria-label="Share">Share</button></div>'));
    let ignored=await context('owned',[first]);
    assert.equal(ignored.approval_present,false);assert.equal(ignored.page,undefined);
    // The second request has identical wording but a distinct request id.
    await page.locator('article').evaluate((e,html)=>e.insertAdjacentHTML('beforeend',html),approvalCard('second'));
    const second=(await context('owned',[first])).approval_fingerprint;
    assert.notEqual(second,first);
    await page.evaluate(()=>window.clickedRequest='');
    assert.equal((await page.evaluate(native('SCOPED_APPROVE_SCRIPT',[],second))).clicked,true);
    assert.equal(await page.evaluate(()=>window.clickedRequest),'second');
    await page.locator('[data-approval-request-id="second"]').evaluate(e=>e.remove());
    assert.equal((await page.evaluate(native('SCOPED_APPROVE_SCRIPT',[],second))).clicked,false);
    await page.locator('main').evaluate(e=>e.insertAdjacentHTML('beforeend','<div data-content-search-unit-key="next:user">Next turn [Fabushi:owned]</div>'));
    assert.equal((await context('owned')).approval_present,false);
    console.log('PASS stable approval identity, remount and final-answer deduplication, distinct identical requests, target recheck and historical exclusion');
    await page.setContent('<main><div data-content-search-unit-key="u:user">[Fabushi:owned]</div><article data-content-search-turn-key="a"><div><button>Reject</button><button>Allow once</button></div><p id="reply">Working</p></article></main>');
    const fallback=(await context('owned')).approval_fingerprint;
    await page.locator('#reply').evaluate(e=>e.textContent='Final answer: unrelated changing content');
    assert.equal((await context('owned')).approval_fingerprint,fallback);
    assert.equal((await context('owned',[fallback])).approval_present,true);
    await page.locator('article').evaluate(e=>e.insertAdjacentHTML('beforeend','<div data-content-search-unit-key="a:assistant" data-is-streaming="false">Completed reply<button aria-label="Copy">Copy</button><button aria-label="Share">Share</button></div>'));
    assert.equal((await context('owned',[fallback])).approval_present,false);
    await page.locator('article').evaluate(e=>e.setAttribute('data-content-search-turn-key','b'));
    assert.notEqual((await context('owned',[fallback])).approval_fingerprint,fallback);
    await page.locator('article').evaluate(e=>e.setAttribute('data-approval-state','resolved'));
    assert.equal((await context('owned')).approval_present,false);
    console.log('PASS missing-id fallback ignores reply changes, new message remains actionable and explicit resolved state is excluded');


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

    await page.setContent('<div id="sidebar" style="height:100px;overflow-y:auto;width:220px"><div role="button" onclick="window.unrelatedOpened=true"><span data-thread-title="true">Unrelated</span></div><div style="height:1000px"></div></div>');
    await page.evaluate(()=>{
      window.unrelatedOpened=false;window.requestAnimationFrame=()=>0;
      const list=document.querySelector('#sidebar');const row=list.querySelector('[role="button"]');row.__reactFiber$fixture={memoizedProps:{conversation:{id:'unrelated-id'}},return:null};
      list.addEventListener('scroll',()=>{
        if(list.scrollTop>=100 && !window.mountedOwned){window.mountedOwned=true;
          list.innerHTML='<div style="height:160px"></div><div role="button"><span data-thread-title="true">Owned task</span></div><div style="height:840px"></div>';
          list.querySelector('[role="button"]').__reactFiber$fixture={memoizedProps:{conversation:{id:'owned-id'}},return:null};
        }
      });
    });
    result=await Promise.race([page.evaluate(native('SWITCH_SCRIPT').replace('__BINDING__',JSON.stringify({identity:'owned-id',sidebar_id:'owned-id',title:'task'}))),new Promise((_,reject)=>setTimeout(()=>reject(Error('sidebar locator depended on paused animation frames')),3000))]);
    assert.equal(result.needs_click,true);assert.ok(result.scroll_steps>0);
    assert.equal(await page.evaluate(()=>!!window.unrelatedOpened),false);
    await page.setContent('<button aria-label="Show sidebar">Show</button><div id="hidden-sidebar" style="display:none"><div role="button"><span data-thread-title="true">Owned</span></div></div>');
    await page.locator('button[aria-label="Show sidebar"]').evaluate(e=>e.onclick=()=>document.querySelector('#hidden-sidebar').style.display='block');
    await page.locator('#hidden-sidebar [role="button"]').evaluate(e=>e.__reactFiber$fixture={memoizedProps:{conversation:{id:'owned-id'}},return:null});
    result=await page.evaluate(native('SWITCH_SCRIPT').replace('__BINDING__',JSON.stringify({identity:'owned-id',sidebar_id:'owned-id',title:'task'})));
    assert.equal(result.needs_click,true);assert.equal(result.opened_sidebar,true);
    console.log('PASS virtual sidebar mounts exact recorded id, bounded background wait, collapsed sidebar and no unrelated chat clicks');
    await page.setContent('<div id="history" style="height:100px;overflow-y:auto;width:220px"><a href="/c/other"><span data-thread-title="true">Other</span></a><div style="height:120px"></div></div>');
    await page.evaluate(()=>{
      const list=document.querySelector('#history');
      list.addEventListener('scroll',()=>{
        if(list.scrollTop+list.clientHeight>=list.scrollHeight && !window.historyLoading){
          window.historyLoading=true;
          setTimeout(()=>list.insertAdjacentHTML('beforeend','<a href="/c/older-owned"><span data-thread-title="true">Older owned</span></a>'),600);
        }
      });
    });
    result=await page.evaluate(native('SWITCH_SCRIPT').replace('__BINDING__',JSON.stringify({identity:'older-owned',sidebar_id:'older-owned',title:'older'})));
    assert.equal(result.needs_click,true);
    assert.equal(await page.locator('a[href="/c/older-owned"]').count(),1);
    console.log('PASS sidebar waits for delayed history pagination before declaring chat absent');
    await page.setContent('<main><div data-content-search-unit-key="u:user">[Fabushi:large-fixture]</div><div data-content-search-unit-key="a:assistant">public reply</div></main><div id="unrelated-bulk"></div>');
    await page.locator('#unrelated-bulk').evaluate(el=>{el.innerHTML='<div><span>ordinary text</span></div>'.repeat(5000);for(const node of [el,...el.querySelectorAll('*')])Object.defineProperty(node,'innerText',{get(){throw new Error('loading detection swept unrelated content');}});});
    const largeObservation=await context('large-fixture');
    assert.equal(largeObservation.load_failed,false);
    assert.equal((await page.evaluate(native('RETRY_LOAD_SCRIPT'))).clicked,false);
    await page.locator('[data-content-search-unit-key="u:user"]').evaluate(e=>{e.textContent='short public text';const props={};Object.defineProperty(props,'message',{get(){throw new Error('reactive message getter invoked');}});Object.defineProperty(props,'text',{get(){throw new Error('reactive text getter invoked');}});e.__reactFiber$unsafe={memoizedProps:props,return:null};});
    assert.equal((await context('large-fixture')).marker_present,false);
    await page.locator('[data-content-search-unit-key="u:user"]').evaluate(e=>e.textContent='[Fabushi:large-fixture]');
    assert.equal((await context('large-fixture')).marker_present,true);
    await page.setContent('<main><article data-content-search-turn-key="shared-turn"><div data-content-search-unit-key="u:user">LONG_ORIGINAL_GOAL [Fabushi:shared]</div><div data-content-search-unit-key="a:assistant"><p>assistant only progress</p></div></article></main>');
    const sharedTurn=await context('shared');
    assert.equal(sharedTurn.marker_present,true);
    assert.equal(sharedTurn.visible_work.includes('LONG_ORIGINAL_GOAL'),false);
    assert.equal(sharedTurn.visible_work.includes('assistant only progress'),true);
    console.log('PASS marker proof reads DOM and plain metadata without invoking reactive message getters');
    console.log('PASS bounded load detection ignores 10000 unrelated nodes without reading their innerText');
    console.log('PASS native role-less Work card, countdown, quoted exclusion and load error outside cached main');
  } finally {await browser.close();}
})().catch(error=>{console.error(error);process.exitCode=1;});
