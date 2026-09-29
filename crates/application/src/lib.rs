use anyhow::{Result, bail};
use async_trait::async_trait;
use fabushi_chatgpt_domain::{
    ApprovalFingerprint, ExecutionProfile, ObservedExecutionProfile, PageSnapshot, QueueTask,
    RecoveryEnvelope, RunCheckpoint, RunCounters, RunEvent, RunEventKind, RunRecord, RunReport,
    RunState,
};
use serde_json::json;
use std::time::Duration;

pub use fabushi_chatgpt_domain::{
    AutomationTaskReport, TaskReportStatus, TaskState, parse_task_report, parse_task_wait,
};

#[async_trait]
pub trait BrowserPort: Send + Sync {
    async fn snapshot(&self) -> Result<PageSnapshot>;
    async fn send_prompt(&self, prompt: &str) -> Result<()>;
    async fn approve_once(&self) -> Result<bool>;
    async fn dismiss_rate_limit_notice(&self) -> Result<bool>;
    async fn reload(&self) -> Result<()>;
    async fn navigate(&self, url: &str) -> Result<()>;
    async fn ensure_execution_profile(
        &self,
        requested: &ExecutionProfile,
    ) -> Result<ObservedExecutionProfile>;
    async fn target_identity(&self) -> Result<Option<String>>;
}

#[async_trait]
pub trait Clock: Send + Sync {
    fn now(&self) -> Duration;
    fn unix_time_ms(&self) -> i64;
    async fn sleep(&self, duration: Duration);
}

pub trait RunJournal: Send + Sync {
    fn record(&self, event: &RunEvent) -> Result<()>;
    fn begin_approval_attempt(
        &self,
        fingerprint: &ApprovalFingerprint,
        max_attempts: u32,
    ) -> Result<bool>;
    fn settle_approval(&self, fingerprint: &ApprovalFingerprint) -> Result<()>;
    fn load_checkpoint(&self) -> Result<Option<RunCheckpoint>> {
        Ok(None)
    }
    fn record_with_checkpoint(&self, event: &RunEvent, checkpoint: &RunCheckpoint) -> Result<()> {
        let _ = checkpoint;
        self.record(event)
    }
}

#[derive(Debug, Clone)]
pub struct QueueClaim {
    pub task: QueueTask,
    pub run: RunRecord,
}

#[derive(Debug, Clone, Default)]
pub struct QueueSnapshot {
    pub tasks: Vec<QueueTask>,
    pub runs: Vec<RunRecord>,
}

pub trait QueueStore: Send + Sync {
    fn enqueue_task(&self, task: &QueueTask) -> Result<()>;
    fn claim_next_runnable(
        &self,
        owner_id: &str,
        now_ms: i64,
        lease_duration_ms: i64,
    ) -> Result<Option<QueueClaim>>;
    fn renew_lease(
        &self,
        run_id: &str,
        owner_id: &str,
        now_ms: i64,
        lease_duration_ms: i64,
    ) -> Result<bool>;
    fn release_lease(&self, run_id: &str, owner_id: &str) -> Result<()>;
    fn settle_task(
        &self,
        task: &QueueTask,
        report: Option<&AutomationTaskReport>,
        recovery: Option<&RecoveryEnvelope>,
        waiting_until_ms: Option<i64>,
        error: Option<&str>,
    ) -> Result<()>;
    fn recover_expired_leases(&self, now_ms: i64) -> Result<u32>;
    fn snapshot(&self) -> Result<QueueSnapshot>;
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub timeout: Duration,
    pub poll_interval: Duration,
    pub approval_settle_delay: Duration,
    pub reload_settle_delay: Duration,
    pub continuation_settle_delay: Duration,
    pub auto_confirm: bool,
    pub stale_reload_after: Duration,
    pub rate_limit_pause: Duration,
    pub max_rate_limit_pauses: u32,
    pub max_dispatch_retries: u32,
    pub max_refresh_attempts: u32,
    pub max_continuations: u32,
    pub dispatch_confirm_after: Duration,
    pub continuation_after: Duration,
    pub connection_recovery_after: Duration,
    pub execution_profile: Option<ExecutionProfile>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(60 * 60),
            poll_interval: Duration::from_millis(900),
            approval_settle_delay: Duration::from_millis(600),
            reload_settle_delay: Duration::from_secs(3),
            continuation_settle_delay: Duration::from_secs(1),
            auto_confirm: true,
            stale_reload_after: Duration::from_secs(15 * 60),
            rate_limit_pause: Duration::from_secs(5 * 60),
            max_rate_limit_pauses: 3,
            max_dispatch_retries: 3,
            max_refresh_attempts: 3,
            max_continuations: 6,
            dispatch_confirm_after: Duration::from_secs(90),
            continuation_after: Duration::from_secs(30 * 60),
            connection_recovery_after: Duration::from_secs(15 * 60),
            execution_profile: None,
        }
    }
}

