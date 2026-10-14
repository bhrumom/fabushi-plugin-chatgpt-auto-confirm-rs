use crate::{ChatGptCdp, ExecutionProfile, Queue, TaskMode};
use anyhow::{Result, bail};
use fabushi_chatgpt_application::TaskStore;
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize, Default)]
#[serde(default)]
struct PanelCommand {
    action: String,
    id: String,
    request_id: String,
    goal: String,
    title: String,
    mode: String,
    model: String,
    effort: u8,
    value: String,
    auto: Option<bool>,
    files: Vec<PanelFile>,
}
#[derive(Deserialize)]
struct PanelFile {
    name: String,
    data: String,
}

struct IncomingFiles {
    folder: std::path::PathBuf,
    paths: Vec<std::path::PathBuf>,
}
impl Drop for IncomingFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.folder);
    }
}

fn save_files(queue: &Queue, files: &[PanelFile], request: &str) -> Result<IncomingFiles> {
    use base64::Engine;
    if files.len() > 32 {
        bail!("最多 32 个附件");
    }
    let folder = queue
        .path
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("incoming")
        .join(fabushi_chatgpt_domain::text_hash(request));
    std::fs::create_dir_all(&folder)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut incoming = IncomingFiles {
        folder: folder.clone(),
        paths: vec![],
    };
    let mut total = 0usize;
    for (index, file) in files.iter().enumerate() {
        if file.name.is_empty()
            || file.name.contains(['/', '\\', '\0'])
            || file.name == "."
            || file.name == ".."
            || file.name.len() > 255
            || file.data.len() > 28_000_000
        {
            bail!("附件名称或大小无效");
        }
        let bytes = base64::engine::general_purpose::STANDARD.decode(&file.data)?;
        total += bytes.len();
        if bytes.len() > 20 * 1024 * 1024 || total > 50 * 1024 * 1024 {
            bail!("单文件最多 20 MiB，合计最多 50 MiB");
        }
        let dir = folder.join(index.to_string());
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(&file.name);
        std::fs::write(&path, bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        incoming.paths.push(path);
    }
    Ok(incoming)
}

fn apply(queue: &Queue, endpoint: &str, command: &PanelCommand) -> Result<serde_json::Value> {
    let mut result = json!({});
    match command.action.as_str() {
        "refresh" => {}
        "select" => {
            if !command.id.is_empty() && !queue.tasks()?.iter().any(|t| t.id == command.id) {
                bail!("任务不存在");
            }
            let mut settings = queue.settings()?;
            settings.ui_selected_task = command.id.clone();
            queue.set_settings(settings)?;
        }
        "pause_all" => queue.pause_all()?,
        "resume_all" => queue.resume_all()?,
        "pause" | "resume" | "retry" | "cancel" | "delete" => {
            queue.change(&command.id, &command.action, None)?
        }
        "edit" => {
            if command.goal.trim().is_empty() || command.goal.len() > 800_000 {
                bail!("目标过长");
            }
            queue.change(&command.id, "edit", Some(command.goal.clone()))?;
        }
        "model" => {
            validate_profile(&command.model, command.effort)?;
            queue.change(&command.id, "model", Some(command.model.clone()))?;
        }
        "effort" => {
            if command.effort > 4 {
                bail!("推理强度无效");
            }
            queue.change(&command.id, "effort", Some(command.effort.to_string()))?;
        }
        "auto" => queue.change(
            &command.id,
            "auto",
            Some(command.auto.unwrap_or(true).to_string()),
        )?,
        "workspace" => {
            if command.value.trim().is_empty() || command.value.len() > 200 {
                bail!("工作区名称无效");
            }
            queue.change(&command.id, "workspace", Some(command.value.clone()))?;
        }
        "clear_attachments" => queue.change(&command.id, "clear-attachments", None)?,
        "defaults" => {
            validate_profile(&command.model, command.effort)?;
            let mut settings = queue.settings()?;
            settings.default_profile = ExecutionProfile {
                model: command.model.clone(),
                effort: command.effort,
            };
            settings.default_mode = parse_mode(&command.mode)?;
            settings.default_auto_approve = command.auto.unwrap_or(true);
            queue.set_settings(settings)?;
        }
        "global_auto" => {
            let mut settings = queue.settings()?;
            settings.global_auto_approve = command.auto.unwrap_or(true);
            queue.set_settings(settings)?;
        }
        "cleanup" => {
            let mut settings = queue.settings()?;
            settings.request_reload = true;
            queue.set_settings(settings)?;
        }
        "open" => {
            if !queue
                .tasks()?
                .iter()
                .any(|t| t.id == command.id && t.binding.is_some())
            {
                bail!("任务尚未绑定会话");
            }
            let mut settings = queue.settings()?;
            settings.request_open = command.id.clone();
            queue.set_settings(settings)?;
        }
        "add" | "attach" => {
            if command.goal.len() > 800_000 || command.title.len() > 800 {
                bail!("目标或名称过长");
            }
            if command.action == "add" {
                validate_profile(&command.model, command.effort)?;
                parse_mode(&command.mode)?;
                if command.goal.trim().is_empty() {
                    bail!("目标不能为空");
                }
            }
            let incoming = save_files(queue, &command.files, &command.request_id)?;
            let operation = (|| -> Result<()> {
                if command.action == "attach" {
                    for path in &incoming.paths {
                        queue.change(
                            &command.id,
                            "attach",
                            Some(path.to_string_lossy().into_owned()),
                        )?;
                    }
                } else {
                    let title = if command.title.trim().is_empty() {
                        command.goal.chars().take(30).collect()
                    } else {
                        command.title.clone()
                    };
                    let task = queue.add(
                        title,
                        command.goal.clone(),
                        parse_mode(&command.mode)?,
                        ExecutionProfile {
                            model: command.model.clone(),
                            effort: command.effort,
                        },
                        command.auto.unwrap_or(true),
                        &incoming.paths,
                    )?;
                    result["task_id"] = json!(task.id);
                    let mut settings = queue.settings()?;
                    settings.running = true;
                    settings.ui_selected_task = task.id;
                    queue.set_settings(settings)?;
                }
                Ok(())
            })();
            operation?;
        }
        "export" => {
            let mut tasks = queue.tasks()?;
            for task in &mut tasks {
                for file in &mut task.attachments {
                    file.path.clear();
                }
            }
            result["download"] = json!({"version":1,"tasks":tasks,"settings":queue.settings()?,"activity":queue.activity("")?});
        }
        "import" => {
            if command.value.len() > 10_000_000 {
                bail!("导入文件过大");
            }
            let mut export: crate::QueueExport = serde_json::from_str(&command.value)?;
            if export.version != 1 {
                bail!("导入版本不支持");
            }
            for task in &mut export.tasks {
                task.attachments.clear();
            }
            let path = queue
                .path
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .join("panel-import.json");
            std::fs::write(&path, serde_json::to_vec(&export)?)?;
            let imported = queue.import(&path);
            let _ = std::fs::remove_file(path);
            result["imported"] = json!(imported?);
        }
        "open_tui" => open_tui(queue, endpoint)?,
        _ => bail!("不支持的工作台操作"),
    }
    Ok(result)
}
fn validate_profile(model: &str, effort: u8) -> Result<()> {
    if effort > 4 || !["GPT-5.6 Sol", "GPT-6", "GPT-5.5"].contains(&model) {
        bail!("模型或推理强度不支持");
    }
    Ok(())
}
fn parse_mode(mode: &str) -> Result<TaskMode> {
    match mode {
        "goal" => Ok(TaskMode::Goal),
        "once" => Ok(TaskMode::Once),
        _ => bail!("任务模式不支持"),
    }
}

#[cfg(target_os = "macos")]
fn open_tui(queue: &Queue, endpoint: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let executable = std::env::current_exe()?;
    let path = queue
        .path
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("fabushi-workbench.command");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nexec {} --cdp {} --database {} tui\n",
            quote(&executable.to_string_lossy()),
            quote(endpoint),
            quote(&queue.path.to_string_lossy())
        ),
    )?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    if !std::process::Command::new("/usr/bin/open")
        .arg(&path)
        .status()?
        .success()
    {
        bail!("无法打开 TUI 启动器");
    }
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn open_tui(_: &Queue, _: &str) -> Result<()> {
    bail!("请在终端运行 tui")
}

