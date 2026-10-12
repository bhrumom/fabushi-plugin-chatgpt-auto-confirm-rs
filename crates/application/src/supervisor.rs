use crate::Clock;
use anyhow::{Result, bail};
use async_trait::async_trait;
use fabushi_chatgpt_domain::*;
use std::time::Duration;

pub trait TaskStore: Send + Sync {
    fn list(&self) -> Result<Vec<Task>>;
    fn insert(&self, task: &Task) -> Result<()>;
    fn save(&self, task: &mut Task, expected: u64, activity: Option<Activity>) -> Result<bool>;
    fn delete(&self, id: &str) -> Result<()>;
    fn activity(&self, id: &str, since: u64, limit: usize) -> Result<Vec<Activity>>;
    fn operations(&self, id: &str, since: u64, limit: usize) -> Result<Vec<Activity>> {
        Ok(self
            .activity(id, since, limit)?
            .into_iter()
            .filter(|a| matches!(a.role.as_str(), "status" | "switch" | "action"))
            .collect())
    }
    fn settings(&self) -> Result<Settings>;
    fn save_settings(&self, settings: &mut Settings) -> Result<bool>;
    fn lease(&self, endpoint: &str, owner: &str, now: u64, ttl: u64) -> Result<bool>;
    fn release(&self, endpoint: &str, owner: &str) -> Result<()>;
    fn heartbeat(&self, endpoint: &str) -> Result<Option<(String, u64)>>;
}
#[async_trait]
pub trait TaskDesktop: Send + Sync {
    async fn new_chat(&self) -> Result<()>;
    async fn activate(&self, binding: &SessionBinding, token: &str) -> Result<()>;
    async fn observe(&self, token: &str) -> Result<TaskObservation>;
    async fn ensure_profile(&self, profile: &ExecutionProfile) -> Result<()>;
    async fn upload(&self, attachments: &[Attachment]) -> Result<()>;
    async fn dispatch(&self, prompt: &str) -> Result<()>;
    async fn approve(&self) -> Result<bool>;
    async fn stay_chat(&self) -> Result<bool>;
    async fn reload(&self) -> Result<()>;
    async fn retry_load(&self) -> Result<bool>;
}
#[derive(Debug, Clone)]
pub struct SupervisorPolicy {
    pub rotation_ms: u64,
    pub max_wait_ms: u64,
    pub send_gap_ms: u64,
    pub confirmation_ms: u64,
    pub stable_ms: u64,
    pub stall_ms: u64,
    pub rate_limit_ms: u64,
    pub approval_reload_ms: u64,
    pub approval_settle_ms: u64,
    pub handoff_guard_ms: u64,
}
impl Default for SupervisorPolicy {
    fn default() -> Self {
        Self {
            rotation_ms: 15_000,
            max_wait_ms: 60_000,
            send_gap_ms: 60_000,
            confirmation_ms: 90_000,
            stable_ms: 4_000,
            stall_ms: 300_000,
            rate_limit_ms: 300_000,
            approval_reload_ms: 60_000,
            approval_settle_ms: 12_000,
            handoff_guard_ms: 8_000,
        }
    }
}
pub struct Supervisor<'a> {
    pub store: &'a dyn TaskStore,
    pub desktop: &'a dyn TaskDesktop,
    pub clock: &'a dyn Clock,
    pub policy: SupervisorPolicy,
}
impl Supervisor<'_> {
    fn now(&self) -> u64 {
        self.clock.now().as_millis().min(u128::from(u64::MAX)) as u64
    }
    fn persist(&self, t: &mut Task, event: Option<(&str, String)>) -> Result<bool> {
        let expected = t.revision;
        t.updated_at = self.now();
        let console_event = event.clone();
        let saved = self.store.save(
            t,
            expected,
            event.map(|(role, text)| Activity {
                task_id: t.id.clone(),
                at: self.now(),
                role: role.into(),
                text,
            }),
        )?;
        if saved && let Some((role, text)) = console_event {
            eprintln!(
                "[{}] task={} round={} phase={:?} {} {}",
                t.updated_at,
                t.id,
                t.round,
                t.phase,
                role,
                if matches!(role, "scan" | "switch" | "status" | "action") {
                    bounded(&text, 1500)
                } else {
                    "内容已保存到私有任务记录；操作日志不输出正文".into()
                }
            );
        }
        Ok(saved)
    }
    fn current(&self, t: &Task) -> Result<bool> {
        Ok(self.store.settings()?.running
            && self.store.list()?.iter().any(|live| {
                live.id == t.id && live.revision == t.revision && live.state.runnable()
            }))
    }
    pub async fn tick(&self) -> Result<Option<String>> {
        let settings = self.store.settings()?;
        if !settings.running {
            return Ok(None);
        }
        let now = self.now();
        let tasks = self.store.list()?;
        let selected = tasks
            .iter()
            .filter(|t| t.state.runnable() && t.cooldown_until <= now)
            .filter(|t| {
                settings.last_navigation_task.is_empty()
                    || settings.last_navigation_task == t.id
                    || now.saturating_sub(settings.last_navigation_at) >= self.policy.rotation_ms
            })
            .min_by_key(|t| {
                (
                    if t.state == TaskState::Sending
                        && t.binding.is_none()
                        && now.saturating_sub(t.dispatch_at) < self.policy.confirmation_ms
                    {
                        0
                    } else if t.terminal_polls > 0 {
                        1
                    } else if now.saturating_sub(t.last_scan_at) >= self.policy.max_wait_ms {
                        2
                    } else if matches!(t.state, TaskState::Sending | TaskState::Approval) {
                        3
                    } else {
                        4
                    },
                    t.last_scan_at,
                    t.created_at,
                )
            });
        let Some(mut t) = selected.cloned() else {
            return Ok(None);
        };
        if !matches!(t.state, TaskState::Sending | TaskState::Approval)
            && t.last_scan_at > 0
            && now.saturating_sub(t.last_scan_at) < self.policy.rotation_ms
        {
            return Ok(None);
        }
        t.last_scan_at = now;
        let id = t.id.clone();
        let mut live_settings = self.store.settings()?;
        live_settings.last_task = id.clone();
        live_settings.last_attempt_at = now;
        self.store.save_settings(&mut live_settings)?;
        let message = format!(
            "开始监督：第 {} 轮 {:?}，状态 {}；准备核对记录会话",
            t.round,
            t.phase,
            t.state.label()
        );
        if !self.persist(&mut t, Some(("scan", message)))? {
            return Ok(None);
        }
        if let Err(error) = self.visit(&mut t, &settings).await {
            // Operational errors remain visible and retry with bounded backoff.
            // Never throw away a confirmed binding solely because a DOM effect failed.
            if self.current(&t)? {
                t.retries += 1;
                t.last_error = error.to_string();
                t.cooldown_until = now
                    + 15_000u64
                        .saturating_mul(1u64 << t.retries.min(4))
                        .min(180_000);
                t.state = if t.send_clicked {
                    TaskState::Waiting
                } else {
                    TaskState::Recovering
                };
                let message = format!("自动恢复等待：{}", t.last_error);
                self.persist(&mut t, Some(("status", message)))?;
            }
        } else if self.current(&t)? {
            let wait = t.cooldown_until.saturating_sub(self.now()) / 1000;
            let description = format!(
                "扫描结束：第 {} 轮 {:?}，{}；授权 {} 次，恢复 {} 次；下一次允许调度 {}s 后",
                t.round,
                t.phase,
                t.state.label(),
                t.approvals,
                t.recoveries,
                wait
            );
            self.persist(&mut t, Some(("scan", description)))?;
        }
        Ok(Some(id))
    }
    async fn visit(&self, t: &mut Task, settings: &Settings) -> Result<()> {
        let now = self.now();
        if !t.send_clicked {
            // Persist each intent before any Send click. An interrupted process
            // reuses exactly this marker and payload on resumption.
            if settings.last_send_at > 0
                && now.saturating_sub(settings.last_send_at) < self.policy.send_gap_ms
            {
                t.cooldown_until = settings.last_send_at + self.policy.send_gap_ms;
                self.persist(t, None)?;
                return Ok(());
            }
            if t.token.is_empty() {
                t.token =
                    text_hash(&format!("{}:{}:{:?}:{}", t.id, t.round, t.phase, now))[..24].into();
                t.prepared_prompt = t.prompt();
                t.dispatch_goal_revision = t.goal_revision;
            }
            t.state = TaskState::Preparing;
            if !self.persist(
                t,
                Some((
                    "status",
                    format!(
                        "准备 {:?} 第 {} 轮，模型 {} / {}",
                        t.phase,
                        t.round,
                        t.profile.model,
                        t.profile.effort_label()
                    ),
                )),
            )? {
                return Ok(());
            }
            let foreground = self.desktop.observe("").await?;
            if foreground.chat_work_offer && !foreground.approval_present {
                let clicked = self.desktop.stay_chat().await?;
                t.cooldown_until = now + if clicked { 3_000 } else { 1_000 };
                self.persist(
                    t,
                    Some((
                        if clicked { "action" } else { "status" },
                        if clicked {
                            "派发前点击当前卡片「留在聊天模式」；等待界面结算后再打开新阶段会话"
                                .into()
                        } else {
                            "派发前发现 Work 转换卡片；等待留在聊天按钮，不切换或发送".into()
                        },
                    )),
                )?;
                return Ok(());
            }
            if foreground.approval_present || !foreground.page.composer_text.trim().is_empty() {
                bail!("当前界面有未结算授权或未发送草稿，保留当前会话");
            }
            self.desktop.new_chat().await?;
            let mut navigation = self.store.settings()?;
            navigation.last_navigation_at = self.now();
            navigation.last_navigation_task = t.id.clone();
            self.store.save_settings(&mut navigation)?;
            self.persist(
                t,
                Some((
                    "action",
                    format!(
                        "点击新聊天：准备第 {} 轮 {:?}；接力原因：{}",
                        t.round,
                        t.phase,
                        if t.last_error.is_empty() {
                            "新阶段派发"
                        } else {
                            &t.last_error
                        }
                    ),
                )),
            )?;
            if !self.current(t)? {
                return Ok(());
            }
            self.desktop.ensure_profile(&t.profile).await?;
            self.desktop.upload(&t.attachments).await?;
            self.persist(
                t,
                Some((
                    "action",
                    format!(
                        "派发预检通过：模型 {} / {}；{} 个附件已就绪",
                        t.profile.model,
                        t.profile.effort_label(),
                        t.attachments.len()
                    ),
                )),
            )?;
            if !self.current(t)? {
                return Ok(());
            }
            let before = self.desktop.observe(&t.token).await?;
            if before.approval_present
                || before.page.response_in_flight()
                || !before.page.composer_text.trim().is_empty()
            {
                bail!("发送前发现授权、生成中回复或未发送草稿");
            }
            t.baseline_users = before.page.user_turns;
            t.dispatch_at = now;
            t.send_clicked = true;
            t.state = TaskState::Sending;
            // Possible-send is durable before the effect. Crash here is reconciled
            // by the marker, rather than blindly replaying a possibly completed click.
            if !self.persist(
                t,
                Some(("status", "发送意图已持久化，等待当前标识确认".into())),
            )? {
                return Ok(());
            }
            if !self.current(t)? {
                return Ok(());
            }
            self.desktop.dispatch(&t.prepared_prompt).await?;
            self.persist(
                t,
                Some((
                    "action",
                    format!(
                        "已触发发送：第 {} 轮 {:?}，{} 字符，任务标记 {}；等待原会话确认",
                        t.round,
                        t.phase,
                        t.prepared_prompt.chars().count(),
                        t.token
                    ),
                )),
            )?;
            let mut config = self.store.settings()?;
            config.last_send_at = self.now();
            self.store.save_settings(&mut config)?;
            return Ok(());
        }
        if let Some(binding) = t.binding.as_ref().or(t.pending_binding.as_ref()) {
            if settings.last_navigation_task != t.id {
                let mut navigation = self.store.settings()?;
                navigation.last_navigation_at = now;
                navigation.last_navigation_task = t.id.clone();
                self.store.save_settings(&mut navigation)?;
            }
            self.desktop.activate(binding, &t.token).await?;
        }
        if !self.current(t)? {
            return Ok(());
        }
        let observed = self.desktop.observe(&t.token).await?;
        if observed.foreign_marker {
            bail!("当前回复属于另一个任务，保留绑定并等待正确会话");
        }
        // A loading error may occur before a new conversation gets a stable id.
        // Keep the possible-send intent and retry this same visible conversation.
        if observed.load_failed || t.load_failure_pending {
            if !observed.load_failed && observed.marker_present && !observed.foreign_marker {
                t.load_failure_pending = false;
                self.persist(
                    t,
                    Some((
                        "action",
                        "原会话加载恢复，任务标记已确认；继续原轮次".into(),
                    )),
                )?;
            } else {
                t.load_failure_pending = true;
                t.cooldown_until = now + 15_000;
                t.recovery_guard_since = 0;
                t.terminal_since = 0;
                t.terminal_polls = 0;
                if t.last_load_retry_at == 0 || now.saturating_sub(t.last_load_retry_at) >= 30_000 {
                    t.last_load_retry_at = now;
                    let clicked = self.desktop.retry_load().await?;
                    t.state = if t.binding.is_none() {
                        TaskState::Sending
                    } else {
                        TaskState::Waiting
                    };
                    self.persist(t, Some((if clicked {"action"} else {"status"}, if clicked {
                        "点击原会话加载错误卡片的「重试」；保留会话、发送意图及上下文；30 秒后再检查".into()
                    } else { "原会话加载尚未恢复：重试控件未就绪；保留发送意图，不新建或重发".into() })))?;
                } else {
                    self.persist(t, None)?;
                }
                return Ok(());
            }
        }
        if observed.chat_work_offer && !observed.approval_present {
            let clicked = self.desktop.stay_chat().await?;
            t.approval_settle_until = now + if clicked { 3_000 } else { 1_000 };
            self.persist(
                t,
                Some((
                    if clicked { "action" } else { "status" },
                    if clicked {
                        "点击 Work 转换卡片的「留在聊天模式」；继续当前聊天任务".into()
                    } else {
                        "识别到 Work 转换卡片，留在聊天按钮尚未就绪；下一次扫描重试".into()
                    },
                )),
            )?;
            return Ok(());
        }
        if t.binding.is_none() {
            if observed.marker_present
                && !observed.foreign_marker
                && !observed.binding.sidebar_id.is_empty()
            {
                t.pending_binding = Some(observed.binding.clone());
            }
            if observed.marker_present
                && observed.page.user_turns > 0
                && !observed.binding.identity.is_empty()
                && !observed.binding.sidebar_id.is_empty()
                && !observed.binding.sidebar_id.starts_with("local-chatgpt:")
            {
                t.binding = Some(observed.binding.clone());
                t.pending_binding = None;
                t.sessions.push(SessionRecord {
                    binding: observed.binding.clone(),
                    phase: t.phase,
                    round: t.round,
                    token: t.token.clone(),
                    at: now,
                });
                if t.sessions.len() > 80 {
                    t.sessions.remove(0);
                }
                t.last_progress_at = now;
                t.last_error.clear();
                t.retries = 0;
                t.state = TaskState::Waiting;
                self.persist(
                    t,
                    Some((
                        "status",
                        format!("已确认派发与会话归属：{}", observed.binding.identity),
                    )),
                )?;
            } else if now.saturating_sub(t.dispatch_at) >= self.policy.confirmation_ms {
                t.state = TaskState::Sending;
                t.cooldown_until = now + 15_000;
                self.persist(
                    t,
                    Some((
                        "status",
                        "发送确认超过 90 秒：保留原发送意图与标记，等待原会话恢复；不重发、不新建"
                            .into(),
                    )),
                )?;
            } else {
                self.persist(t, None)?;
            }
            return Ok(());
        }
        if let Some(binding) = &mut t.binding
            && !binding.sidebar_id.is_empty()
            && binding.sidebar_id == observed.binding.sidebar_id
        {
            binding.identity = observed.binding.identity.clone();
        }
        if let Some(binding) = &mut t.binding
            && binding.sidebar_id.starts_with("local-chatgpt:")
            && observed.marker_present
            && !observed.foreign_marker
            && !observed.binding.sidebar_id.starts_with("local-chatgpt:")
        {
            *binding = observed.binding.clone();
        }
        let binding = t.binding.as_ref().expect("binding checked");
        if observed.binding.identity != binding.identity
            && (binding.sidebar_id.is_empty() || observed.binding.sidebar_id != binding.sidebar_id)
        {
            bail!("会话切换尚未确认，拒绝读取或推进其他任务");
        }
        if observed.marker_present && !observed.foreign_marker {
            let mut confirmed = self.store.settings()?;
            if confirmed.last_confirmed_task != t.id {
                if confirmed.last_confirmed_task.is_empty() {
                    confirmed.switches = 0;
                }
                confirmed.last_confirmed_task = t.id.clone();
                confirmed.switches += 1;
                self.store.save_settings(&mut confirmed)?;
                self.persist(
                    t,
                    Some((
                        "switch",
                        format!(
                            "已确认切换到 {}；第 {} 轮 {:?}；会话 {}；任务标记匹配",
                            t.title, t.round, t.phase, observed.binding.sidebar_id
                        ),
                    )),
                )?;
            }
        }
        t.last_error.clear();
        t.retries = 0;
        let fp = observed.page.activity_fingerprint();
        if fp != t.fingerprint {
            t.fingerprint = fp;
            t.last_progress_at = now;
            t.terminal_since = 0;
            t.terminal_polls = 0;
        }
        let public_progress = if observed.visible_work.trim().is_empty() {
            &observed.page.assistant_text
        } else {
            &observed.visible_work
        };
        if !public_progress.trim().is_empty() {
            t.latest_reply = bounded(public_progress, 64_000);
        }
        if observed.approval_present {
            t.state = TaskState::Approval;
            t.terminal_since = 0;
            t.terminal_polls = 0;
            t.recovery_guard_since = 0;
            if t.approval_since == 0 {
                t.approval_since = now;
            }
            if settings.global_auto_approve
                && t.auto_approve
                && observed.approval_actionable
                && (t.approval_fingerprint != observed.approval_fingerprint
                    || now >= t.approval_settle_until)
                && self.desktop.approve().await?
            {
                t.approval_fingerprint = observed.approval_fingerprint;
                t.approval_settle_until = now + self.policy.approval_settle_ms;
                t.approvals += 1;
                t.approval_since = now;
                self.persist(
                    t,
                    Some((
                        "status",
                        "已选择当前会话授权；等待授权结算，不切换或续发".into(),
                    )),
                )?;
            } else if settings.global_auto_approve
                && t.auto_approve
                && now >= t.approval_settle_until
                && now.saturating_sub(t.approval_since) >= self.policy.approval_reload_ms
            {
                self.desktop.reload().await?;
                t.approval_since = now;
                t.recoveries += 1;
                self.persist(
                    t,
                    Some((
                        "status",
                        "授权控件尚未可用：恢复同一桌面会话，保留派发标识".into(),
                    )),
                )?;
            } else {
                self.persist(t, None)?;
            }
            return Ok(());
        }
        if now < t.approval_settle_until {
            self.persist(t, None)?;
            return Ok(());
        }
        if observed.page.rate_limit_notice {
            t.rate_limit_episodes += 1;
            t.cooldown_until = now + self.policy.rate_limit_ms;
            t.state = TaskState::Cooldown;
            if t.rate_limit_episodes > 3 {
                self.fresh(t, "连续限流，携带进度重新派发")?;
            }
            self.persist(t, Some(("status", "请求频繁：等待五分钟后继续".into())))?;
            return Ok(());
        }
        if !observed.marker_present {
            t.state = TaskState::Waiting;
            self.persist(
                t,
                Some((
                    "status",
                    "原会话内容或任务标记尚未加载：保留绑定与上下文，暂不判定结束或新开会话".into(),
                )),
            )?;
            return Ok(());
        }
        if observed.interrupted || observed.length_limit || observed.empty_final {
            if t.recovery_guard_since == 0 {
                t.recovery_guard_since = now;
            }
            if now.saturating_sub(t.recovery_guard_since)
                >= if t.phase == Phase::Review && observed.empty_final {
                    120_000
                } else {
                    self.policy.handoff_guard_ms
                }
            {
                self.fresh(
                    t,
                    if observed.interrupted {
                        "连接中断：携带现场继续"
                    } else if observed.length_limit {
                        "对话长度达到上限：携带现场继续"
                    } else {
                        "仅有思考记录而无最终正文：携带现场继续"
                    },
                )?;
            }
            self.persist(
                t,
                Some((
                    "status",
                    if t.binding.is_none() {
                        format!("异常接力已保存：{}；等待发送间隔后新建会话", t.last_error)
                    } else {
                        "连接或会话异常：复核授权后等待接力保护期".into()
                    },
                )),
            )?;
            return Ok(());
        }
        if observed.page.is_terminal() && !observed.empty_final {
            t.terminal_polls += 1;
            if t.terminal_since == 0 {
                t.terminal_since = now;
            }
            if t.terminal_polls >= 2
                && now.saturating_sub(t.terminal_since) >= self.policy.stable_ms
            {
                return self.finish(t, &observed.page.assistant_text);
            }
            if t.terminal_polls == 1 {
                self.persist(t,Some(("status", format!("第 {} 轮 {:?} 回复已出现完整结束证据；等待第二次稳定检查，确认后解析验收并推进下一阶段",t.round,t.phase))))?;
            } else {
                self.persist(t, None)?;
            }
            return Ok(());
        }
        t.terminal_since = 0;
        t.terminal_polls = 0;
        t.recovery_guard_since = 0;
        if now.saturating_sub(t.last_progress_at) >= self.policy.stall_ms
            && observed.page.composer_text.trim().is_empty()
        {
            self.desktop.reload().await?;
            t.recoveries += 1;
            t.last_progress_at = now;
            self.persist(
                t,
                Some((
                    "status",
                    "五分钟无进展：恢复同一会话，保持原任务归属".into(),
                )),
            )?;
            return Ok(());
        }
        t.state = if observed.page.response_in_flight() {
            TaskState::Generating
        } else {
            TaskState::Waiting
        };
        self.persist(t, None)?;
        Ok(())
    }
    fn fresh(&self, t: &mut Task, reason: &str) -> Result<()> {
        let source = t
            .binding
            .as_ref()
            .map(|b| b.sidebar_id.as_str())
            .unwrap_or("");
        t.carry = bounded(
            &format!(
                "中断原因：{reason}\n来源会话：{source}\n任务：{}；阶段：{:?}；轮次：{}\n此前接力材料：\n{}\n上一执行轮结果：\n{}\n中断前最后可见回复与工具进度：\n{}",
                t.id, t.phase, t.round, t.carry, t.work_result, t.latest_reply
            ),
            96_000,
        );
        t.reset_dispatch();
        t.recoveries += 1;
        t.last_error = reason.into();
        t.cooldown_until = self.now() + self.policy.send_gap_ms;
        Ok(())
    }
    fn finish(&self, t: &mut Task, reply: &str) -> Result<()> {
        if t.goal_revision != t.dispatch_goal_revision {
            t.phase = Phase::Work;
            t.round += 1;
            t.work_result.clear();
            t.reset_dispatch();
            self.persist(
                t,
                Some((
                    "status",
                    "目标已修改：旧轮次结果保留记录，按新目标执行".into(),
                )),
            )?;
            return Ok(());
        }
        if t.mode == TaskMode::Once {
            t.state = TaskState::Done;
            self.persist(t, Some(("assistant", reply.into())))?;
            return Ok(());
        }
        let completed_phase = t.phase;
        let completed_round = t.round;
        match t.phase {
            Phase::Work => {
                t.work_result = bounded(reply, 64_000);
                t.phase = Phase::Review;
                t.carry.clear();
                t.review_repairs = 0;
                t.reset_dispatch();
            }
            Phase::Review => match ReviewReport::parse(reply, t) {
                Ok(r) => {
                    t.next = r.next;
                    t.carry.clear();
                    if r.status == "complete" {
                        t.state = TaskState::Done;
                    } else {
                        t.phase = Phase::Work;
                        t.round += 1;
                        t.review_repairs = 0;
                        t.reset_dispatch();
                    }
                }
                Err(reason) => {
                    t.review_repairs += 1;
                    t.carry = bounded(reply, 32_000);
                    t.reset_dispatch();
                    t.cooldown_until = self.now()
                        + if t.review_repairs <= 2 {
                            self.policy.send_gap_ms
                        } else {
                            180_000
                        };
                    t.last_error = reason;
                }
            },
        }
        if self.persist(t, Some(("assistant", reply.into())))? {
            let result = if t.state == TaskState::Done {
                format!("独立验收第 {completed_round} 轮报告 complete 且身份匹配；任务标记完成")
            } else if completed_phase == Phase::Work {
                format!("执行第 {completed_round} 轮回复稳定结束；已保存执行结果，进入独立验收阶段")
            } else if t.phase == Phase::Work {
                format!(
                    "独立验收第 {completed_round} 轮给出 next；进入第 {} 轮执行，保留验收下一步",
                    t.round
                )
            } else {
                format!(
                    "独立验收第 {completed_round} 轮报告无效；保留验收阶段等待修复：{}",
                    t.last_error
                )
            };
            self.persist(t, Some(("action", result)))?;
        }
        Ok(())
    }
    pub async fn run(&self) -> Result<()> {
        loop {
            self.tick().await?;
            self.clock.sleep(Duration::from_secs(4)).await;
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn persistent_goal_turn_completion_is_not_goal_completion() {
        let mut t = Task::new("x".into(), "goal".into(), "all".into(), TaskMode::Goal, 1);
        t.state = TaskState::Generating;
        assert!(!t.state.terminal());
        assert_ne!(t.mode, TaskMode::Once);
    }
}

#[cfg(test)]
#[path = "supervisor_tests.rs"]
mod regression;