pub struct RunPrompt<'a> {
    browser: &'a dyn BrowserPort,
    clock: &'a dyn Clock,
    journal: Option<&'a dyn RunJournal>,
}

impl<'a> RunPrompt<'a> {
    pub fn new(browser: &'a dyn BrowserPort, clock: &'a dyn Clock) -> Self {
        Self {
            browser,
            clock,
            journal: None,
        }
    }

    pub fn with_journal(
        browser: &'a dyn BrowserPort,
        clock: &'a dyn Clock,
        journal: &'a dyn RunJournal,
    ) -> Self {
        Self {
            browser,
            clock,
            journal: Some(journal),
        }
    }

    pub async fn execute(&self, prompt: &str, options: RunOptions) -> Result<RunReport> {
        let mut checkpoint = match self.journal {
            Some(journal) => journal.load_checkpoint()?.unwrap_or_default(),
            None => RunCheckpoint::default(),
        };
        let mut counters = checkpoint.counters.clone();

        if let Some(profile) = &options.execution_profile {
            let observed = self.browser.ensure_execution_profile(profile).await?;
            if !observed.satisfies(profile) {
                bail!(
                    "execution_profile_unverified: requested model={} thinking={} observed={observed:?}",
                    profile.model,
                    profile.thinking_effort
                );
            }
            self.record(event(
                RunEventKind::ExecutionProfileVerified,
                RunState::Dispatching,
                &counters,
                None,
                json!({"model": observed.model, "thinking_effort": observed.thinking_effort}),
            ))?;
        }

        let initial = self.browser.snapshot().await?;
        let now_ms = self.clock.unix_time_ms();
        if checkpoint.run_deadline_ms == 0 {
            checkpoint.run_deadline_ms = add_duration_ms(now_ms, options.timeout);
        }
        if checkpoint.stale_deadline_ms == 0 {
            checkpoint.stale_deadline_ms = add_duration_ms(now_ms, options.stale_reload_after);
        }
        if checkpoint.continuation_deadline_ms == 0 {
            checkpoint.continuation_deadline_ms =
                add_duration_ms(now_ms, options.continuation_after);
        }
        checkpoint.last_observed_at_ms = now_ms;
        checkpoint
            .last_conversation_url
            .clone_from(&initial.canonical_conversation_url());

        if checkpoint.last_committed_outbound_message.is_none() {
            checkpoint.baseline_user_turns = initial.user_turns;
            checkpoint.outbound_baseline_user_turns = initial.user_turns;
            checkpoint.last_committed_outbound_message = Some(prompt.to_owned());
            checkpoint.outbound_delivery_confirmed = false;
            checkpoint.dispatch_attempts = 1;
            checkpoint.dispatch_deadline_ms =
                add_duration_ms(now_ms, options.dispatch_confirm_after);
            checkpoint.last_activity_fingerprint = Some(initial.activity_fingerprint());
            checkpoint.counters = counters.clone();
            self.record_checkpoint(
                event(
                    RunEventKind::PromptDispatchRequested,
                    RunState::Dispatching,
                    &counters,
                    Some(&initial),
                    json!({"attempt": 1, "baseline_user_turns": initial.user_turns}),
                ),
                &checkpoint,
            )?;
            self.browser.send_prompt(prompt).await?;
        } else if checkpoint.dispatch_deadline_ms == 0 && !checkpoint.outbound_delivery_confirmed {
            checkpoint.dispatch_deadline_ms =
                add_duration_ms(now_ms, options.dispatch_confirm_after);
            checkpoint.counters = counters.clone();
        }

        loop {
            let now_ms = self.clock.unix_time_ms();
            if now_ms > checkpoint.run_deadline_ms {
                checkpoint.pending_recovery = Some("global_timeout".into());
                checkpoint.counters = counters.clone();
                self.record_checkpoint(
                    event(
                        RunEventKind::RunFailed,
                        RunState::TimedOut,
                        &counters,
                        None,
                        json!({"reason": "timeout"}),
                    ),
                    &checkpoint,
                )?;
                return Ok(report_from(
                    RunState::TimedOut,
                    checkpoint.last_conversation_url.clone(),
                    String::new(),
                    Vec::new(),
                    &counters,
                    "run timed out before terminal response evidence",
                ));
            }

            if let Some(resume_at) = checkpoint.rate_limit_resume_at_ms {
                if now_ms < resume_at {
                    self.clock
                        .sleep(wait_slice(now_ms, resume_at, options.poll_interval))
                        .await;
                    continue;
                }
                checkpoint.rate_limit_resume_at_ms = None;
                checkpoint.counters = counters.clone();
                self.record_checkpoint(
                    event(
                        RunEventKind::RateLimitBackoffFinished,
                        RunState::Running,
                        &counters,
                        None,
                        json!({}),
                    ),
                    &checkpoint,
                )?;
            }

            let snapshot = self.browser.snapshot().await?;
            checkpoint.last_observed_at_ms = now_ms;
            if let Some(url) = snapshot.canonical_conversation_url() {
                checkpoint.last_conversation_url = Some(url);
            }

            if !checkpoint.outbound_delivery_confirmed {
                if snapshot.user_turns > checkpoint.outbound_baseline_user_turns {
                    checkpoint.outbound_delivery_confirmed = true;
                    checkpoint.dispatch_deadline_ms = 0;
                    checkpoint.stale_deadline_ms =
                        add_duration_ms(now_ms, options.stale_reload_after);
                    checkpoint.continuation_deadline_ms =
                        add_duration_ms(now_ms, options.continuation_after);
                    checkpoint.counters = counters.clone();
                    self.record_checkpoint(
                        event(
                            RunEventKind::OutboundDeliveryConfirmed,
                            RunState::Running,
                            &counters,
                            Some(&snapshot),
                            json!({"user_turns": snapshot.user_turns}),
                        ),
                        &checkpoint,
                    )?;
                } else if now_ms >= checkpoint.dispatch_deadline_ms {
                    if counters.dispatch_retries >= options.max_dispatch_retries {
                        checkpoint.pending_recovery =
                            Some("dispatch_confirmation_retry_limit".into());
                        counters.fresh_conversation_recoveries += 1;
                        checkpoint.counters = counters.clone();
                        self.record_checkpoint(
                            event(
                                RunEventKind::FreshConversationRequested,
                                RunState::Recovering,
                                &counters,
                                Some(&snapshot),
                                json!({"reason": "dispatch_confirmation_retry_limit"}),
                            ),
                            &checkpoint,
                        )?;
                        return Ok(report_from(
                            RunState::Recovering,
                            snapshot.canonical_conversation_url(),
                            snapshot.assistant_text,
                            snapshot.visible_assistant_messages,
                            &counters,
                            "dispatch_confirmation_retry_limit",
                        ));
                    }
                    let outbound = checkpoint
                        .last_committed_outbound_message
                        .clone()
                        .unwrap_or_else(|| prompt.to_owned());
                    counters.dispatch_retries += 1;
                    checkpoint.dispatch_attempts += 1;
                    checkpoint.dispatch_deadline_ms =
                        add_duration_ms(now_ms, options.dispatch_confirm_after);
                    checkpoint.counters = counters.clone();
                    self.record_checkpoint(
                        event(
                            RunEventKind::PromptDispatchRequested,
                            RunState::Dispatching,
                            &counters,
                            Some(&snapshot),
                            json!({"retry": counters.dispatch_retries, "attempt": checkpoint.dispatch_attempts}),
                        ),
                        &checkpoint,
                    )?;
                    self.browser.send_prompt(&outbound).await?;
                }
                if !checkpoint.outbound_delivery_confirmed {
                    self.clock.sleep(options.poll_interval).await;
                    continue;
                }
            }

            let fingerprint = snapshot.activity_fingerprint();
            if checkpoint.last_activity_fingerprint.as_deref() != Some(&fingerprint) {
                checkpoint.last_activity_fingerprint = Some(fingerprint.clone());
                checkpoint.stale_deadline_ms = add_duration_ms(now_ms, options.stale_reload_after);
                checkpoint.terminal_evidence_count = 0;
                checkpoint.counters = counters.clone();
                self.record_checkpoint(
                    event(
                        RunEventKind::SnapshotProgressed,
                        RunState::Running,
                        &counters,
                        Some(&snapshot),
                        json!({"activity_fingerprint": fingerprint}),
                    ),
                    &checkpoint,
                )?;
            }

            if let Some(url) = snapshot.canonical_conversation_url() {
                checkpoint.last_conversation_url = Some(url.clone());
                checkpoint.counters = counters.clone();
                self.record_checkpoint(
                    event(
                        RunEventKind::CanonicalConversationBound,
                        RunState::Running,
                        &counters,
                        Some(&snapshot),
                        json!({"url": url}),
                    ),
                    &checkpoint,
                )?;
            }

            if snapshot.is_terminal() {
                checkpoint.terminal_evidence_count =
                    checkpoint.terminal_evidence_count.saturating_add(1);
                checkpoint.counters = counters.clone();
                self.record_checkpoint(
                    event(
                        RunEventKind::TerminalEvidenceObserved,
                        RunState::Running,
                        &counters,
                        Some(&snapshot),
                        json!({"stable_observation": checkpoint.terminal_evidence_count}),
                    ),
                    &checkpoint,
                )?;
                if checkpoint.terminal_evidence_count >= 2 {
                    checkpoint.pending_recovery = None;
                    checkpoint.counters = counters.clone();
                    self.record_checkpoint(
                        event(
                            RunEventKind::RunCompleted,
                            RunState::Complete,
                            &counters,
                            Some(&snapshot),
                            json!({}),
                        ),
                        &checkpoint,
                    )?;
                    return Ok(report_from(
                        RunState::Complete,
                        snapshot.canonical_conversation_url(),
                        snapshot.assistant_text,
                        snapshot.visible_assistant_messages,
                        &counters,
                        "terminal response action row is stable and bound to the latest user turn",
                    ));
                }
                self.clock.sleep(options.poll_interval).await;
                continue;
            }
            checkpoint.terminal_evidence_count = 0;

            if snapshot.conversation_too_long {
                checkpoint.pending_recovery = Some("conversation_too_long".into());
                counters.fresh_conversation_recoveries += 1;
                checkpoint.counters = counters.clone();
                self.record_checkpoint(
                    event(
                        RunEventKind::ConversationTooLongObserved,
                        RunState::Recovering,
                        &counters,
                        Some(&snapshot),
                        json!({}),
                    ),
                    &checkpoint,
                )?;
                self.record_checkpoint(
                    event(
                        RunEventKind::FreshConversationRequested,
                        RunState::Recovering,
                        &counters,
                        Some(&snapshot),
                        json!({"reason": "conversation_too_long"}),
                    ),
                    &checkpoint,
                )?;
                return Ok(report_from(
                    RunState::Recovering,
                    snapshot.canonical_conversation_url(),
                    snapshot.assistant_text,
                    snapshot.visible_assistant_messages,
                    &counters,
                    "conversation_too_long",
                ));
            }

            if options.auto_confirm && snapshot.waiting_for_approval {
                let should_click = if let Some(fingerprint) = snapshot.approval_fingerprint() {
                    match self.journal {
                        Some(journal) => journal.begin_approval_attempt(&fingerprint, 3)?,
                        None => true,
                    }
                } else {
                    false
                };
                if should_click {
                    self.record(event(
                        RunEventKind::ApprovalObserved,
                        RunState::WaitingApproval,
                        &counters,
                        Some(&snapshot),
                        json!({"fingerprinted": snapshot.approval_fingerprint().is_some()}),
                    ))?;
                    if self.browser.approve_once().await? {
                        counters.approvals_clicked += 1;
                        if let (Some(journal), Some(fingerprint)) =
                            (self.journal, snapshot.approval_fingerprint())
                        {
                            journal.settle_approval(&fingerprint)?;
                        }
                        checkpoint.counters = counters.clone();
                        self.record_checkpoint(
                            event(
                                RunEventKind::ApprovalApplied,
                                RunState::Running,
                                &counters,
                                Some(&snapshot),
                                json!({}),
                            ),
                            &checkpoint,
                        )?;
                        self.clock.sleep(options.approval_settle_delay).await;
                        continue;
                    }
                }
            }

            if snapshot.rate_limit_notice && snapshot.rate_limit_dialog_visible {
                if counters.rate_limit_pauses >= options.max_rate_limit_pauses {
                    checkpoint.pending_recovery = Some("rate_limit_threshold_exceeded".into());
                    counters.fresh_conversation_recoveries += 1;
                    checkpoint.counters = counters.clone();
                    self.record_checkpoint(
                        event(
                            RunEventKind::FreshConversationRequested,
                            RunState::Recovering,
                            &counters,
                            Some(&snapshot),
                            json!({"reason": "rate_limit_threshold_exceeded"}),
                        ),
                        &checkpoint,
                    )?;
                    return Ok(report_from(
                        RunState::Recovering,
                        snapshot.canonical_conversation_url(),
                        snapshot.assistant_text,
                        snapshot.visible_assistant_messages,
                        &counters,
                        "rate_limit_threshold_exceeded",
                    ));
                }
                self.record(event(
                    RunEventKind::RateLimitObserved,
                    RunState::Recovering,
                    &counters,
                    Some(&snapshot),
                    json!({}),
                ))?;
                let dismissed = self.browser.dismiss_rate_limit_notice().await?;
                if snapshot.rate_limit_ack_available && !dismissed {
                    bail!("visible rate-limit acknowledgement could not be dismissed");
                }
                if dismissed {
                    self.record(event(
                        RunEventKind::RateLimitDismissed,
                        RunState::Recovering,
                        &counters,
                        Some(&snapshot),
                        json!({}),
                    ))?;
                }
                counters.rate_limit_pauses += 1;
                checkpoint.rate_limit_resume_at_ms =
                    Some(add_duration_ms(now_ms, options.rate_limit_pause));
                checkpoint.counters = counters.clone();
                self.record_checkpoint(
                    event(
                        RunEventKind::RateLimitBackoffStarted,
                        RunState::Recovering,
                        &counters,
                        Some(&snapshot),
                        json!({"pause_seconds": options.rate_limit_pause.as_secs()}),
                    ),
                    &checkpoint,
                )?;
                continue;
            }

            if snapshot.connection_interrupted {
                if checkpoint.connection_recovery_deadline_ms.is_none() {
                    counters.connection_interruptions += 1;
                    checkpoint.connection_recovery_deadline_ms =
                        Some(add_duration_ms(now_ms, options.connection_recovery_after));
                    checkpoint.counters = counters.clone();
                    self.record_checkpoint(
                        event(
                            RunEventKind::ConnectionInterrupted,
                            RunState::Recovering,
                            &counters,
                            Some(&snapshot),
                            json!({}),
                        ),
                        &checkpoint,
                    )?;
                }
            } else {
                checkpoint.connection_recovery_deadline_ms = None;
            }

            let connection_due = checkpoint
                .connection_recovery_deadline_ms
                .is_some_and(|deadline| now_ms >= deadline);
            let stale_due = now_ms >= checkpoint.stale_deadline_ms;
            if connection_due || stale_due {
                if checkpoint.refresh_attempts >= options.max_refresh_attempts {
                    checkpoint.pending_recovery = Some("refresh_attempt_limit_exceeded".into());
                    counters.fresh_conversation_recoveries += 1;
                    checkpoint.counters = counters.clone();
                    self.record_checkpoint(
                        event(
                            RunEventKind::FreshConversationRequested,
                            RunState::Recovering,
                            &counters,
                            Some(&snapshot),
                            json!({"reason": "refresh_attempt_limit_exceeded"}),
                        ),
                        &checkpoint,
                    )?;
                    return Ok(report_from(
                        RunState::Recovering,
                        snapshot.canonical_conversation_url(),
                        snapshot.assistant_text,
                        snapshot.visible_assistant_messages,
                        &counters,
                        "refresh_attempt_limit_exceeded",
                    ));
                }
                self.record(event(
                    RunEventKind::RecoveryReloadRequested,
                    RunState::Recovering,
                    &counters,
                    Some(&snapshot),
                    json!({"reason": if connection_due {"connection_interrupted"} else {"stale"}}),
                ))?;
                self.browser.reload().await?;
                counters.recoveries += 1;
                counters.refresh_attempts += 1;
                checkpoint.refresh_attempts += 1;
                checkpoint.stale_deadline_ms = add_duration_ms(now_ms, options.stale_reload_after);
                checkpoint.connection_recovery_deadline_ms = if connection_due {
                    Some(add_duration_ms(now_ms, options.connection_recovery_after))
                } else {
                    None
                };
                checkpoint.counters = counters.clone();
                self.record_checkpoint(
                    event(
                        RunEventKind::RecoveryReloadApplied,
                        RunState::Running,
                        &counters,
                        Some(&snapshot),
                        json!({"attempt": checkpoint.refresh_attempts}),
                    ),
                    &checkpoint,
                )?;
                self.clock.sleep(options.reload_settle_delay).await;
                continue;
            }

            if now_ms >= checkpoint.continuation_deadline_ms && !snapshot.response_in_flight() {
                if counters.continuations >= options.max_continuations {
                    checkpoint.pending_recovery =
                        Some("continuation_attempt_limit_exceeded".into());
                    counters.fresh_conversation_recoveries += 1;
                    checkpoint.counters = counters.clone();
                    self.record_checkpoint(
                        event(
                            RunEventKind::FreshConversationRequested,
                            RunState::Recovering,
                            &counters,
                            Some(&snapshot),
                            json!({"reason": "continuation_attempt_limit_exceeded"}),
                        ),
                        &checkpoint,
                    )?;
                    return Ok(report_from(
                        RunState::Recovering,
                        snapshot.canonical_conversation_url(),
                        snapshot.assistant_text,
                        snapshot.visible_assistant_messages,
                        &counters,
                        "continuation_attempt_limit_exceeded",
                    ));
                }
                checkpoint.outbound_baseline_user_turns = snapshot.user_turns;
                checkpoint.last_committed_outbound_message = Some("继续完成所有".into());
                checkpoint.outbound_delivery_confirmed = false;
                checkpoint.dispatch_attempts = 1;
                checkpoint.dispatch_deadline_ms =
                    add_duration_ms(now_ms, options.dispatch_confirm_after);
                checkpoint.continuation_deadline_ms =
                    add_duration_ms(now_ms, options.continuation_after);
                counters.continuations += 1;
                checkpoint.counters = counters.clone();
                self.record_checkpoint(
                    event(
                        RunEventKind::ContinuationRequested,
                        RunState::Running,
                        &counters,
                        Some(&snapshot),
                        json!({"prompt": "continue_all", "attempt": counters.continuations}),
                    ),
                    &checkpoint,
                )?;
                self.browser.send_prompt("继续完成所有").await?;
                self.clock.sleep(options.continuation_settle_delay).await;
                continue;
            }

            checkpoint.counters = counters.clone();
            self.clock.sleep(options.poll_interval).await;
        }
    }

