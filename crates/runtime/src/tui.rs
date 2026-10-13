use crate::{Queue, wall_ms};
use anyhow::Result;
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use fabushi_chatgpt_domain::*;
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use std::{
    io,
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};
const MODELS: [&str; 3] = ["GPT-5.6 Sol", "GPT-6", "GPT-5.5"];
#[derive(Default)]
struct Ui {
    selected: usize,
    new_view: bool,
    input: String,
    cursor: usize,
    editing: Option<String>,
    input_mode: bool,
    dialog: Option<&'static str>,
    notice: String,
    scroll: u16,
    help: bool,
    settings: bool,
    logs: bool,
    files: Vec<std::path::PathBuf>,
}
fn time_label(at: u64) -> String {
    let seconds = (at / 1000 + 8 * 3600) % 86400;
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}
fn age(at: u64) -> String {
    if at == 0 {
        "未扫描".into()
    } else {
        format!("{}s 前", wall_ms().saturating_sub(at) / 1000)
    }
}
fn palette() -> Style {
    Style::default()
        .bg(Color::Rgb(23, 23, 23))
        .fg(Color::Rgb(236, 236, 236))
}
fn block(title: impl Into<Line<'static>>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(Color::Rgb(75, 75, 75)))
}
fn draw(frame: &mut Frame, queue: &Queue, endpoint: &str, ui: &Ui) -> Result<()> {
    let area = frame.area();
    frame.render_widget(Block::default().style(palette()), area);
    let cols = Layout::horizontal([Constraint::Length(29), Constraint::Min(30)]).split(area);
    let tasks = queue.tasks()?;
    let settings = queue.settings()?;
    let task = if ui.new_view {
        None
    } else {
        tasks.get(ui.selected)
    };
    let mut rows = vec![ListItem::new(Line::styled(
        "＋ 新任务  Ctrl+N",
        Style::default().fg(Color::Magenta),
    ))];
    for t in &tasks {
        let phase = if t.phase == Phase::Review {
            "验收"
        } else {
            "执行"
        };
        rows.push(ListItem::new(vec![
            Line::styled(
                format!(
                    "{} {}",
                    if t.state == TaskState::Done {
                        "●"
                    } else {
                        "○"
                    },
                    bounded(&t.title, 26)
                ),
                Style::default().fg(if t.state == TaskState::Done {
                    Color::Green
                } else {
                    Color::White
                }),
            ),
            Line::styled(
                format!("  {} · {} #{}", t.state.label(), phase, t.round),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }
    let blocked = tasks
        .iter()
        .filter(|t| t.state.runnable() && !t.last_error.is_empty())
        .count();
    let status = if queue
        .heartbeat(endpoint)?
        .is_some_and(|(_, expires)| expires > wall_ms())
    {
        if blocked > 0 {
            format!("监督在线 · {blocked} 项恢复中")
        } else {
            "监督在线 · 正在推进".into()
        }
    } else {
        "监督进程未连接".into()
    };
    let sidebar = Layout::vertical([Constraint::Min(5), Constraint::Length(8)]).split(cols[0]);
    let list = List::new(rows)
        .block(block(" Fabushi 工作台 r3 "))
        .highlight_style(Style::default().bg(Color::Rgb(55, 45, 78)))
        .highlight_symbol("▸ ");
    let mut state = ListState::default().with_selected(if ui.new_view || tasks.is_empty() {
        Some(0)
    } else {
        Some(ui.selected + 1)
    });
    frame.render_stateful_widget(list, sidebar[0], &mut state);
    let selected_title = |id: &str| {
        tasks
            .iter()
            .find(|t| t.id == id)
            .map(|t| bounded(&t.title, 22))
            .unwrap_or_else(|| "尚无".into())
    };
    frame.render_widget(
        Paragraph::new(format!(
            "{} · {}\n尝试：{}\n确认：{}\n扫描 {} · 已确认切换 {}\n最近扫描 {} · 用时 {}ms\nr3 · V 一小时记录 · ? 帮助",
            status, if settings.running { "开启" } else { "暂停" },
            selected_title(&settings.last_task), selected_title(&settings.last_confirmed_task),
            settings.scans, settings.switches, age(settings.last_attempt_at), settings.last_scan_ms
        ))
        .block(block(" 监督实时状态 "))
        .style(Style::default().fg(Color::Gray)),
        sidebar[1],
    );
    let split = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(5),
        Constraint::Length(if ui.input_mode { 9 } else { 5 }),
        Constraint::Length(2),
    ])
    .split(cols[1]);
    let heading = task
        .map(|t| format!("{} · {} · 第 {} 轮", t.title, t.state.label(), t.round))
        .unwrap_or_else(|| "新任务".into());
    frame.render_widget(
        Paragraph::new(heading)
            .block(block(" 编辑 E   暂停 P   打开 O   设置 F2 "))
            .style(Style::default().fg(Color::White)),
        split[0],
    );
    let mut feed = String::new();
    if ui.help {
        feed.push_str("Ctrl+N 新任务  i 输入  Ctrl+S/Alt+Enter 发送  Esc 返回\n↑↓/j k 选择任务  PageUp/Down 滚动内容\nP 暂停/继续当前  Shift+P 暂停全部  S 继续全部\nC 取消  D 可恢复删除  E 编辑目标  O 打开记录会话\nM 模型  R 思考强度  G 单次/持续  A 自动授权\nF2 设置  L 安全内存恢复  a 附件路径  x 清空附件\nF 导入目标文本文件  I 导入队列  X 导出队列\nQ 关闭 TUI（后台监督继续；用 Shift+P 停止任务）\n\n已发送的轮次不改模型、不重发；修改对后续轮次生效。\n完成需要独立验收报告。回复结束不是目标完成。\n");
    } else if ui.settings {
        feed.push_str(&format!("设置\n监督：{}\n全局当前会话自动授权：{}\n新任务模式：{:?}\n默认模型：{} / {}\n数据库：{}\n\nS 开启/恢复监督，Shift+P 暂停全部\nA 切换全局授权；L 请求安全 renderer 恢复\n恢复时保留任务标识、会话和进度；有授权/草稿/生成则等待。\n",settings.running,settings.global_auto_approve,settings.default_mode,settings.default_profile.model,settings.default_profile.effort_label(),queue.path.display()));
    } else if ui.logs {
        feed.push_str("过去 1 小时 · 全部任务具体操作 · V 返回 · PageDown 滚动历史\n");
        for a in queue.operations("")?.iter().rev() {
            let title = tasks
                .iter()
                .find(|t| t.id == a.task_id)
                .map(|t| t.title.as_str())
                .unwrap_or(&a.task_id);
            feed.push_str(&format!(
                "[{}] {} / {}\n{}\n",
                time_label(a.at),
                bounded(title, 28),
                a.role,
                bounded(&a.text, 400)
            ));
        }
    } else if let Some(t) = task {
        feed.push_str(&format!("第 {} 轮 · {} · {}\n最后扫描 {} · 重试 {}s 后 · 授权 {} 次 · 恢复 {} 次\n{} / {} · 自动授权 {}\n",
            t.round, if t.phase == Phase::Review { "独立验收" } else { "执行目标" }, t.state.label(),
            age(t.last_scan_at), t.cooldown_until.saturating_sub(wall_ms()) / 1000, t.approvals, t.recoveries,
            t.profile.model, t.profile.effort_label(), t.auto_approve));
        if !t.last_error.is_empty() {
            feed.push_str(&format!("当前阻塞：{}\n", t.last_error));
        }
        if let Some(b) = &t.binding {
            feed.push_str(&format!("会话：{}\n", b.sidebar_id));
        }
        feed.push_str("\n过去 1 小时具体操作 · V 全局记录 · PageDown 查看历史\n");
        for a in queue.operations(&t.id)?.iter().rev() {
            feed.push_str(&format!(
                "[{}] {}  {}\n",
                time_label(a.at),
                a.role,
                bounded(&a.text, 250)
            ));
        }
        if !t.latest_reply.is_empty() {
            feed.push_str(&format!(
                "\n最近可见工作内容\n{}\n",
                bounded(&t.latest_reply, 12_000)
            ));
        }
        feed.push_str(&format!(
            "\n原始目标（E 查看/编辑）\n{}",
            bounded(&t.goal, 350)
        ));
    } else {
        feed.push_str("在下方输入目标。持续目标自动执行、开启独立验收，再按验收结果推进；单次任务只等待一次最终回复。\nF 导入任务文本。a 添加图片 / 视频 / 文件。");
    }
    frame.render_widget(
        Paragraph::new(feed)
            .block(block(" 工作内容 / 操作记录 "))
            .wrap(Wrap { trim: false })
            .scroll((ui.scroll, 0)),
        split[1],
    );
    let compose_title = ui
        .dialog
        .map(|s| format!(" {}路径 · Enter 确认 · Esc 取消 ", s))
        .unwrap_or_else(|| {
            if ui.input_mode {
                " 输入目标 · Ctrl+S 发送 · Enter 换行 ".into()
            } else {
                " 输入 I · 新任务 Ctrl+N · Ctrl+S 发送 ".into()
            }
        });
    let input_lines = ui.input.lines().count();
    let input = if input_lines > 5 {
        ui.input
            .lines()
            .skip(input_lines - 5)
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        ui.input.clone()
    };
    let mut composer = String::new();
    if ui.dialog.is_none() {
        let p = task
            .filter(|_| ui.editing.is_some())
            .map(|t| &t.profile)
            .unwrap_or(&settings.default_profile);
        composer.push_str(&format!(
            "{:?}  模型 {}  强度 {}  自动授权 {}  [Ctrl+S ↑]\n",
            settings.default_mode,
            p.model,
            p.effort_label(),
            settings.default_auto_approve
        ));
        if !ui.files.is_empty() {
            composer.push_str(&format!(
                "附件：{}\n",
                ui.files
                    .iter()
                    .map(|p| p.file_name().unwrap_or_default().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" · ")
            ));
        }
    }
    composer.push_str(&input);
    frame.render_widget(
        Paragraph::new(composer)
            .block(block(compose_title))
            .wrap(Wrap { trim: false })
            .style(Style::default().bg(Color::Rgb(40, 40, 40))),
        split[2],
    );
    frame.render_widget(
        Paragraph::new(format!(
            "{}  {}",
            if ui.input_mode {
                "输入模式"
            } else {
                "命令模式"
            },
            ui.notice
        ))
        .style(Style::default().fg(Color::Gray)),
        split[3],
    );
    Ok(())
}
struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            LeaveAlternateScreen,
            DisableBracketedPaste,
            DisableMouseCapture
        );
    }
}
fn spawn_worker(queue: &Queue, endpoint: &str) -> Result<()> {
    if queue
        .heartbeat(endpoint)?
        .is_some_and(|(_, expires)| expires > wall_ms())
    {
        return Ok(());
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(queue.path.with_extension("supervisor.log"))?;
    Command::new(std::env::current_exe()?)
        .args(["--cdp", endpoint, "--database"])
        .arg(&queue.path)
        .arg("supervise")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()?;
    Ok(())
}
pub fn run_tui(queue: Queue, endpoint: String) -> Result<()> {
    spawn_worker(&queue, &endpoint)?;
    enable_raw_mode()?;
    execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    let _guard = TerminalGuard;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut ui = Ui::default();
    let mut worker_check = std::time::Instant::now();
    loop {
        if worker_check.elapsed() >= Duration::from_secs(5) {
            if let Err(error) = spawn_worker(&queue, &endpoint) {
                ui.notice = format!("监督恢复失败：{error}");
            }
            worker_check = std::time::Instant::now();
        }
        let len = queue.tasks()?.len();
        if len > 0 {
            ui.selected = ui.selected.min(len - 1);
        }
        terminal.draw(|frame| {
            if let Err(e) = draw(frame, &queue, &endpoint, &ui) {
                frame.render_widget(Paragraph::new(e.to_string()), frame.area());
            }
        })?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        match event::read()? {
            Event::Paste(text) => {
                ui.input_mode = true;
                ui.input.insert_str(ui.cursor, &text);
                ui.cursor += text.len();
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if handle_key(key, &queue, &endpoint, &mut ui)? {
                    break;
                }
            }
            Event::Mouse(mouse)
                if mouse.kind == event::MouseEventKind::Down(event::MouseButton::Left)
                    && mouse.column < 29
                    && mouse.row >= 3 =>
            {
                ui.selected = ((mouse.row as usize - 3) / 2).min(len.saturating_sub(1));
                ui.scroll = 0;
                ui.new_view = false;
            }
            _ => {}
        }
    }
    Ok(())
}
fn handle_key(key: KeyEvent, queue: &Queue, _endpoint: &str, ui: &mut Ui) -> Result<bool> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if key.code == KeyCode::Char('n') && ctrl {
        ui.input.clear();
        ui.cursor = 0;
        ui.input_mode = true;
        ui.editing = None;
        ui.dialog = None;
        ui.new_view = true;
        return Ok(false);
    }
    if key.code == KeyCode::Char('s') && ctrl
        || key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::ALT)
    {
        let result = (|| {
            if let Some(id) = ui.editing.clone() {
                queue.change(&id, "edit", Some(ui.input.clone()))?;
            } else {
                let settings = queue.settings()?;
                let title = bounded(
                    ui.input
                        .lines()
                        .find(|s| !s.trim().is_empty())
                        .unwrap_or("任务")
                        .trim(),
                    50,
                );
                queue.add(
                    title,
                    ui.input.clone(),
                    settings.default_mode,
                    settings.default_profile,
                    settings.default_auto_approve,
                    &ui.files,
                )?;
            }
            Ok::<(), anyhow::Error>(())
        })();
        match result {
            Ok(()) => {
                ui.input.clear();
                ui.cursor = 0;
                ui.editing = None;
                ui.files.clear();
                ui.input_mode = false;
                ui.new_view = false;
                ui.selected = queue.tasks()?.len().saturating_sub(1);
                ui.notice = "任务已持久化并排队".into();
            }
            Err(e) => ui.notice = e.to_string(),
        }
        return Ok(false);
    }
    if ui.input_mode {
        match key.code {
            KeyCode::Esc => {
                ui.input_mode = false;
                ui.dialog = None;
            }
            KeyCode::Enter if ui.dialog.is_some() => {
                let value = ui.input.trim().to_owned();
                let kind = ui.dialog.take().unwrap_or("");
                let result = (|| {
                    match kind {
                        "附件" => ui.files.push(value.into()),
                        "目标文件" => {
                            ui.input = std::fs::read_to_string(value)?;
                            ui.cursor = ui.input.len();
                            return Ok::<(), anyhow::Error>(());
                        }
                        "导入队列" => {
                            let n = queue.import(Path::new(&value))?;
                            ui.notice = format!("已导入 {n} 个任务，保持暂停");
                        }
                        "导出队列" => {
                            queue.export(Path::new(&value))?;
                            ui.notice = "导出完成".into();
                        }
                        _ => {}
                    }
                    ui.input.clear();
                    ui.cursor = 0;
                    ui.input_mode = false;
                    Ok(())
                })();
                if let Err(e) = result {
                    ui.notice = e.to_string();
                }
            }
            KeyCode::Char(c) if !ctrl => {
                ui.input.insert(ui.cursor, c);
                ui.cursor += c.len_utf8();
            }
            KeyCode::Enter => {
                ui.input.insert(ui.cursor, '\n');
                ui.cursor += 1;
            }
            KeyCode::Backspace => {
                if let Some((pos, _)) = ui.input[..ui.cursor].char_indices().next_back() {
                    ui.input.remove(pos);
                    ui.cursor = pos;
                }
            }
            KeyCode::Delete => {
                if ui.cursor < ui.input.len() {
                    ui.input.remove(ui.cursor);
                }
            }
            KeyCode::Left => {
                ui.cursor = ui.input[..ui.cursor]
                    .char_indices()
                    .next_back()
                    .map(|(p, _)| p)
                    .unwrap_or(0);
            }
            KeyCode::Right => {
                if let Some(c) = ui.input[ui.cursor..].chars().next() {
                    ui.cursor += c.len_utf8();
                }
            }
            KeyCode::Home => ui.cursor = 0,
            KeyCode::End => ui.cursor = ui.input.len(),
            _ => {}
        }
        return Ok(false);
    }
    let tasks = queue.tasks()?;
    let task = if ui.new_view {
        None
    } else {
        tasks.get(ui.selected)
    };
    let result = (|| {
        match key.code {
            KeyCode::Char('q') => return Ok::<bool, anyhow::Error>(true),
            KeyCode::Char('?') => ui.help = !ui.help,
            KeyCode::F(2) => ui.settings = !ui.settings,
            KeyCode::Up | KeyCode::Char('k') => {
                ui.selected = ui.selected.saturating_sub(1);
                ui.scroll = 0;
                ui.new_view = false;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                ui.selected = (ui.selected + 1).min(tasks.len().saturating_sub(1));
                ui.scroll = 0;
                ui.new_view = false;
            }
            KeyCode::PageDown => ui.scroll = ui.scroll.saturating_add(10),
            KeyCode::PageUp => ui.scroll = ui.scroll.saturating_sub(10),
            KeyCode::Char('i') => {
                ui.input_mode = true;
                ui.cursor = ui.input.len();
            }
            KeyCode::Char('p') => {
                if let Some(t) = task {
                    queue.change(
                        &t.id,
                        if t.state == TaskState::Paused {
                            "resume"
                        } else {
                            "pause"
                        },
                        None,
                    )?;
                }
            }
            KeyCode::Char('P') => queue.pause_all()?,
            KeyCode::Char('s') => queue.resume_all()?,
            KeyCode::Char('c') => {
                if let Some(t) = task {
                    queue.change(&t.id, "cancel", None)?;
                }
            }
            KeyCode::Char('d') => {
                if let Some(t) = task {
                    queue.change(&t.id, "delete", None)?;
                }
            }
            KeyCode::Char('e') => {
                if let Some(t) = task {
                    ui.editing = Some(t.id.clone());
                    ui.input = t.goal.clone();
                    ui.cursor = ui.input.len();
                    ui.input_mode = true;
                }
            }
            KeyCode::Char('o') => {
                if let Some(t) = task {
                    queue.change(&t.id, "pause", None)?;
                    let mut s = queue.settings()?;
                    s.request_open = t.id.clone();
                    queue.set_settings(s)?;
                    ui.notice = "打开已记录会话，暂停当前任务；P 恢复".into();
                }
            }
            KeyCode::Char('m') => {
                let mut s = queue.settings()?;
                if let Some(t) = task {
                    let index = MODELS
                        .iter()
                        .position(|m| *m == t.profile.model)
                        .unwrap_or(0);
                    queue.change(&t.id, "model", Some(MODELS[(index + 1) % 3].into()))?;
                } else {
                    let index = MODELS
                        .iter()
                        .position(|m| *m == s.default_profile.model)
                        .unwrap_or(0);
                    s.default_profile.model = MODELS[(index + 1) % 3].into();
                    queue.set_settings(s)?;
                }
            }
            KeyCode::Char('r') => {
                let mut s = queue.settings()?;
                if let Some(t) = task {
                    queue.change(
                        &t.id,
                        "effort",
                        Some(((t.profile.effort + 1) % 5).to_string()),
                    )?;
                } else {
                    s.default_profile.effort = (s.default_profile.effort + 1) % 5;
                    queue.set_settings(s)?;
                }
            }
            KeyCode::Char('g') => {
                let mut s = queue.settings()?;
                s.default_mode = if s.default_mode == TaskMode::Goal {
                    TaskMode::Once
                } else {
                    TaskMode::Goal
                };
                queue.set_settings(s)?;
            }
            KeyCode::Char('A') => {
                if !ui.settings
                    && let Some(t) = task
                {
                    queue.change(&t.id, "auto", None)?;
                    return Ok(false);
                }
                let mut s = queue.settings()?;
                if ui.settings {
                    s.global_auto_approve = !s.global_auto_approve;
                } else {
                    s.default_auto_approve = !s.default_auto_approve;
                }
                queue.set_settings(s)?;
            }
            KeyCode::Char('v') => {
                ui.logs = !ui.logs;
                ui.scroll = 0;
            }
            KeyCode::Char('l') => {
                let mut s = queue.settings()?;
                s.request_reload = true;
                queue.set_settings(s)?;
                ui.notice = "已请求安全恢复，等待授权/草稿/生成结束".into();
            }
            KeyCode::Char('a') | KeyCode::Char('f') | KeyCode::Char('I') | KeyCode::Char('X') => {
                ui.dialog = Some(match key.code {
                    KeyCode::Char('a') => "附件",
                    KeyCode::Char('f') => "目标文件",
                    KeyCode::Char('I') => "导入队列",
                    _ => "导出队列",
                });
                ui.input.clear();
                ui.cursor = 0;
                ui.input_mode = true;
            }
            KeyCode::Char('x') => ui.files.clear(),
            _ => {}
        }
        Ok(false)
    })();
    match result {
        Ok(quit) => Ok(quit),
        Err(e) => {
            ui.notice = e.to_string();
            Ok(false)
        }
    }
}
pub fn tui_snapshot(queue: &Queue, endpoint: &str, path: &Path) -> Result<()> {
    let backend = ratatui::backend::TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend)?;
    let ui = Ui::default();
    terminal.draw(|f| {
        let _ = draw(f, queue, endpoint, &ui);
    })?;
    let buffer = terminal.backend().buffer();
    let mut result = String::new();
    for y in 0..40 {
        for x in 0..120 {
            result.push_str(buffer[(x, y)].symbol());
        }
        result.push('\n');
    }
    std::fs::write(path, result)?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use fabushi_chatgpt_application::TaskStore;
    #[test]
    fn dashboard_shows_round_retry_and_latest_event_before_long_goal() {
        let root = std::env::temp_dir().join(format!("fabushi-dashboard-{}", std::process::id()));
        let q = Queue::open(&root.join("state.sqlite")).unwrap();
        let mut t = q
            .add(
                "iOS迁移".into(),
                "LONG_GOAL_CONTENT".repeat(200),
                TaskMode::Goal,
                ExecutionProfile::default(),
                true,
                &[],
            )
            .unwrap();
        t.round = 3;
        t.phase = Phase::Review;
        t.last_scan_at = wall_ms();
        t.cooldown_until = wall_ms() + 30_000;
        let rev = t.revision;
        let task_id = t.id.clone();
        q.store
            .save(
                &mut t,
                rev,
                Some(Activity {
                    task_id,
                    at: wall_ms(),
                    role: "switch".into(),
                    text: "已确认切换，任务标记匹配".into(),
                }),
            )
            .unwrap();
        let mut settings = q.settings().unwrap();
        settings.last_task = t.id.clone();
        settings.last_confirmed_task = t.id.clone();
        settings.last_attempt_at = wall_ms();
        q.set_settings(settings).unwrap();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|f| draw(f, &q, "desktop", &Ui::default()).unwrap())
            .unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        let screen = screen
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>();
        assert!(screen.contains("第3轮"));
        assert!(screen.contains("独立验收"));
        assert!(screen.contains("已确认切换"));
        assert!(screen.contains("重试"));
        assert!(
            screen.find("已确认切换").unwrap()
                < screen.find("LONG_GOAL_CONTENT").unwrap_or(screen.len())
        );
    }
    #[test]
    fn unicode_editor_and_escape_do_not_pause_tasks() {
        let root = std::env::temp_dir().join(format!("fabushi-tui-{}", std::process::id()));
        let q = Queue::open(&root.join("state.sqlite")).unwrap();
        let mut ui = Ui {
            input_mode: true,
            ..Ui::default()
        };
        handle_key(
            KeyEvent::new(KeyCode::Char('中'), KeyModifiers::NONE),
            &q,
            "desktop",
            &mut ui,
        )
        .unwrap();
        assert_eq!(ui.cursor, 3);
        handle_key(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            &q,
            "desktop",
            &mut ui,
        )
        .unwrap();
        assert!(ui.input.is_empty());
        handle_key(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &q,
            "desktop",
            &mut ui,
        )
        .unwrap();
        assert!(q.settings().unwrap().running);
    }
}
