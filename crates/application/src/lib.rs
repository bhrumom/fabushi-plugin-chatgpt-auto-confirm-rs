use anyhow::{Result, bail};
use async_trait::async_trait;
use fabushi_chatgpt_domain::{
    ApprovalFingerprint, ExecutionProfile, ObservedExecutionProfile, PageSnapshot, QueueTask,
    RecoveryEnvelope, RunCounters, RunEvent, RunEventKind, RunRecord, RunReport, RunState,
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
    pub dispatch_confirm_after: Duration,
    pub continuation_after: Duration,
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
            dispatch_confirm_after: Duration::from_secs(90),
            continuation_after: Duration::from_secs(30 * 60),
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
        let mut counters = RunCounters::default();

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

        let before = self.browser.snapshot().await?;
        let baseline_users = before.user_turns;
        self.record(event(
            RunEventKind::PromptDispatchRequested,
            RunState::Dispatching,
            &counters,
            Some(&before),
            json!({"baseline_user_turns": baseline_users}),
        ))?;
        self.browser.send_prompt(prompt).await?;

        let started_at = self.clock.now();
        let mut dispatch_started_at = started_at;
        let mut last_progress_at = started_at;
        let mut last_continuation_at = started_at;
        let mut last_fingerprint = String::new();
        let mut stable_terminal_count = 0u8;
        let mut dispatch_confirmed = false;

        loop {
            let now = self.clock.now();
            if elapsed(now, started_at) > options.timeout {
                let report = report_from(
                    RunState::TimedOut,
                    None,
                    String::new(),
                    &counters,
                    "run timed out before terminal response evidence",
                );
                self.record(event(
                    RunEventKind::RunFailed,
                    RunState::TimedOut,
                    &counters,
                    None,
                    json!({"reason": "timeout"}),
                ))?;
                return Ok(report);
            }

            let snapshot = self.browser.snapshot().await?;

            if snapshot.user_turns < baseline_users + 1 {
                if elapsed(now, dispatch_started_at) >= options.dispatch_confirm_after {
                    self.record(event(
                        RunEventKind::PromptDispatchRequested,
                        RunState::Dispatching,
                        &counters,
                        Some(&snapshot),
                        json!({"retry": counters.dispatch_retries + 1}),
                    ))?;
                    self.browser.send_prompt(prompt).await?;
                    counters.dispatch_retries += 1;
                    dispatch_started_at = now;
                }
                self.clock.sleep(options.poll_interval).await;
                continue;
            }

            if !dispatch_confirmed {
                dispatch_confirmed = true;
                self.record(event(
                    RunEventKind::PromptDispatchConfirmed,
                    RunState::Running,
                    &counters,
                    Some(&snapshot),
                    json!({"user_turns": snapshot.user_turns}),
                ))?;
            }

            let fingerprint = snapshot.activity_fingerprint();
            if fingerprint != last_fingerprint {
                last_fingerprint = fingerprint.clone();
                last_progress_at = now;
                stable_terminal_count = 0;
                self.record(event(
                    RunEventKind::SnapshotProgressed,
                    RunState::Running,
                    &counters,
                    Some(&snapshot),
                    json!({"activity_fingerprint": fingerprint}),
                ))?;
                if snapshot.connection_interrupted {
                    counters.connection_interruptions += 1;
                }
            }

            if let Some(url) = snapshot.canonical_conversation_url() {
                self.record(event(
                    RunEventKind::CanonicalConversationBound,
                    RunState::Running,
                    &counters,
                    Some(&snapshot),
                    json!({"url": url}),
                ))?;
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
                        self.record(event(
                            RunEventKind::ApprovalApplied,
                            RunState::Running,
                            &counters,
                            Some(&snapshot),
                            json!({}),
                        ))?;
                        self.clock.sleep(options.approval_settle_delay).await;
                        continue;
                    }
                }
            }

            if snapshot.rate_limit_notice {
                self.record(event(
                    RunEventKind::RateLimitObserved,
                    RunState::Recovering,
                    &counters,
                    Some(&snapshot),
                    json!({}),
                ))?;
                if self.browser.dismiss_rate_limit_notice().await? {
                    counters.rate_limit_pauses += 1;
                    if counters.rate_limit_pauses > options.max_rate_limit_pauses {
                        bail!(
                            "rate-limit dialog repeated more than {} times",
                            options.max_rate_limit_pauses
                        );
                    }
                    self.record(event(
                        RunEventKind::RateLimitDismissed,
                        RunState::Recovering,
                        &counters,
                        Some(&snapshot),
                        json!({"pause_seconds": options.rate_limit_pause.as_secs()}),
                    ))?;
                    self.clock.sleep(options.rate_limit_pause).await;
                    continue;
                }
            }

            if snapshot.is_terminal() {
                stable_terminal_count += 1;
                self.record(event(
                    RunEventKind::TerminalEvidenceObserved,
                    RunState::Running,
                    &counters,
                    Some(&snapshot),
                    json!({"stable_observation": stable_terminal_count}),
                ))?;
                if stable_terminal_count >= 2 {
                    self.record(event(
                        RunEventKind::RunCompleted,
                        RunState::Complete,
                        &counters,
                        Some(&snapshot),
                        json!({}),
                    ))?;
                    return Ok(report_from(
                        RunState::Complete,
                        snapshot.canonical_conversation_url(),
                        snapshot.assistant_text,
                        &counters,
                        "terminal response action row is stable and bound to the latest user turn",
                    ));
                }
            } else {
                stable_terminal_count = 0;
            }

            if elapsed(now, last_progress_at) >= options.stale_reload_after {
                self.record(event(
                    RunEventKind::RecoveryReloadRequested,
                    RunState::Recovering,
                    &counters,
                    Some(&snapshot),
                    json!({}),
                ))?;
                self.browser.reload().await?;
                counters.recoveries += 1;
                self.record(event(
                    RunEventKind::RecoveryReloadApplied,
                    RunState::Running,
                    &counters,
                    Some(&snapshot),
                    json!({}),
                ))?;
                last_progress_at = now;
                self.clock.sleep(options.reload_settle_delay).await;
                continue;
            }

            if elapsed(now, last_continuation_at) >= options.continuation_after
                && !snapshot.response_in_flight()
                && !snapshot.is_terminal()
            {
                self.record(event(
                    RunEventKind::ContinuationRequested,
                    RunState::Running,
                    &counters,
                    Some(&snapshot),
                    json!({"prompt": "continue_all"}),
                ))?;
                self.browser.send_prompt("继续完成所有").await?;
                counters.continuations += 1;
                last_continuation_at = now;
                last_progress_at = now;
                self.clock.sleep(options.continuation_settle_delay).await;
                continue;
            }

            self.clock.sleep(options.poll_interval).await;
        }
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
    }
    event
}

