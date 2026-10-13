use crate::{ChatGptCdp, ExecutionProfile, Phase, Queue, TaskMode};
use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
struct PanelCommand {
    action: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    goal: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    mode: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    effort: u8,
}

fn apply(queue: &Queue, endpoint: &str, command: &PanelCommand) -> Result<()> {
    match command.action.as_str() {
        "pause_all" => queue.pause_all(),
        "resume_all" => queue.resume_all(),
        "pause" | "resume" | "retry" => queue.change(&command.id, &command.action, None),
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
            queue.set_settings(settings)
        }
        "add" => {
            if command.goal.len() > 800_000 || command.title.len() > 800 || command.effort > 4 {
                bail!("目标、名称或推理强度超出范围");
            }
            if !["GPT-5.6 Sol", "GPT-6", "GPT-5.5"].contains(&command.model.as_str()) {
                bail!("不支持的模型");
            }
            let mode = match command.mode.as_str() {
                "goal" => TaskMode::Goal,
                "once" => TaskMode::Once,
                _ => bail!("不支持的任务模式"),
            };
            let title = if command.title.trim().is_empty() {
                command.goal.chars().take(30).collect()
            } else {
                command.title.clone()
            };
            queue.add(
                title,
                command.goal.clone(),
                mode,
                ExecutionProfile {
                    model: command.model.clone(),
                    effort: command.effort,
                },
                true,
                &[],
            )?;
            Ok(())
        }
        "open_tui" => open_tui(queue, endpoint),
        _ => bail!("不支持的工作台操作"),
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

pub(crate) async fn sync_panel(queue: &Queue, cdp: &ChatGptCdp) -> Result<()> {
    let settings = queue.settings()?;
    let snapshot = json!({
        "running": settings.running,
        "scans": settings.scans,
        "tasks": queue.tasks()?.iter().map(|t| json!({
            "id":t.id,"title":t.title,"round":t.round,
            "phase":if t.phase == Phase::Review {"验收"} else {"执行"},
            "state":t.state.label(),"approvals":t.approvals,"recoveries":t.recoveries,"error":t.last_error
        })).collect::<Vec<_>>(),
        "operations":queue.operations("")?
    });
    let result = cdp.workbench_panel(&snapshot).await?;
    let commands = result["commands"].as_array().cloned().unwrap_or_default();
    for value in commands.into_iter().take(50) {
        let command: PanelCommand = serde_json::from_value(value)?;
        let outcome = apply(queue, cdp.endpoint(), &command);
        let mut feedback = snapshot.clone();
        feedback["notice"] = json!({"action":command.action,"ok":outcome.is_ok(),"text":match outcome {
            Ok(()) => "操作已保存；监督器将按任务状态继续执行".to_owned(),
            Err(error) => format!("操作未执行：{error}")
        }});
        cdp.workbench_panel(&feedback).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
