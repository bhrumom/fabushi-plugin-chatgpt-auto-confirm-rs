const q = id => document.getElementById(id);
const MAX_GOAL_BYTES = 16 * 1024;
let ready = false, tasks = [], reviews = [], snapshots = [];
let max_tasks, build_task_prompt, verify_review_report, evaluate_snapshot;
const textRow = (list, text) => { const row = document.createElement('li'); row.textContent = text; list.append(row); };

function controls() {
  const hasTasks = tasks.length > 0;
  q('add-task').disabled = !ready;
  q('reset-queue').disabled = !hasTasks;
  q('verify-review').disabled = !ready || !hasTasks;
  q('verify-snapshot').disabled = !ready;
  q('export').disabled = !hasTasks && !reviews.length && !snapshots.length;
  q('task-count').textContent = String(tasks.length);
  q('review-count').textContent = String(reviews.length);
  q('snapshot-count').textContent = String(snapshots.length);
}

function renderTasks() {
  q('selection').replaceChildren();
  for (const item of tasks) {
    textRow(q('selection'), `${item.taskId} · ${item.title} · 第 ${item.round} 轮 (${item.phase}) · ${item.marker}`);
  }
  q('selection-summary').textContent = tasks.length
    ? `已编排 ${tasks.length} 个任务 · 当前选中 ${tasks[tasks.length - 1].taskId}`
    : '还没有编排任务';
}

q('add-task').onclick = () => {
  if (!ready) return;
  const limit = max_tasks();
  const title = q('task-title').value.trim();
  const goal = q('task-goal').value.trim();
  const mode = q('task-mode').value;
  const phase = q('task-phase').value;
  const round = Number.parseInt(q('task-round').value, 10) || 1;
  const workResult = q('work-result').value.trim();
  const goalBytes = new TextEncoder().encode(goal).length;
  if (tasks.length >= limit || goalBytes > MAX_GOAL_BYTES) {
    q('selection-summary').textContent = `超出限制：最多 ${limit} 个任务、单任务目标 16 KiB`;
    q('status').textContent = '任务超出限制，未加入队列。';
    controls();
    return;
  }
  if (!title || !goal) {
    q('status').textContent = '请填写任务标题与原始目标。';
    return;
  }
  const index = tasks.length + 1;
  const taskId = `task-${index}`;
  const token = `${taskId}-r${round}`;
  try {
    const built = JSON.parse(build_task_prompt(taskId, title, goal, mode, phase, round, token, '', workResult, ''));
    tasks.push({ ...built, title, goal, mode });
    q('prompt-preview').textContent = built.prompt;
    textRow(q('results'), `提示词已生成 · ${built.taskId} · ${built.marker} · sha256=${built.promptHash.slice(0, 12)}`);
    renderTasks();
    q('status').textContent = '本地提示词生成完成。没有联网或触发 CDP 发送。';
  } catch (error) {
    q('status').textContent = `生成失败：${String(error.message || error)}`;
  }
  controls();
};

q('reset-queue').onclick = () => {
  tasks = []; reviews = []; snapshots = [];
  q('selection').replaceChildren();
  q('results').replaceChildren();
  q('selection-summary').textContent = '还没有编排任务';
  q('prompt-preview').textContent = '生成的 Work / Review 提示词将显示在这里。';
  q('status').textContent = '已清空当前会话队列；未发送任何网络请求。';
  controls();
};

q('verify-review').onclick = () => {
  if (!ready || !tasks.length) return;
  const active = tasks[tasks.length - 1];
  const raw = q('review-input').value.trim();
  try {
    const verified = JSON.parse(verify_review_report(raw, active.taskId, active.round, active.goal));
    reviews.push(verified);
    textRow(q('results'), `验收报告通过 · ${verified.taskId} r${verified.round} · status=${verified.status} · ${verified.summary}`);
    q('status').textContent = `本地验收校验完成：status=${verified.status}。不代表远端会话执行。`;
  } catch (error) {
    q('status').textContent = `验收报告拒绝：${String(error.message || error)}`;
  }
  controls();
};

q('verify-snapshot').onclick = () => {
  if (!ready) return;
  try {
    const sample = JSON.stringify({
      url: 'https://chatgpt.com/c/fabushi-local-check?model=gpt',
      title: 'Local Snapshot Check',
      user_turns: 1,
      assistant_turns: 1,
      stop_available: false,
      waiting_for_approval: false,
      rate_limit_notice: false,
      copy_available_on_last_assistant: true,
      response_actions_complete: true,
      response_action_turn_bound_to_last: true,
      awaiting_assistant: false,
      assistant_text: q('review-input').value.trim() || 'checked',
      composer_text: '',
    });
    const evaluated = JSON.parse(evaluate_snapshot(sample));
    snapshots.push(evaluated);
    textRow(q('results'), `终态快照校验 · isTerminal=${evaluated.isTerminal} · url=${evaluated.canonicalUrl || 'none'}`);
    q('status').textContent = '本地终态快照校验完成。没有连接 CDP 或浏览器。';
  } catch (error) {
    q('status').textContent = `终态校验失败：${String(error.message || error)}`;
  }
  controls();
};

q('export').onclick = () => {
  if (!tasks.length && !reviews.length && !snapshots.length) return;
  const payload = {
    schemaVersion: 1,
    appId: 'org.fabushi.chatgpt-auto-confirm',
    appVersion: '0.1.0',
    verification: 'local-domain-workbench',
    networkSent: false,
    liveCdpAvailable: false,
    status: 'complete',
    tasks: tasks.map(({ taskId, title, mode, phase, round, marker, promptHash }) => ({
      taskId, title, mode, phase, round, marker, promptHash,
    })),
    reviews,
    snapshots,
  };
  const url = URL.createObjectURL(new Blob([JSON.stringify(payload, null, 2)], { type: 'application/json' }));
  const link = document.createElement('a');
  link.href = url;
  link.download = 'chatgpt-auto-confirm-local-report.json';
  link.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
};

const preferenceKey = 'chatgpt-auto-confirm-display-v1';
try { q('remember').checked = localStorage.getItem(preferenceKey) === 'true'; }
catch { q('storage-status').textContent = '当前宿主不支持持久 Web 存储；离线工作台仍可使用。'; }
q('remember').onchange = () => {
  try { localStorage.setItem(preferenceKey, String(q('remember').checked)); q('storage-status').textContent = '已保存当前小程序显示偏好，不包含任务内容。'; }
  catch { q('storage-status').textContent = '保存失败；偏好仅保留到页面关闭。'; }
};
q('clear').onclick = () => {
  try { localStorage.removeItem(preferenceKey); q('remember').checked = false; q('storage-status').textContent = '已删除显示偏好。'; }
  catch { q('storage-status').textContent = '存储不可访问，无法确认删除。'; }
};
window.addEventListener('pagehide', () => { tasks = []; reviews = []; snapshots = []; });

try {
  const [adapter, { wasmBytes }] = await Promise.all([
    import('./pkg/chatgpt_auto_confirm_web_adapter.js'),
    import('./wasm-bytes.js'),
  ]);
  await adapter.default({ module_or_path: wasmBytes });
  ({ max_tasks, build_task_prompt, verify_review_report, evaluate_snapshot } = adapter);
  ready = true;
  q('status').textContent = '本地 Rust 内核已就绪。';
} catch {
  q('status').textContent = '无法加载本地 Wasm：请检查完整安装及宿主 Wasm 策略。不会连接或启动外部浏览器。';
}
controls();
