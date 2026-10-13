use crate::ChatGptCdp;
use anyhow::{Result, bail};
use fabushi_chatgpt_application::{Clock, Supervisor, SupervisorPolicy, TaskStore};
use fabushi_chatgpt_domain::*;
use fabushi_chatgpt_sqlite_store::SqliteTaskStore;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}
pub struct WallClock;
#[async_trait::async_trait]
impl Clock for WallClock {
    fn now(&self) -> Duration {
        Duration::from_millis(wall_ms())
    }
    async fn sleep(&self, d: Duration) {
        tokio::time::sleep(d).await;
    }
}
#[derive(Clone)]
pub struct Queue {
    pub(crate) store: Arc<SqliteTaskStore>,
    pub path: PathBuf,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueExport {
    pub version: u32,
    pub tasks: Vec<Task>,
    pub settings: Settings,
    pub activity: Vec<Activity>,
}
impl Queue {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            store: Arc::new(SqliteTaskStore::open(path)?),
            path: path.into(),
        })
    }
    pub fn tasks(&self) -> Result<Vec<Task>> {
        self.store.list()
    }
    pub fn settings(&self) -> Result<Settings> {
        self.store.settings()
    }
    pub fn set_settings(&self, mut settings: Settings) -> Result<()> {
        if !self.store.save_settings(&mut settings)? {
            bail!("设置已被另一窗口修改，请重试");
        }
        Ok(())
    }
    pub fn add(
        &self,
        title: String,
        goal: String,
        mode: TaskMode,
        profile: ExecutionProfile,
        auto: bool,
        files: &[PathBuf],
    ) -> Result<Task> {
        if goal.trim().is_empty() {
            bail!("任务目标不能为空");
        }
        let now = wall_ms();
        let id = text_hash(&format!(
            "{}:{}:{}:{}",
            std::process::id(),
            now,
            title,
            goal
        ))[..16]
            .to_string();
        let mut t = Task::new(id, title, goal, mode, now);
        t.profile = profile;
        t.auto_approve = auto;
        t.attachments = self.copy_files(files)?;
        self.store.insert(&t)?;
        Ok(t)
    }
    pub fn copy_files(&self, files: &[PathBuf]) -> Result<Vec<Attachment>> {
        let root = self
            .path
            .parent()
            .unwrap_or(Path::new("."))
            .join("attachments");
        std::fs::create_dir_all(&root)?;
        let mut result = vec![];
        for file in files {
            let bytes = std::fs::read(file)?;
            use sha2::{Digest, Sha256};
            let sha = format!("{:x}", Sha256::digest(&bytes));
            let name = file
                .file_name()
                .and_then(|v| v.to_str())
                .unwrap_or("attachment")
                .to_owned();
            let dir = root.join(&sha);
            std::fs::create_dir_all(&dir)?;
            let dest = dir.join(&name);
            std::fs::write(&dest, &bytes)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o600))?;
            }
            let mime = match file
                .extension()
                .and_then(|v| v.to_str())
                .unwrap_or("")
                .to_lowercase()
                .as_str()
            {
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "webp" => "image/webp",
                "gif" => "image/gif",
                "mp4" => "video/mp4",
                "mov" => "video/quicktime",
                "pdf" => "application/pdf",
                "txt" | "md" => "text/plain",
                _ => "application/octet-stream",
            }
            .to_owned();
            if !result
                .iter()
                .any(|a: &Attachment| a.sha256 == sha && a.name == name)
            {
                result.push(Attachment {
                    path: dest.to_string_lossy().into_owned(),
                    name,
                    sha256: sha,
                    size: bytes.len() as u64,
                    mime,
                });
            }
        }
        Ok(result)
    }
    pub fn change(&self, id: &str, action: &str, value: Option<String>) -> Result<()> {
        let mut t = self
            .tasks()?
            .into_iter()
            .find(|t| t.id == id)
            .ok_or_else(|| anyhow::anyhow!("task not found: {id}"))?;
        if action == "delete" {
            self.store.delete(id)?;
            return Ok(());
        }
        let expected = t.revision;
        match action {
            "pause" => t.pause(),
            "resume" => t.resume(),
            "cancel" => t.state = TaskState::Cancelled,
            "edit" => t.edit_goal(value.ok_or_else(|| anyhow::anyhow!("goal required"))?),
            "model" => t.profile.model = value.ok_or_else(|| anyhow::anyhow!("model required"))?,
            "effort" => {
                let effort: u8 = value.unwrap_or_default().parse()?;
                if effort > 4 {
                    bail!("effort must be 0..4");
                }
                t.profile.effort = effort;
            }
            "auto" => t.auto_approve = !t.auto_approve,
            "workspace" => t.workspace = value.unwrap_or_else(|| "桌面工作区".into()),
            "attach" => t
                .attachments
                .extend(self.copy_files(&[PathBuf::from(value.unwrap_or_default())])?),
            "clear-attachments" => t.attachments.clear(),
            "retry" => {
                if t.state == TaskState::Paused {
                    t.resume();
                }
                t.cooldown_until = 0;
                t.last_error.clear();
            }
            _ => bail!("unknown task action: {action}"),
        }
        t.updated_at = wall_ms();
        let event = Activity {
            task_id: t.id.clone(),
            at: wall_ms(),
            role: "status".into(),
            text: format!("用户操作：{action}；已发送轮次保持原配置，后续轮次使用新设置"),
        };
        if action == "cancel" && t.binding.is_some() {
            let mut settings = self.settings()?;
            settings.stop_task = t.id.clone();
            self.set_settings(settings)?;
        }
        if !self.store.save(&mut t, expected, Some(event))? {
            bail!("任务版本已变化，请重试");
        }
        Ok(())
    }
    pub fn pause_all(&self) -> Result<()> {
        let mut settings = self.settings()?;
        settings.running = false;
        self.set_settings(settings)?;
        for t in self.tasks()? {
            if t.state.runnable() {
                self.change(&t.id, "pause", None)?;
            }
        }
        Ok(())
    }
    pub fn resume_all(&self) -> Result<()> {
        for t in self.tasks()? {
            if t.state == TaskState::Paused {
                self.change(&t.id, "resume", None)?;
            }
        }
        let mut settings = self.settings()?;
        settings.running = true;
        self.set_settings(settings)
    }
    pub fn activity(&self, id: &str) -> Result<Vec<Activity>> {
        self.store
            .activity(id, wall_ms().saturating_sub(7_200_000), 360)
    }
    pub fn operations(&self, id: &str) -> Result<Vec<Activity>> {
        self.store
            .operations(id, wall_ms().saturating_sub(3_600_000), 10_000)
    }
    pub fn export(&self, path: &Path) -> Result<()> {
        let data = QueueExport {
            version: 1,
            tasks: self.tasks()?,
            settings: self.settings()?,
            activity: self.activity("")?,
        };
        std::fs::write(path, serde_json::to_string_pretty(&data)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }
    pub fn import(&self, path: &Path) -> Result<usize> {
        let data: QueueExport = serde_json::from_slice(&std::fs::read(path)?)?;
        if data.version != 1 {
            bail!("unsupported queue export");
        }
        let existing = self.tasks()?;
        let mut count = 0;
        for mut t in data.tasks {
            if !existing.iter().any(|e| e.id == t.id) {
                t.pause();
                self.store.insert(&t)?;
                count += 1;
            }
        }
        Ok(count)
    }
    pub fn heartbeat(&self, endpoint: &str) -> Result<Option<(String, u64)>> {
        self.store.heartbeat(endpoint)
    }
}
pub async fn supervise(queue: Queue, endpoint: String, once: bool) -> Result<()> {
    let owner = format!("rust-supervisor-{}-{}", std::process::id(), wall_ms());
    let clock = WallClock;
    if !once && !queue.store.lease(&endpoint, &owner, wall_ms(), 120_000)? {
        return Ok(());
    }
    let heartbeat_queue = queue.clone();
    let heartbeat_endpoint = endpoint.clone();
    let heartbeat_owner = owner.clone();
    let renewer = if once {
        None
    } else {
        Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(20)).await;
                if !heartbeat_queue
                    .store
                    .lease(&heartbeat_endpoint, &heartbeat_owner, wall_ms(), 120_000)
                    .unwrap_or(false)
                {
                    break;
                }
            }
        }))
    };
    loop {
        if !queue.store.lease(&endpoint, &owner, wall_ms(), 120_000)? {
            if once {
                bail!("another supervisor already owns this desktop endpoint");
            }
            return Ok(());
        }
        let result_future = async {
            let cdp = ChatGptCdp::connect_desktop(&endpoint).await?;
            crate::desktop_panel::sync_panel(&queue, &cdp).await?;
            let settings = queue.settings()?;
            if !settings.stop_task.is_empty() {
                if let Some(task) = queue
                    .tasks()?
                    .into_iter()
                    .find(|t| t.id == settings.stop_task)
                    && let Some(binding) = &task.binding
                {
                    cdp.switch_conversation(binding).await?;
                    cdp.stop_response().await?;
                }
                let mut updated = queue.settings()?;
                updated.stop_task.clear();
                queue.set_settings(updated)?;
                return Ok::<(), anyhow::Error>(());
            }
            if !settings.request_open.is_empty() {
                if let Some(task) = queue
                    .tasks()?
                    .into_iter()
                    .find(|t| t.id == settings.request_open)
                    && let Some(binding) = &task.binding
                {
                    cdp.switch_conversation(binding).await?;
                }
                let mut updated = queue.settings()?;
                updated.request_open.clear();
                updated.hold_view_until = wall_ms() + 60_000;
                queue.set_settings(updated)?;
                return Ok::<(), anyhow::Error>(());
            }
            if settings.hold_view_until > wall_ms() {
                return Ok(());
            }
            if settings.request_reload {
                let observation = cdp.task_observation("").await?;
                if !observation.page.response_in_flight()
                    && !observation.approval_present
                    && observation.page.composer_text.trim().is_empty()
                {
                    cdp.reload().await?;
                    let mut updated = queue.settings()?;
                    updated.request_reload = false;
                    queue.set_settings(updated)?;
                }
                return Ok(());
            }
            cdp.dismiss_safe_modals().await?;
            let controller = Supervisor {
                store: queue.store.as_ref(),
                desktop: &cdp,
                clock: &clock,
                policy: SupervisorPolicy::default(),
            };
            let started = wall_ms();
            if let Some(id) = controller.tick().await? {
                let mut stats = queue.settings()?;
                stats.scans += 1;
                stats.last_task = id;
                stats.last_scan_ms = wall_ms().saturating_sub(started);
                queue.set_settings(stats)?;
            }
            Ok(())
        };
        let result = tokio::select! {
            result=result_future=>result,
            _=tokio::signal::ctrl_c()=>{
                if let Some(renewer)=&renewer{renewer.abort();}
                queue.store.release(&endpoint,&owner)?;
                return Ok(());
            }
        };
        if let Err(error) = result {
            eprintln!("supervisor: {error:#}");
            if once {
                queue.store.release(&endpoint, &owner)?;
                return Err(error);
            }
        }
        if once {
            queue.store.release(&endpoint, &owner)?;
            return Ok(());
        }
        tokio::select! {_=tokio::time::sleep(Duration::from_secs(4))=>{},_=tokio::signal::ctrl_c()=>{queue.store.release(&endpoint,&owner)?;if let Some(renewer)=&renewer{renewer.abort();}return Ok(());}}
    }
}

pub async fn activate_task(queue: &Queue, endpoint: &str, id: &str) -> Result<()> {
    use fabushi_chatgpt_application::TaskDesktop;
    let t = queue
        .tasks()?
        .into_iter()
        .find(|t| t.id == id)
        .ok_or_else(|| anyhow::anyhow!("task not found"))?;
    let b = t
        .binding
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("task not bound"))?;
    ChatGptCdp::connect_desktop(endpoint)
        .await?
        .activate(b, &t.token)
        .await
}