fn panel_snapshot(queue: &Queue, endpoint: &str, details: bool) -> Result<serde_json::Value> {
    let settings = queue.settings()?;
    Ok(json!({
        "version":4,"details":details,"running":settings.running,"scans":settings.scans,
        "settings":settings,"now":crate::wall_ms(),"worker":queue.heartbeat(endpoint)?,
        "storage_bytes":std::fs::metadata(&queue.path).map(|m|m.len()).unwrap_or(0),
        "tasks":queue.tasks()?.iter().map(|t|json!({
          "id":t.id,"title":t.title,"goal":if details {t.goal.as_str()} else {""},"state":t.state,"mode":t.mode,"phase":t.phase,"round":t.round,"workspace":t.workspace,"profile":t.profile,"auto_approve":t.auto_approve,"attachments":t.attachments,"binding":t.binding,"updated_at":t.updated_at,"last_scan_at":t.last_scan_at,"cooldown_until":t.cooldown_until,"last_error":t.last_error,"approvals":t.approvals,"recoveries":t.recoveries,"latest_reply":if details {t.latest_reply.chars().take(40_000).collect::<String>()} else {String::new()}
        })).collect::<Vec<_>>(),"activity":if details {queue.store.activity("",crate::wall_ms().saturating_sub(7_200_000),2_000)?} else {vec![]},"operations":if details {queue.operations("")?} else {vec![]}
    }))
}