    fn record_checkpoint(&self, event: RunEvent, checkpoint: &RunCheckpoint) -> Result<()> {
        if let Some(journal) = self.journal {
            journal.record_with_checkpoint(&event, checkpoint)?;
        }
        Ok(())
    }

    fn record(&self, event: RunEvent) -> Result<()> {
        if let Some(journal) = self.journal {
            journal.record(&event)?;
        }
        Ok(())
    }
}

fn event(
    kind: RunEventKind,
    state: RunState,
    counters: &RunCounters,
    snapshot: Option<&PageSnapshot>,
    payload: serde_json::Value,
) -> RunEvent {
    let mut event = RunEvent::new(kind, state, counters.clone());
    event.payload_json = payload.to_string();
    if let Some(snapshot) = snapshot {
        event.canonical_conversation_url = snapshot.canonical_conversation_url();
        event.activity_fingerprint = Some(snapshot.activity_fingerprint());
        if !snapshot.assistant_text.trim().is_empty() {
            event.latest_assistant_text = Some(snapshot.assistant_text.clone());
        }
        event.visible_progress_messages = snapshot.visible_assistant_messages.clone();
    }
    event
}

fn report_from(
    state: RunState,
    conversation_url: Option<String>,
    assistant_text: String,
    visible_progress_messages: Vec<String>,
    counters: &RunCounters,
    message: &str,
) -> RunReport {
    RunReport {
        state,
        conversation_url,
        assistant_text,
        visible_progress_messages,
        approvals_clicked: counters.approvals_clicked,
        recoveries: counters.recoveries,
        rate_limit_pauses: counters.rate_limit_pauses,
        dispatch_retries: counters.dispatch_retries,
        continuations: counters.continuations,
        message: message.into(),
    }
}

