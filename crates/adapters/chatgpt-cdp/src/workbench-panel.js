(() => {
  const snapshot = __SNAPSHOT_JSON__;
  let bridge = window.__fabushiWorkbench;
  if (!bridge || !bridge.host.isConnected) {
    const host = document.createElement('div');
    host.id = 'fabushi-workbench-entry';
    host.style.cssText = 'position:fixed;right:22px;top:72px;z-index:2147483646;';
    document.body.append(host);
    const root = host.attachShadow({mode:'open'});
    root.innerHTML = `<style>
      :host{font:14px system-ui;color:#eee}button,input,textarea,select{font:inherit}button{cursor:pointer;background:#353535;color:#eee;border:1px solid #555;border-radius:8px;padding:7px 11px}button:hover{background:#494949}button:disabled{opacity:.5;cursor:default}#entry{background:#245442;border-color:#42866c;box-shadow:0 2px 12px #0008}#panel{display:none;position:fixed;right:22px;top:115px;width:min(680px,calc(100vw - 44px));max-height:calc(100vh - 150px);overflow:auto;background:#202020;border:1px solid #555;border-radius:14px;padding:18px;box-shadow:0 8px 32px #0009;box-sizing:border-box}h2{font-size:18px;margin:0}header,.tools{display:flex;gap:8px;align-items:center;flex-wrap:wrap;margin-bottom:12px}header button{margin-left:auto}#health,#notice,.muted{color:#aaa;white-space:pre-wrap;margin:8px 0}textarea,input,select{background:#171717;color:#eee;border:1px solid #555;border-radius:7px;padding:9px;box-sizing:border-box}textarea,input{width:100%;margin:5px 0}textarea{height:110px;resize:vertical}#tasks{display:grid;gap:8px;margin:12px 0}.task{border:1px solid #454545;border-radius:9px;padding:10px;white-space:pre-wrap;overflow-wrap:anywhere}.task button{margin:6px 6px 0 0}#logs{height:230px;overflow:auto;white-space:pre-wrap;overflow-wrap:anywhere;background:#151515;border-radius:8px;padding:10px;font-size:12px}details{margin-top:14px}#notice{color:#b7e4ce}
    </style><button id="entry" aria-label="打开 Fabushi 脚本工作台">◉ Fabushi 工作台</button><section id="panel" aria-label="Fabushi 脚本工作台"><header><h2>Fabushi 脚本工作台</h2><button id="close">关闭</button></header><div id="health"></div><div class="tools"><button data-global="resume_all">继续监督</button><button data-global="pause_all">暂停监督</button><button data-global="open_tui">打开完整 TUI</button></div><div id="tasks"></div><details><summary>输入目标并派发</summary><input id="title" placeholder="任务名称（可选）" maxlength="200"><textarea id="goal" placeholder="输入完整目标；目标模式会持续执行并独立验收"></textarea><div class="tools"><select id="mode"><option value="goal">持续目标＋独立验收</option><option value="once">执行一次</option></select><select id="model"><option>GPT-5.6 Sol</option><option>GPT-6</option><option>GPT-5.5</option></select><select id="effort"><option value="4">极高</option><option value="3">高</option><option value="2">中</option><option value="1">低</option><option value="0">默认</option></select><button id="send">派发目标</button></div></details><div id="notice"></div><details open><summary id="log-title">过去一小时的操作</summary><div id="logs"></div></details><div class="muted">本机 Rust 监督器 · 操作与 TUI 共用同一任务数据库</div></section>`;
    bridge = {host, root, commands: [], next: 1};
    window.__fabushiWorkbench = bridge;
    const q = id => root.getElementById(id);
    const enqueue = command => {
      if (bridge.commands.length >= 50) { q('notice').textContent = '操作队列已满，请等待监督器处理'; return; }
      bridge.commands.push({...command, request_id: String(Date.now()) + '-' + bridge.next++});
      q('notice').textContent = '已提交操作，等待 Rust 监督器处理…';
    };
    q('entry').onclick = () => { q('panel').style.display = q('panel').style.display === 'block' ? 'none' : 'block'; };
    q('close').onclick = () => { q('panel').style.display = 'none'; };
    root.querySelectorAll('[data-global]').forEach(button => button.onclick = () => enqueue({action:button.dataset.global}));
    q('send').onclick = () => {
      const goal = q('goal').value.trim();
      if (!goal) { q('notice').textContent = '请先输入目标'; return; }
      if (goal.length > 200000) { q('notice').textContent = '目标超过 200000 字符，请使用完整 TUI 导入文件'; return; }
      enqueue({action:'add',goal,title:q('title').value.trim(),mode:q('mode').value,model:q('model').value,effort:Number(q('effort').value)});
      bridge.pendingGoal = goal; q('send').disabled = true;
    };
    bridge.enqueue = enqueue;
    bridge.timer = setInterval(() => {
      if (!host.isConnected) { clearInterval(bridge.timer); return; }
      if (bridge.updated && Date.now() - bridge.updated > 30000) q('health').textContent = '监督器暂未更新；保留任务和输入，等待恢复';
    }, 5000);
  }
  const {root} = bridge;
  const q = id => root.getElementById(id);
  bridge.updated = Date.now();
  q('health').textContent = `${snapshot.running ? '监督已开启' : '监督已暂停'} · ${snapshot.tasks.length} 个任务 · 累计 ${snapshot.scans} 次扫描 · 更新 ${new Date().toLocaleTimeString()}`;
  if (snapshot.notice) {
    q('notice').textContent = snapshot.notice.text;
    if (snapshot.notice.action === 'add') {
      q('send').disabled = false;
      if (snapshot.notice.ok && q('goal').value.trim() === bridge.pendingGoal) { q('goal').value = ''; q('title').value = ''; }
    }
  }
  const signature = JSON.stringify(snapshot.tasks);
  if (bridge.signature !== signature) {
    bridge.signature = signature;
    q('tasks').replaceChildren();
    for (const task of snapshot.tasks) {
      const row = document.createElement('div'); row.className = 'task';
      const label = document.createElement('div');
      label.textContent = `${task.title} · 第 ${task.round} 轮 ${task.phase} · ${task.state}\n授权 ${task.approvals} 次 · 恢复 ${task.recoveries} 次${task.error ? '\n恢复原因：' + task.error : ''}`;
      row.append(label);
      for (const [action, text] of [['open','查看会话'],['pause','暂停'],['resume','继续'],['retry','立即重试']]) {
        const button = document.createElement('button'); button.textContent = text;
        button.onclick = () => bridge.enqueue({action,id:task.id}); row.append(button);
      }
      q('tasks').append(row);
    }
  }
  q('log-title').textContent = `过去一小时的操作（${snapshot.operations.length} 条）`;
  const log = q('logs'), atBottom = log.scrollTop + log.clientHeight >= log.scrollHeight - 30;
  log.textContent = snapshot.operations.map(event => `${new Date(event.at).toLocaleTimeString()} [${event.task_id}] ${event.text}`).join('\n') || '暂无操作记录';
  if (atBottom) log.scrollTop = log.scrollHeight;
  const commands = snapshot.notice ? [] : bridge.commands.splice(0);
  return {commands};
})()
