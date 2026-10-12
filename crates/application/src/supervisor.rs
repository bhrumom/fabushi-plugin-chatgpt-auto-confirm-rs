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
                if matches!(role, "scan" | "switch" | "status") {
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
            .min_by_key(|t| {
                (
                    if t.state == TaskState::Sending || t.state == TaskState::Approval {
                        0
                    } else {
                        1
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
            if foreground.approval_present || !foreground.page.composer_text.trim().is_empty() {
                bail!("当前界面有未结算授权或未发送草稿，保留当前会话");
            }
            self.desktop.new_chat().await?;
            if !self.current(t)? {
                return Ok(());
            }
            self.desktop.ensure_profile(&t.profile).await?;
            self.desktop.upload(&t.attachments).await?;
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
            let mut config = self.store.settings()?;
            config.last_send_at = self.now();
            self.store.save_settings(&mut config)?;
            return Ok(());
        }
        if let Some(binding) = &t.binding {
            self.desktop.activate(binding, &t.token).await?;
        }
        if !self.current(t)? {
            return Ok(());
        }
        let observed = self.desktop.observe(&t.token).await?;
        if observed.foreign_marker {
            bail!("当前回复属于另一个任务，保留绑定并等待正确会话");
        }
        if t.binding.is_none() {
            if observed.marker_present
                && observed.page.user_turns > t.baseline_users
                && !observed.binding.identity.is_empty()
                && !observed.binding.sidebar_id.is_empty()
                && !observed.binding.sidebar_id.starts_with("local-chatgpt:")
            {
                t.binding = Some(observed.binding.clone());
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
            } else if now.saturating_sub(t.dispatch_at) >= self.policy.confirmation_ms
                && !observed.approval_present
                && !observed.page.response_in_flight()
                && observed.page.composer_text.trim().is_empty()
            {
                t.reset_dispatch();
                t.retries += 1;
                t.cooldown_until = now + 15_000u64.saturating_mul(u64::from(t.retries.min(12)));
                self.persist(
                    t,
                    Some((
                        "status",
                        "90 秒未确认原发送，重新排队同一阶段；不会重复已确认的会话".into(),
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
        if observed.chat_work_offer {
            self.desktop.stay_chat().await?;
            t.approval_settle_until = now + 45_000;
            self.persist(
                t,
                Some(("status", "已选择留在聊天模式，等待界面恢复".into())),
            )?;
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
        if observed.load_failed {
            // Loading failure is a retry of this same conversation, not a
            // new model turn or reason to discard its saved checkpoint.
            if t.last_load_retry_at == 0 || now.saturating_sub(t.last_load_retry_at) >= 30_000 {
                t.last_load_retry_at = now;
                let clicked = self.desktop.retry_load().await?;
                t.state = TaskState::Waiting;
                self.persist(
                    t,
                    Some((
                        "status",
                        if clicked {
                            "对话无法加载：已点击原会话重试，30 秒后仍失败再重试".into()
                        } else {
                            "对话无法加载：等待重试按钮，保留原会话与上下文".into()
                        },
                    )),
                )?;
            } else {
                self.persist(t, None)?;
            }
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
            self.persist(t, None)?;
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
        self.persist(t, Some(("assistant", reply.into())))?;
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