fn report_from(
    state: RunState,
    conversation_url: Option<String>,
    assistant_text: String,
    counters: &RunCounters,
    message: &str,
) -> RunReport {
    RunReport {
        state,
        conversation_url,
        assistant_text,
        approvals_clicked: counters.approvals_clicked,
        recoveries: counters.recoveries,
        rate_limit_pauses: counters.rate_limit_pauses,
        dispatch_retries: counters.dispatch_retries,
        continuations: counters.continuations,
        message: message.into(),
    }
}

fn elapsed(now: Duration, earlier: Duration) -> Duration {
    now.saturating_sub(earlier)
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
            self.sent.lock().expect("sent poisoned").push(prompt.to_owned());
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
            self.events.lock().expect("events poisoned").push(event.clone());
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
            approvals.entry(fingerprint.0.clone()).or_insert((0, false)).1 = true;
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
            assistant_text: "done".into(),
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
        assert_eq!(report.conversation_url.as_deref(), Some("https://chatgpt.com/c/abc123"));
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
        assert!(journal.events.lock().unwrap().iter().any(|event| event.kind == RunEventKind::ApprovalApplied));
    }

    #[tokio::test]
    async fn rate_limit_pause_is_counted_and_resumed() {
        let limited = PageSnapshot {
            user_turns: 1,
            rate_limit_notice: true,
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
            model: Some("GPT-5.6".into()),
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
