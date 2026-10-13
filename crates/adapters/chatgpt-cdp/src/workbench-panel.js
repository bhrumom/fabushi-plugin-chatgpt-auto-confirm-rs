(() => {
  const snapshot = __SNAPSHOT_JSON__;
  let b = window.__fabushiWorkbench;
  const labels={queued:'排队',preparing:'准备',sending:'发送中',generating:'生成中',approval:'等待授权',waiting:'等待',reviewing:'验收中',cooldown:'冷却',recovering:'恢复中',paused:'已暂停',cancelled:'已取消',done:'已完成'};
  const terminal=t=>t && ['done','cancelled'].includes(t.state);
  const removable=t=>t && (terminal(t)||t.state==='paused');
  if (!b || b.version !== 2) {
    const previous={draft:b?.root?.getElementById('goal')?.value||'',title:b?.root?.getElementById('title')?.value||'',commands:b?.commands||[],open:b?.root?.getElementById('panel')?.style.display==='block'};
    if(b){clearInterval(b.timer);b.observer?.disconnect();b.host.remove();}
    const host=document.createElement('div');host.id='fabushi-workbench-entry';
    const root=host.attachShadow({mode:'open'});
    root.innerHTML=`<style>__PANEL_CSS__</style><section id="panel" class="desk" aria-label="Fabushi 任务工作台"><aside><h3>Fabushi</h3><div id="tasks"></div><div id="workspace-editor" class="workspace-editor"><input id="workspace-name" placeholder="新工作区名称"><button id="workspace-save">移动</button></div><small class="drop-hint">拖动任务分组；所有工作区共用一个桌面实例</small></aside><div class="chat"><header><strong id="heading">任务工作台</strong><button id="edit">编辑目标</button><button id="settings-toggle">设置</button><button id="pause-current">暂停当前任务</button><button id="close" aria-label="收起任务工作台">×</button></header><div id="settings" class="settings"><label><input id="global-auto" type="checkbox">在受控桌面会话中自动处理授权卡</label><small>仅选择允许本次会话或允许一次；不会选择永久授权。</small><div class="tools"><button data-global="pause_all">暂停全部任务</button><button data-global="resume_all">继续全部任务</button><button id="cleanup" class="memory-cleanup">清理当前桌面内存</button><button data-global="open_tui">打开完整 TUI</button><button data-global="export">导出任务</button><button id="import">导入任务</button><input id="import-file" type="file" accept=".json,application/json" hidden></div><small id="memory-status" class="memory-status"></small><small>安全重载会保留数据库和会话归属；回复、授权、未保存文字或附件存在时等待。浏览器标签页卸载在单桌面实例中由安全重载替代。</small></div><div id="feed" class="feed" role="log" aria-live="polite"></div><div id="health" class="notice"></div><div id="notice" class="notice"></div><form id="compose" class="compose"><input id="title" type="text" placeholder="任务名称（可选）" maxlength="200"><textarea id="goal" aria-label="任务目标" placeholder="输入任务目标，可直接粘贴图片或视频…"></textarea><div class="attachment-box"><label class="attachment-picker"><input id="file-input" type="file" multiple aria-label="添加任务附件"><span>＋ 添加图片 / 视频 / 文件</span></label><button id="clear-files" type="button">清空附件</button><div id="files" class="attachment-list"></div><small class="attachment-note">附件私密保存在本机；完成上传后再发送目标。单文件 20 MiB，合计 50 MiB。</small></div><div class="tools"><select id="mode" aria-label="任务模式"><option value="once">单次任务</option><option value="goal">持续目标</option></select><select id="model" aria-label="ChatGPT 模型"><option>GPT-5.6 Sol</option><option>GPT-6</option><option>GPT-5.5</option></select><select id="effort" aria-label="ChatGPT 思考强度"><option value="0">即时</option><option value="1">中</option><option value="2">高</option><option value="3">极高</option><option value="4">Pro（第 5 档）</option></select><label><input id="auto" type="checkbox">本次会话自动授权</label><button id="send" class="send" type="submit" aria-label="发送任务">↑</button></div><small id="scope-hint"></small><div class="attachment-actions"><button id="attach-existing" type="button">添加到当前任务</button><button id="cancel-edit" type="button" hidden>取消编辑</button></div></form></div></section><button id="entry" class="launch" aria-label="打开 Fabushi 脚本工作台">⚡ Fabushi 脚本</button>`;
    b={version:2,host,root,commands:previous.commands,next:1,selected:snapshot.settings?.ui_selected_task||'',editing:'',files:[],urls:[],pending:new Map(),lastNotice:'',profileKey:'',snapshot,updated:Date.now()};
    window.__fabushiWorkbench=b;
    const q=id=>root.getElementById(id);
    const node=(tag,text='',className='')=>{const n=document.createElement(tag);n.textContent=text;n.className=className;return n;};
    b.node=node;b.q=q;
    const report=text=>q('notice').textContent=text;
    const enqueue=command=>{
      if(b.commands.length>=50){report('操作队列已满，请等待处理');return '';}
      const request_id=Date.now()+'-'+b.next++;
      b.commands.push({...command,request_id});b.pending.set(request_id,command);
      report('操作已提交，等待 Rust 监督器处理…');return request_id;
    };b.enqueue=enqueue;
    const selected=()=>b.snapshot.tasks.find(t=>t.id===b.selected);
    const show=open=>{q('panel').classList.toggle('open',open);q('panel').style.display=open?'flex':'none';};
    q('entry').onclick=()=>show(!q('panel').classList.contains('open'));q('close').onclick=()=>show(false);show(previous.open);
    q('settings-toggle').onclick=()=>q('settings').classList.toggle('open');
    root.querySelectorAll('[data-global]').forEach(button=>button.onclick=()=>enqueue({action:button.dataset.global}));
    q('global-auto').onchange=()=>enqueue({action:'global_auto',auto:q('global-auto').checked});
    q('pause-current').onclick=()=>{const t=selected();enqueue(t&&t.state!=='done'?{action:t.state==='paused'||t.state==='cancelled'?'resume':'pause',id:t.id}:{action:b.snapshot.running?'pause_all':'resume_all'});};
    q('cleanup').onclick=()=>{
      if(q('goal').value.trim()||b.files.length){report('请先提交或清空工作台草稿与附件，再清理内存');return;}
      enqueue({action:'cleanup'});
    };
    q('edit').onclick=()=>{const t=selected();if(!t||t.state==='done')return;b.editing=t.id;q('goal').value=t.goal;q('cancel-edit').hidden=false;report('编辑当前目标；已发送轮次保留原配置，后续轮次使用新目标');q('goal').focus();};
    q('cancel-edit').onclick=()=>{b.editing='';q('cancel-edit').hidden=true;report('已退出编辑；输入仍保留');};
    const defaults=()=>({action:'defaults',mode:q('mode').value,model:q('model').value,effort:Number(q('effort').value),auto:q('auto').checked});
    for(const field of ['mode','model','effort','auto'])q(field).onchange=()=>{
      const t=selected();
      if(t&&t.state!=='done'&&field!=='mode')enqueue({action:field,id:t.id,model:q('model').value,effort:Number(q('effort').value),auto:q('auto').checked});
      else enqueue(defaults());
    };
    const revoke=()=>{for(const u of b.urls)URL.revokeObjectURL(u);b.urls=[];};
    const paintFiles=()=>{
      revoke();q('files').replaceChildren();
      for(const f of b.files){const p=node('div','','attachment-preview');
        if(/^image\/|^video\//.test(f.type)){const u=URL.createObjectURL(f);b.urls.push(u);const media=document.createElement(f.type.startsWith('image/')?'img':'video');media.src=u;media.title=f.name;if(media.tagName==='IMG')media.alt=f.name;else{media.controls=true;media.muted=true;media.preload='metadata';}p.append(media);}
        p.append(node('span',f.name+' · '+(f.size/1024).toFixed(1)+' KB','attachment-chip'));q('files').append(p);
      }q('clear-files').disabled=!b.files.length;
    };b.paintFiles=paintFiles;
    const addFiles=files=>{
      const all=[...b.files,...Array.from(files)].filter((f,i,a)=>a.findIndex(x=>x.name===f.name&&x.size===f.size&&x.lastModified===f.lastModified)===i);
      if(all.length>32||all.some(f=>f.size>20*1024*1024)||all.reduce((sum,f)=>sum+f.size,0)>50*1024*1024){report('附件超出上限：32 个，单文件 20 MiB，合计 50 MiB');return;}
      b.files=all;paintFiles();
    };
    q('file-input').onchange=()=>{addFiles(q('file-input').files);q('file-input').value='';};
    q('clear-files').onclick=()=>{b.files=[];paintFiles();};paintFiles();
    q('compose').addEventListener('paste',event=>{const files=Array.from(event.clipboardData?.files||[]);if(files.length){if(!event.clipboardData.getData('text/plain'))event.preventDefault();addFiles(files);}});
    q('compose').addEventListener('dragover',e=>{if(e.dataTransfer?.types.includes('Files'))e.preventDefault();});
    q('compose').addEventListener('drop',e=>{if(e.dataTransfer?.files.length){e.preventDefault();addFiles(e.dataTransfer.files);}});
    const encodeFiles=async files=>Promise.all(files.map(async f=>{const data=await new Promise((resolve,reject)=>{const reader=new FileReader();reader.onload=()=>resolve(String(reader.result).split(',')[1]);reader.onerror=()=>reject(new Error('读取附件失败'));reader.readAsDataURL(f);});return{name:f.name,data};}));
    const submit=async action=>{
      if(q('send').disabled)return;
      const goal=q('goal').value.trim();if(action!=='attach'&&!goal){report('请先输入目标');return;}
      if(goal.length>200000){report('目标超过 200000 字符');return;}
      if(action==='attach'&&(!selected()||!b.files.length)){report('先选择任务和附件');return;}
      q('send').disabled=true;b.pendingGoal=q('goal').value;b.pendingFiles=b.files.slice();
      try{enqueue({action:b.editing&&action!=='attach'?'edit':action,id:b.editing||b.selected,goal,title:q('title').value.trim(),mode:q('mode').value,model:q('model').value,effort:Number(q('effort').value),auto:q('auto').checked,files:b.editing&&action!=='attach'?[]:await encodeFiles(b.pendingFiles)});}
      catch(error){report(error.message);q('send').disabled=false;}
    };
    q('compose').onsubmit=e=>{e.preventDefault();submit('add');};q('attach-existing').onclick=()=>submit('attach');
    q('import').onclick=()=>q('import-file').click();q('import-file').onchange=async()=>{const f=q('import-file').files[0];if(!f)return;if(f.size>10000000){report('导入文件过大');return;}try{enqueue({action:'import',value:await f.text()});}catch(e){report(e.message);}q('import-file').value='';};
    b.newWorkspace=id=>{b.moveTask=id;q('workspace-editor').classList.add('open');q('workspace-name').focus();};
    q('workspace-save').onclick=()=>{const value=q('workspace-name').value.trim();if(value&&b.moveTask){enqueue({action:'workspace',id:b.moveTask,value});q('workspace-editor').classList.remove('open');}};
    q('goal').value=previous.draft;q('title').value=previous.title;
    const repair=()=>{b.observer.disconnect();const parent=document.body||document.documentElement;if(host.parentNode!==parent)parent.append(host);for(const anchor of new Set([document,document.documentElement,document.body].filter(Boolean)))b.observer.observe(anchor,{childList:true});};
    b.observer=new MutationObserver(repair);repair();
    b.timer=setInterval(()=>{if(Date.now()-b.updated>30000)q('health').textContent='监督器暂未更新；保留任务、输入与附件，等待恢复';},5000);
  }
  b.render=()=>{
  const snapshot=b.snapshot;
  const {root}=b,q=b.q,node=b.node;
  const tasks=snapshot.tasks||[];
  let task=tasks.find(t=>t.id===b.selected);
  if(b.selected&&!tasks.some(t=>t.id===b.selected))b.selected='';
  const settings=snapshot.settings||{default_profile:{model:'GPT-5.6 Sol',effort:3},default_mode:'goal',default_auto_approve:true,global_auto_approve:true};
  if(snapshot.notice&&snapshot.notice.request_id!==b.lastNotice){
    const n=snapshot.notice;b.lastNotice=n.request_id;q('notice').textContent=n.text;
    b.pending.delete(n.request_id);
    if(['add','edit','attach'].includes(n.action)){
      q('send').disabled=false;
      if(n.ok){if(n.action!=='attach'&&q('goal').value===b.pendingGoal)q('goal').value='';if(n.action!=='edit'&&b.files.length===b.pendingFiles?.length&&b.files.every((f,i)=>f===b.pendingFiles[i])){b.files=[];b.paintFiles();}if(n.action==='add'){b.selected=n.result?.task_id||'';q('title').value='';}if(n.action==='edit'){b.editing='';q('cancel-edit').hidden=true;}}
    }
    if(n.ok&&n.result?.download){const blob=new Blob([JSON.stringify(n.result.download,null,2)],{type:'application/json'});const url=URL.createObjectURL(blob),link=document.createElement('a');link.href=url;link.download='fabushi-tasks.json';link.click();setTimeout(()=>URL.revokeObjectURL(url),1000);}
  }
  const sidebarSig=JSON.stringify(tasks.map(t=>[t.id,t.title,t.state,t.round,t.phase,t.workspace,t.last_error,t.profile]));
  if(b.sidebarSig!==sidebarSig||b.sidebarSelection!==b.selected){
    b.sidebarSig=sidebarSig;b.sidebarSelection=b.selected;q('tasks').replaceChildren();
    const fresh=node('button','＋ 新任务');fresh.onclick=()=>{b.selected='';b.editing='';b.profileKey='';b.files=[];b.paintFiles();q('cancel-edit').hidden=true;q('goal').focus();b.sidebarSig='';b.enqueue({action:'select',id:''});b.render();};q('tasks').append(fresh);
    const drop=node('button','＋ 拖到这里，移入新工作区','task-drop-new');drop.ondragover=e=>e.preventDefault();drop.ondrop=e=>{e.preventDefault();const id=e.dataTransfer.getData('application/x-fabushi-task');if(tasks.some(t=>t.id===id))b.newWorkspace(id);};q('tasks').append(drop);
    const workspaces=[...new Set(tasks.map(t=>t.workspace||'桌面工作区'))];
    for(const workspace of workspaces){const group=node('div','','task-group');const members=tasks.filter(t=>(t.workspace||'桌面工作区')===workspace);group.append(node('div',workspace+' · '+members.length,'task-group-title'));group.ondragover=e=>e.preventDefault();group.ondrop=e=>{e.preventDefault();const id=e.dataTransfer.getData('application/x-fabushi-task');if(tasks.some(t=>t.id===id))b.enqueue({action:'workspace',id,value:workspace});};
      for(const t of members){const row=node('div','','task-row task'+(b.selected===t.id?' selected':''));row.draggable=true;row.ondragstart=e=>{e.dataTransfer.setData('application/x-fabushi-task',t.id);};
        const choose=node('button','','task-select');choose.setAttribute('aria-label','查看任务详情：'+t.title);choose.append(node('span',t.title,'task-name'));const meta=node('span','第 '+t.round+' 轮 · '+(t.phase==='review'?'验收':'执行')+' · ','task-meta');const badge=node('span',labels[t.state]||t.state,'state-badge');badge.dataset.state=t.state;meta.append(badge);choose.append(meta);choose.onclick=()=>{b.selected=t.id;b.profileKey='';b.sidebarSig='';b.enqueue({action:'select',id:t.id});b.render();};row.append(choose);
        const actions=node('div','','task-row-actions');for(const [action,text]of [['pause','暂停'],['resume','继续'],['delete','删除']]){const button=node('button',text,'task-action');button.disabled=action==='delete'?!removable(t):action==='pause'?terminal(t)||t.state==='paused':t.state!=='paused'&&t.state!=='cancelled';button.onclick=()=>b.enqueue({action,id:t.id});actions.append(button);}
        const move=document.createElement('select');move.className='task-move';move.setAttribute('aria-label','移动任务工作区');for(const value of [...workspaces,'＋ 新工作区']){const o=node('option',value);o.value=value;move.append(o);}move.value=workspace;move.onchange=()=>move.value==='＋ 新工作区'?b.newWorkspace(t.id):b.enqueue({action:'workspace',id:t.id,value:move.value});row.append(actions,move);group.append(row);
      }q('tasks').append(group);
    }
  }
  task=tasks.find(t=>t.id===b.selected);
  q('heading').textContent=task?(task.mode==='goal'?'持续目标':'单次任务')+' · '+(labels[task.state]||task.state):'任务工作台';
  q('edit').disabled=!task||task.state==='done';q('attach-existing').disabled=!task||task.state==='done';
  q('pause-current').textContent=task&&task.state!=='done'?(task.state==='paused'||task.state==='cancelled'?'继续当前任务':'暂停当前任务'):(snapshot.running?'暂停全部':'继续全部');
  const profileKey=(task?.id||'new')+':'+JSON.stringify(task?.state!=='done'&&task?task.profile:settings.default_profile);
  if(b.profileKey!==profileKey&&!['model','effort'].some(action=>[...b.pending.values()].some(c=>c.action===action))){b.profileKey=profileKey;const profile=task?.state!=='done'&&task?task.profile:settings.default_profile;q('model').value=profile.model;q('effort').value=String(profile.effort);q('mode').value=task?.mode||settings.default_mode;q('auto').checked=task?task.auto_approve:settings.default_auto_approve;}
  if(![...b.pending.values()].some(c=>c.action==='global_auto'))q('global-auto').checked=settings.global_auto_approve;
  q('scope-hint').textContent=task&&task.state!=='done'?'当前任务模型与授权 · 修改后下一次尚未发送的轮次生效':'新任务默认设置 · 不影响已创建任务';
  const heap=performance.memory?.usedJSHeapSize;
  q('memory-status').textContent=`脚本堆 ${heap?(heap/1024/1024).toFixed(1)+' MiB':'指标不可用'} · 数据库 ${(Number(snapshot.storage_bytes||0)/1024/1024).toFixed(2)} MiB · 扫描 ${snapshot.scans||0} 次 · 最近扫描 ${settings.last_scan_ms||0} ms · 确认切换 ${settings.switches||0} 次`;
  const age=task?.last_scan_at?Math.max(0,Math.round((snapshot.now-task.last_scan_at)/1000))+' 秒前':'未扫描';const cool=task?.cooldown_until>snapshot.now?' · '+Math.ceil((task.cooldown_until-snapshot.now)/1000)+' 秒后重试':'';
  q('health').textContent=`单桌面实例 · ${snapshot.running?'监督中':'已暂停'} · ${snapshot.worker?'工作进程在线':'等待工作进程'} · ${task?'第 '+task.round+' 轮 '+(task.phase==='review'?'验收':'执行')+' · 上次扫描 '+age+cool:tasks.length+' 个任务'}${task?.last_error?' · 恢复原因：'+task.last_error:''}`;
  const feedSig=JSON.stringify([b.selected,task?.updated_at,task?.latest_reply,task?.goal,snapshot.activity,snapshot.operations]);
  if(b.feedSig!==feedSig){b.feedSig=feedSig;const feed=q('feed'),nearBottom=feed.scrollHeight-feed.scrollTop-feed.clientHeight<80;feed.replaceChildren();
    if(!task)feed.append(node('p','在下方输入任务。单次任务等待一次最终回复；持续目标在执行和独立规划/验收之间推进。附件完成上传后再发送目标。任务按记录的唯一会话恢复。'));
    else{
      feed.append(node('div','当前目标：'+task.goal,'goal'));feed.append(node('div',task.profile.model+' · '+['即时','中','高','极高','Pro'][task.profile.effort]+' · 授权 '+task.approvals+' 次 · 恢复 '+task.recoveries+' 次','muted'));
      if(task.attachments?.length)feed.append(node('div','任务附件：'+task.attachments.map(f=>f.name+' · '+(f.size/1024).toFixed(1)+' KB').join('；'),'attachment-summary'));
      const messages=(snapshot.activity||[]).filter(m=>m.task_id===task.id);feed.append(node('small','最近 2 小时记录：保留 '+messages.length+' 条，显示最近 '+Math.min(messages.length,100)+' 条 · '+(task.last_error||'')));
      for(const m of messages.slice(-100)){const bubble=node('div',m.text,'bubble '+m.role);bubble.append(node('time',new Date(m.at).toLocaleTimeString()));feed.append(bubble);}
      if(task.latest_reply)feed.append(node('div','最新公开回复\n'+task.latest_reply,'bubble assistant'));
      if(task.binding){feed.append(node('div','会话（第 '+task.round+' 轮 · '+(task.phase==='review'?'验收':'执行')+'）：'+task.binding.sidebar_id,'session-link'));const open=node('button','打开已记录会话');open.onclick=()=>b.enqueue({action:'open',id:task.id});feed.append(open);}
      for(const [action,text]of [['retry','检查已有回复 / 重试附件（不重发）'],['cancel','取消此任务'],['delete','删除此任务'],['clear_attachments','清空任务附件']]){const button=node('button',text);button.disabled=action==='delete'?!removable(task):action==='cancel'?terminal(task):false;button.onclick=()=>b.enqueue({action,id:task.id});feed.append(button);}
    }
    const detail=node('details');detail.open=true;detail.append(node('summary','过去一小时的具体操作'));const filter=document.createElement('select');filter.id='log-filter';for(const [value,text]of [['all','全部任务'],['selected','当前任务']]){const o=node('option',text);o.value=value;filter.append(o);}filter.value=b.logFilter||'all';const logs=node('div','','history');logs.id='logs';const paint=()=>{b.logFilter=filter.value;const ops=(snapshot.operations||[]).filter(m=>filter.value!=='selected'||m.task_id===b.selected);logs.textContent=ops.map(m=>new Date(m.at).toLocaleTimeString()+' ['+m.task_id+'] '+m.text).join('\n')||'暂无记录';};filter.onchange=paint;paint();detail.append(filter,logs);feed.append(detail);if(nearBottom)feed.scrollTop=feed.scrollHeight;
  }
  };
  b.snapshot=snapshot;b.updated=Date.now();b.render();
  return {commands:snapshot.notice?[]:b.commands.splice(0),unsaved:!!(b.q('goal').value.trim()||b.files.length)};
})()