pub(crate) async fn sync_panel(queue: &Queue, cdp: &ChatGptCdp) -> Result<()> {
    let probe = cdp.workbench_probe().await?;
    let details = probe["open"] == true;
    let snapshot = panel_snapshot(queue, cdp.endpoint(), details)?;
    let result = cdp.workbench_panel(&snapshot).await?;
    let commands = result["commands"].as_array().cloned().unwrap_or_default();
    for value in commands.into_iter().take(50) {
        let command: PanelCommand = serde_json::from_value(value)?;
        let outcome = apply(queue, cdp.endpoint(), &command);
        let mut feedback = panel_snapshot(queue, cdp.endpoint(), details)?;
        feedback["notice"] = match outcome {
            Ok(result) => {
                json!({"request_id":command.request_id,"action":command.action,"ok":true,"text":if command.action=="cleanup"{"已请求安全重载；有回复、授权或草稿时等待，不强制清理"}else if command.action=="import"{"导入任务已暂停；附件需重新选择"}else{"操作已保存"},"result":result})
            }
            Err(error) => {
                json!({"request_id":command.request_id,"action":command.action,"ok":false,"text":format!("操作未执行：{error}")})
            }
        };
        cdp.workbench_panel(&feedback).await?;
    }
    Ok(())
}

pub(crate) struct PanelLoopGuard(tokio::task::JoinHandle<()>);
impl Drop for PanelLoopGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}
pub(crate) fn spawn_panel_loop(queue: Queue, endpoint: String, owner: String) -> PanelLoopGuard {
    PanelLoopGuard(tokio::spawn(async move {
        loop {
            if !matches!(queue.heartbeat(&endpoint),Ok(Some((current,expires))) if current==owner && expires>crate::wall_ms())
            {
                break;
            }
            let result = async {
                let cdp = ChatGptCdp::connect_desktop(&endpoint).await?;
                sync_panel(&queue, &cdp).await
            }
            .await;
            if let Err(error) = result {
                eprintln!("workbench: {error:#}");
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    }))
}

pub async fn profile_desktop_renderer(endpoint: &str) -> Result<serde_json::Value> {
    ChatGptCdp::connect_desktop(endpoint)
        .await?
        .profile_renderer()
        .await
}

/// Operator recovery: stop stalled renderer execution and detach only our panel.
pub async fn suspend_stalled_workbench(endpoint: &str) -> Result<serde_json::Value> {
    ChatGptCdp::connect_desktop(endpoint)
        .await?
        .suspend_stalled_workbench()
        .await
}

/// Restore the embedded entry without navigating or dispatching a task.
pub async fn show_desktop_workbench(queue: &Queue, endpoint: &str) -> Result<serde_json::Value> {
    let cdp = ChatGptCdp::connect_desktop(endpoint).await?;
    sync_panel(queue, &cdp).await?;
    cdp.workbench_probe().await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resume_one_task_restarts_supervisor_without_resuming_others() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("fabushi-panel-resume-{}", std::process::id()));
        let queue = Queue::open(&dir.join("state.sqlite"))?;
        let mut task = queue.add(
            "selected".into(),
            "goal".into(),
            TaskMode::Goal,
            ExecutionProfile::default(),
            false,
            &[],
        )?;
        let other = queue.add(
            "other".into(),
            "other goal".into(),
            TaskMode::Goal,
            ExecutionProfile::default(),
            false,
            &[],
        )?;
        let revision = task.revision;
        task.state = crate::TaskState::Sending;
        task.send_clicked = true;
        task.token = "original-dispatch".into();
        task.dispatch_at = 1234;
        assert!(queue.store.save(&mut task, revision, None)?);
        queue.pause_all()?;
        let mut settings = queue.settings()?;
        settings.stop_task = task.id.clone();
        queue.set_settings(settings)?;
        assert!(!queue.settings()?.running);
        let command: PanelCommand =
            serde_json::from_value(json!({"action":"resume", "id":task.id}))?;
        apply(&queue, "http://127.0.0.1:9223", &command)?;
        assert!(queue.settings()?.running);
        assert!(queue.settings()?.stop_task.is_empty());
        let tasks = queue.tasks()?;
        let resumed = tasks.iter().find(|t| t.id == task.id).unwrap();
        assert_eq!(resumed.state, crate::TaskState::Sending);
        assert_eq!(resumed.token, "original-dispatch");
        assert_eq!(resumed.dispatch_at, 1234);
        assert!(resumed.send_clicked);
        assert_eq!(
            tasks.iter().find(|t| t.id == other.id).unwrap().state,
            crate::TaskState::Paused
        );
        // Repeated resume must also recover an already-runnable task after a global stop.
        let mut settings = queue.settings()?;
        settings.running = false;
        queue.set_settings(settings)?;
        apply(&queue, "http://127.0.0.1:9223", &command)?;
        assert!(queue.settings()?.running);
        assert_eq!(
            queue
                .tasks()?
                .iter()
                .find(|t| t.id == other.id)
                .unwrap()
                .state,
            crate::TaskState::Paused
        );
        drop(queue);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[test]
    fn closed_snapshot_excludes_transcript_and_journals() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("fabushi-panel-lite-{}", std::process::id()));
        let queue = Queue::open(&dir.join("state.sqlite"))?;
        queue.add(
            "task".into(),
            "long original goal".repeat(1000),
            TaskMode::Goal,
            ExecutionProfile::default(),
            true,
            &[],
        )?;
        let lite = panel_snapshot(&queue, "http://127.0.0.1:9223", false)?;
        assert_eq!(lite["tasks"][0]["goal"], "");
        assert_eq!(lite["tasks"][0]["latest_reply"], "");
        assert!(lite["activity"].as_array().unwrap().is_empty());
        assert!(lite["operations"].as_array().unwrap().is_empty());
        assert!(lite.to_string().len() < 5000);
        let full = panel_snapshot(&queue, "http://127.0.0.1:9223", true)?;
        assert!(full["tasks"][0]["goal"].as_str().unwrap().len() > 10000);
        drop(queue);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[test]
    fn parity_controls_save_private_files_and_preserve_sent_identity() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("fabushi-panel-parity-{}", std::process::id()));
        let queue = Queue::open(&dir.join("state.sqlite"))?;
        let mut c: PanelCommand = serde_json::from_value(
            json!({"action":"add","request_id":"files-test","goal":"implement goal","mode":"goal","model":"GPT-5.6 Sol","effort":4,"auto":false,"files":[{"name":"notes.txt","data":"aGVsbG8="}]}),
        )?;
        let result = apply(&queue, "http://127.0.0.1:9223", &c)?;
        c.id = result["task_id"].as_str().unwrap().to_owned();
        let t = queue.tasks()?[0].clone();
        assert!(!t.auto_approve);
        assert_eq!(std::fs::read(&t.attachments[0].path)?, b"hello");
        c.action = "delete".into();
        assert!(apply(&queue, "http://127.0.0.1:9223", &c).is_err());
        let mut sent = t.clone();
        let revision = sent.revision;
        sent.send_clicked = true;
        sent.token = "unique-owned".into();
        sent.binding = Some(crate::SessionBinding {
            identity: "owned".into(),
            sidebar_id: "owned".into(),
            title: "owned".into(),
        });
        assert!(queue.store.save(&mut sent, revision, None)?);
        c.action = "edit".into();
        c.goal = "revised goal".into();
        apply(&queue, "http://127.0.0.1:9223", &c)?;
        c.action = "workspace".into();
        c.value = "iOS 工作区".into();
        apply(&queue, "http://127.0.0.1:9223", &c)?;
        c.action = "cancel".into();
        apply(&queue, "http://127.0.0.1:9223", &c)?;
        c.action = "resume".into();
        apply(&queue, "http://127.0.0.1:9223", &c)?;
        let t = queue.tasks()?[0].clone();
        assert_eq!(t.token, "unique-owned");
        assert_eq!(t.binding.as_ref().unwrap().sidebar_id, "owned");
        assert_eq!(t.state, crate::TaskState::Waiting);
        assert!(queue.settings()?.stop_task.is_empty());
        assert_eq!(t.workspace, "iOS 工作区");
        c.action = "global_auto".into();
        c.auto = Some(false);
        apply(&queue, "http://127.0.0.1:9223", &c)?;
        assert!(!queue.settings()?.global_auto_approve);
        c.action = "cleanup".into();
        apply(&queue, "http://127.0.0.1:9223", &c)?;
        assert!(queue.settings()?.request_reload);
        c.action = "pause".into();
        apply(&queue, "http://127.0.0.1:9223", &c)?;
        c.action = "delete".into();
        apply(&queue, "http://127.0.0.1:9223", &c)?;
        assert!(queue.tasks()?.is_empty());
        assert!(!std::path::Path::new(&t.attachments[0].path).exists());
        c.action = "add".into();
        c.files[0].name = "../../escape.txt".into();
        assert!(apply(&queue, "http://127.0.0.1:9223", &c).is_err());
        assert!(queue.tasks()?.is_empty());
        drop(queue);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[test]
    fn panel_export_import_pauses_and_never_trusts_imported_paths() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("fabushi-panel-import-{}", std::process::id()));
        let queue = Queue::open(&dir.join("a/state.sqlite"))?;
        let mut c: PanelCommand = serde_json::from_value(
            json!({"action":"add","goal":"goal","mode":"once","model":"GPT-6","effort":2}),
        )?;
        apply(&queue, "http://127.0.0.1:9223", &c)?;
        c.action = "export".into();
        let result = apply(&queue, "http://127.0.0.1:9223", &c)?;
        let mut download = result["download"].clone();
        download["tasks"][0]["attachments"] = json!([{"path":"/arbitrary/secret","name":"file","sha256":"x","size":1,"mime":"text/plain"}]);
        let imported = Queue::open(&dir.join("b/state.sqlite"))?;
        c.action = "import".into();
        c.value = download.to_string();
        apply(&imported, "http://127.0.0.1:9223", &c)?;
        assert_eq!(imported.tasks()?[0].state, crate::TaskState::Paused);
        assert!(imported.tasks()?[0].attachments.is_empty());
        apply(&imported, "http://127.0.0.1:9223", &c)?;
        assert_eq!(imported.tasks()?.len(), 1);
        drop(queue);
        drop(imported);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn panel_commands_share_queue_and_reject_arbitrary_actions() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("fabushi-panel-test-{}", std::process::id()));
        let queue = Queue::open(&dir.join("state.sqlite"))?;
        let mut command: PanelCommand = serde_json::from_value(
            json!({"action":"add","title":"panel test","goal":"goal <script>literal</script>","mode":"goal","model":"GPT-5.6 Sol","effort":4}),
        )?;
        apply(&queue, "http://127.0.0.1:9223", &command)?;
        let tasks = queue.tasks()?;
        let task = tasks.last().unwrap();
        assert_eq!(task.goal, command.goal);
        command.id = task.id.clone();
        command.action = "pause".into();
        apply(&queue, "http://127.0.0.1:9223", &command)?;
        assert_eq!(
            queue.tasks()?.last().unwrap().state,
            crate::TaskState::Paused
        );
        command.action = "resume".into();
        apply(&queue, "http://127.0.0.1:9223", &command)?;
        assert!(queue.tasks()?.last().unwrap().state.runnable());
        command.action = "shell".into();
        assert!(apply(&queue, "http://127.0.0.1:9223", &command).is_err());
        drop(queue);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