fn add_duration_ms(now_ms: i64, duration: Duration) -> i64 {
    now_ms.saturating_add(duration.as_millis().min(i64::MAX as u128) as i64)
}

fn wait_slice(now_ms: i64, deadline_ms: i64, poll: Duration) -> Duration {
    let remaining = deadline_ms.saturating_sub(now_ms).max(1) as u64;
    Duration::from_millis(remaining).min(poll)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;

    struct FakeClock {
        now: Mutex<Duration>,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                now: Mutex::new(Duration::ZERO),
            }
        }
    }

    #[async_trait]
    impl Clock for FakeClock {
        fn now(&self) -> Duration {
            *self.now.lock().expect("clock poisoned")
        }

        fn unix_time_ms(&self) -> i64 {
            self.now
                .lock()
                .expect("clock poisoned")
                .as_millis()
                .min(i64::MAX as u128) as i64
        }

        async fn sleep(&self, duration: Duration) {
            let mut now = self.now.lock().expect("clock poisoned");
            *now += duration;
        }
    }

    struct FakeBrowser {
        snapshots: Mutex<VecDeque<PageSnapshot>>,
        last_snapshot: Mutex<PageSnapshot>,
        sent: Mutex<Vec<String>>,
        approvals: Mutex<u32>,
        dismissals: Mutex<u32>,
        reloads: Mutex<u32>,
        observed_profile: Mutex<ObservedExecutionProfile>,
    }

    impl FakeBrowser {
        fn new(snapshots: impl IntoIterator<Item = PageSnapshot>) -> Self {
            let snapshots: VecDeque<_> = snapshots.into_iter().collect();
            let first = snapshots.front().cloned().unwrap_or_default();
            Self {
                snapshots: Mutex::new(snapshots),
                last_snapshot: Mutex::new(first),
                sent: Mutex::new(Vec::new()),
                approvals: Mutex::new(0),
                dismissals: Mutex::new(0),
                reloads: Mutex::new(0),
                observed_profile: Mutex::new(ObservedExecutionProfile::default()),
            }
        }

        fn sent(&self) -> Vec<String> {
            self.sent.lock().expect("sent poisoned").clone()
        }
    }

    #[async_trait]
    impl BrowserPort for FakeBrowser {
        async fn snapshot(&self) -> Result<PageSnapshot> {
            if let Some(next) = self
                .snapshots
                .lock()
                .expect("snapshots poisoned")
                .pop_front()
            {
                *self.last_snapshot.lock().expect("last snapshot poisoned") = next.clone();
                Ok(next)
            } else {
                Ok(self
                    .last_snapshot
                    .lock()
                    .expect("last snapshot poisoned")
                    .clone())
            }
        }

        async fn send_prompt(&self, prompt: &str) -> Result<()> {
            self.sent
                .lock()
                .expect("sent poisoned")
                .push(prompt.to_owned());
            Ok(())
        }

        async fn approve_once(&self) -> Result<bool> {
            *self.approvals.lock().expect("approvals poisoned") += 1;
            Ok(true)
        }

        async fn dismiss_rate_limit_notice(&self) -> Result<bool> {
            *self.dismissals.lock().expect("dismissals poisoned") += 1;
            Ok(true)
        }

        async fn reload(&self) -> Result<()> {
            *self.reloads.lock().expect("reloads poisoned") += 1;
            Ok(())
        }

        async fn navigate(&self, _url: &str) -> Result<()> {
            Ok(())
        }

        async fn ensure_execution_profile(
            &self,
            requested: &ExecutionProfile,
        ) -> Result<ObservedExecutionProfile> {
            let mut observed = self.observed_profile.lock().expect("profile poisoned");
            if observed.model.is_none() {
                observed.model = Some(requested.model.clone());
                observed.thinking_effort = Some(requested.thinking_effort.clone());
            }
            Ok(observed.clone())
        }

        async fn target_identity(&self) -> Result<Option<String>> {
            Ok(Some("target-1".into()))
        }
    }

    #[derive(Default)]
    struct FakeJournal {
        events: Mutex<Vec<RunEvent>>,
        approvals: Mutex<HashMap<String, (u32, bool)>>,
    }

    impl RunJournal for FakeJournal {
        fn record(&self, event: &RunEvent) -> Result<()> {
            self.events
                .lock()
                .expect("events poisoned")
                .push(event.clone());
            Ok(())
        }

        fn begin_approval_attempt(
            &self,
            fingerprint: &ApprovalFingerprint,
            max_attempts: u32,
        ) -> Result<bool> {
            let mut approvals = self.approvals.lock().expect("approvals poisoned");
            let entry = approvals.entry(fingerprint.0.clone()).or_insert((0, false));
            if entry.1 || entry.0 >= max_attempts {
                return Ok(false);
            }
            entry.0 += 1;
            Ok(true)
        }

        fn settle_approval(&self, fingerprint: &ApprovalFingerprint) -> Result<()> {
            let mut approvals = self.approvals.lock().expect("approvals poisoned");
            approvals
                .entry(fingerprint.0.clone())
                .or_insert((0, false))
                .1 = true;
            Ok(())
        }
    }

    fn terminal_snapshot() -> PageSnapshot {
        PageSnapshot {
            url: "https://chatgpt.com/c/abc123".into(),
            user_turns: 1,
            assistant_turns: 1,
            copy_available_on_last_assistant: true,
            response_actions_complete: true,
            response_action_turn_bound_to_last: true,
            assistant_message_settled: true,
            assistant_text: "done".into(),
            visible_assistant_messages: vec!["done".into()],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn completes_only_after_two_stable_terminal_observations() {
        let browser = FakeBrowser::new([
            PageSnapshot::default(),
            terminal_snapshot(),
            terminal_snapshot(),
        ]);
        let clock = FakeClock::new();
        let report = RunPrompt::new(&browser, &clock)
            .execute("hello", RunOptions::default())
            .await
            .expect("run should complete");
        assert_eq!(report.state, RunState::Complete);
        assert_eq!(
            report.conversation_url.as_deref(),
            Some("https://chatgpt.com/c/abc123")
        );
        assert_eq!(browser.sent(), vec!["hello"]);
    }

    #[tokio::test]
    async fn resends_when_dispatch_is_not_confirmed_for_90_seconds() {
        let delivered = terminal_snapshot();
        let browser = FakeBrowser::new([
            PageSnapshot::default(),
            PageSnapshot::default(),
            PageSnapshot::default(),
            PageSnapshot::default(),
            PageSnapshot::default(),
            delivered.clone(),
            delivered,
        ]);
        let clock = FakeClock::new();
        let options = RunOptions {
            poll_interval: Duration::from_secs(30),
            ..RunOptions::default()
        };
        let report = RunPrompt::new(&browser, &clock)
            .execute("original", options)
            .await
            .expect("run should complete");
        assert_eq!(report.dispatch_retries, 1);
        assert_eq!(browser.sent(), vec!["original", "original"]);
    }

    #[tokio::test]
    async fn durable_approval_is_fingerprinted_and_settled_once() {
        let approval = PageSnapshot {
            user_turns: 1,
            waiting_for_approval: true,
            approval_card_key: Some("tool-card-42|allow-once".into()),
            ..Default::default()
        };
        let browser = FakeBrowser::new([
            PageSnapshot::default(),
            approval,
            terminal_snapshot(),
            terminal_snapshot(),
        ]);
        let clock = FakeClock::new();
        let journal = FakeJournal::default();
        let report = RunPrompt::with_journal(&browser, &clock, &journal)
            .execute("needs tool", RunOptions::default())
            .await
            .expect("run should complete");
        assert_eq!(report.approvals_clicked, 1);
        assert_eq!(*browser.approvals.lock().expect("approvals poisoned"), 1);
        assert!(
            journal
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.kind == RunEventKind::ApprovalApplied)
        );
    }

    #[tokio::test]
    async fn rate_limit_pause_is_counted_and_resumed() {
        let limited = PageSnapshot {
            user_turns: 1,
            rate_limit_notice: true,
            rate_limit_dialog_visible: true,
            rate_limit_ack_available: true,
            ..Default::default()
        };
        let browser = FakeBrowser::new([
            PageSnapshot::default(),
            limited,
            terminal_snapshot(),
            terminal_snapshot(),
        ]);
        let clock = FakeClock::new();
        let report = RunPrompt::new(&browser, &clock)
            .execute("hello", RunOptions::default())
            .await
            .expect("run should complete");
        assert_eq!(report.rate_limit_pauses, 1);
    }

    #[tokio::test]
    async fn reloads_after_15_minutes_without_progress() {
        let idle = PageSnapshot {
            user_turns: 1,
            ..Default::default()
        };
        let browser = FakeBrowser::new([
            PageSnapshot::default(),
            idle.clone(),
            idle.clone(),
            idle.clone(),
            idle,
            terminal_snapshot(),
            terminal_snapshot(),
        ]);
        let clock = FakeClock::new();
        let options = RunOptions {
            poll_interval: Duration::from_secs(5 * 60),
            continuation_after: Duration::from_secs(60 * 60),
            ..RunOptions::default()
        };
        let report = RunPrompt::new(&browser, &clock)
            .execute("original", options)
            .await
            .expect("run should complete");
        assert_eq!(report.recoveries, 1);
    }

    #[tokio::test]
    async fn continues_after_30_minutes_without_terminal_completion() {
        let idle = PageSnapshot {
            user_turns: 1,
            ..Default::default()
        };
        let mut terminal = terminal_snapshot();
        terminal.user_turns = 2;
        terminal.assistant_turns = 2;
        let browser = FakeBrowser::new([
            PageSnapshot::default(),
            idle.clone(),
            idle.clone(),
            idle.clone(),
            idle,
            terminal.clone(),
            terminal,
        ]);
        let clock = FakeClock::new();
        let options = RunOptions {
            poll_interval: Duration::from_secs(10 * 60),
            stale_reload_after: Duration::from_secs(2 * 60 * 60),
            ..RunOptions::default()
        };
        let report = RunPrompt::new(&browser, &clock)
            .execute("original", options)
            .await
            .expect("run should complete");
        assert_eq!(report.continuations, 1);
        assert_eq!(browser.sent(), vec!["original", "继续完成所有"]);
    }

    #[tokio::test]
    async fn execution_profile_fails_closed() {
        let browser = FakeBrowser::new([PageSnapshot::default()]);
        *browser.observed_profile.lock().unwrap() = ObservedExecutionProfile {
            model: Some("GPT-5.6 Sol".into()),
            thinking_effort: Some("High".into()),
        };
        let clock = FakeClock::new();
        let options = RunOptions {
            execution_profile: Some(ExecutionProfile::default()),
            ..RunOptions::default()
        };
        let error = RunPrompt::new(&browser, &clock)
            .execute("hello", options)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("execution_profile_unverified"));
        assert!(browser.sent().is_empty());
    }
}
