use anyhow::{Result, bail};
use async_trait::async_trait;
use fabushi_chatgpt_domain::{
    ApprovalSettlementKey, AssistantResponseBoundary, AuthorizationSettlementState,
    ChatSurfaceSnapshot, GoalRevision, OwnershipConfidence, Phase, ReasoningPreset, ReviewStatus,
    Round, RunReport, RunState, TaskId,
};
use serde::Deserialize;
use std::time::Duration;

pub const AUTHORIZATION_SETTLEMENT_WINDOW: Duration = Duration::from_secs(12);
pub const NO_APPROVAL_RECHECK_WINDOW: Duration = Duration::from_secs(8);
pub const ORDINARY_TERMINAL_STABILITY: Duration = Duration::from_secs(4);
pub const RECOVERED_TERMINAL_STABILITY: Duration = Duration::from_secs(8);
pub const REVIEW_FINAL_SETTLEMENT_WINDOW: Duration = Duration::from_secs(120);
pub const REASONING_PICKER_RECOVERY_WINDOW: Duration = Duration::from_secs(60);
pub const ATTACHMENT_INITIAL_WAIT: Duration = Duration::from_secs(45);
pub const DISPATCH_CONFIRM_WINDOW: Duration = Duration::from_secs(90);
pub const GENERIC_STALL_WINDOW: Duration = Duration::from_secs(15 * 60);
pub const EXPLICIT_LOAD_RETRY_WINDOW: Duration = Duration::from_secs(30);
pub const EXPLICIT_LOAD_MAX_RECOVERIES: u32 = 7;
pub const RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(5 * 60);
pub const RATE_LIMIT_PRESERVE_EPISODES: u32 = 3;
pub const MAX_CONVERSATION_CARRY_CHARS: usize = 64_000;

#[async_trait]
pub trait ChatSurfacePort: Send + Sync {
    async fn observe(&self) -> Result<ChatSurfaceSnapshot>;
    async fn send_prompt(&self, prompt: &str) -> Result<()>;
    async fn approve_current_conversation(&self) -> Result<bool>;
    async fn dismiss_rate_limit_notice(&self) -> Result<bool>;
    async fn recover_current_surface(&self) -> Result<()>;
    async fn start_fresh_conversation(&self) -> Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatProcessHealth {
    Running,
    NotRunning,
}

#[async_trait]
pub trait ChatProcessPort: Send + Sync {
    async fn health(&self) -> Result<ChatProcessHealth>;
    async fn ensure_running(&self) -> Result<()>;
}

#[async_trait]
pub trait Clock: Send + Sync {
    fn now(&self) -> Duration;
    async fn sleep(&self, duration: Duration);
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub timeout: Duration,
    pub poll_interval: Duration,
    pub auto_confirm: bool,
    pub continuation_settle_delay: Duration,
    pub stale_reload_after: Duration,
    pub rate_limit_pause: Duration,
    pub max_rate_limit_pauses: u32,
    pub dispatch_confirm_after: Duration,
    pub continuation_after: Duration,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(3600),
            poll_interval: Duration::from_millis(900),
            auto_confirm: true,
            continuation_settle_delay: Duration::from_secs(1),
            stale_reload_after: GENERIC_STALL_WINDOW,
            rate_limit_pause: RATE_LIMIT_COOLDOWN,
            max_rate_limit_pauses: RATE_LIMIT_PRESERVE_EPISODES,
            dispatch_confirm_after: DISPATCH_CONFIRM_WINDOW,
            continuation_after: Duration::from_secs(30 * 60),
        }
    }
}

/// Compatibility runner kept while runtime composition migrates to durable desktop actors.
/// It is not migration-ledger evidence for desktop parity.
pub struct RunPrompt<'a> {
    surface: &'a dyn ChatSurfacePort,
    clock: &'a dyn Clock,
}

impl<'a> RunPrompt<'a> {
    pub fn new(surface: &'a dyn ChatSurfacePort, clock: &'a dyn Clock) -> Self {
        Self { surface, clock }
    }

    pub async fn execute(&self, prompt: &str, options: RunOptions) -> Result<RunReport> {
        let before = self.surface.observe().await?;
        let baseline = before.user_turn_boundary.clone();
        self.surface.send_prompt(prompt).await?;

        let started = self.clock.now();
        let mut dispatched = started;
        let mut progress = started;
        let mut fingerprint = String::new();
        let mut terminal_since = None;
        let mut approvals = 0;
        let mut recoveries = 0;
        let mut rate_limits = 0;
        let mut dispatch_retries = 0;

        loop {
            let now = self.clock.now();
            if now.saturating_sub(started) > options.timeout {
                return Ok(RunReport {
                    state: RunState::Failed,
                    conversation_ref: None,
                    assistant_text: String::new(),
                    approvals_clicked: approvals,
                    recoveries,
                    rate_limit_pauses: rate_limits,
                    dispatch_retries,
                    message: "run timed out".into(),
                });
            }

            let snapshot = self.surface.observe().await?;
            let dispatch_confirmed = snapshot.user_turn_boundary.is_some()
                && snapshot.user_turn_boundary != baseline
                && snapshot.user_turn_ownership == OwnershipConfidence::Strong;

            if !dispatch_confirmed {
                if now.saturating_sub(dispatched) >= options.dispatch_confirm_after {
                    self.surface.start_fresh_conversation().await?;
                    self.surface.send_prompt(prompt).await?;
                    recoveries += 1;
                    dispatch_retries += 1;
                    dispatched = now;
                }
                self.clock.sleep(options.poll_interval).await;
                continue;
            }

            let current_fingerprint = snapshot.activity_fingerprint();
            if current_fingerprint != fingerprint {
                fingerprint = current_fingerprint;
                progress = now;
                terminal_since = None;
            }

            if options.auto_confirm
                && snapshot.authorization_surface_present
                && snapshot.authorization_actionable
                && self.surface.approve_current_conversation().await?
            {
                approvals += 1;
                self.clock.sleep(AUTHORIZATION_SETTLEMENT_WINDOW).await;
                continue;
            }

            if snapshot.rate_limit && self.surface.dismiss_rate_limit_notice().await? {
                rate_limits += 1;
                if rate_limits > options.max_rate_limit_pauses {
                    self.surface.start_fresh_conversation().await?;
                    recoveries += 1;
                    rate_limits = 0;
                }
                self.clock.sleep(options.rate_limit_pause).await;
                continue;
            }

            if snapshot.ordinary_terminal_evidence() {
                let since = *terminal_since.get_or_insert(now);
                if now.saturating_sub(since) >= ORDINARY_TERMINAL_STABILITY {
                    return Ok(RunReport {
                        state: RunState::Complete,
                        conversation_ref: snapshot.conversation_ref,
                        assistant_text: snapshot.assistant_visible_prose,
                        approvals_clicked: approvals,
                        recoveries,
                        rate_limit_pauses: rate_limits,
                        dispatch_retries,
                        message: "stable current-response terminal evidence".into(),
                    });
                }
            } else {
                terminal_since = None;
            }

            if now.saturating_sub(progress) >= options.stale_reload_after {
                self.surface.recover_current_surface().await?;
                recoveries += 1;
                progress = now;
                continue;
            }

            self.clock.sleep(options.poll_interval).await;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizationSettlementLatch {
    key: ApprovalSettlementKey,
    started_at: Duration,
}

impl AuthorizationSettlementLatch {
    pub fn new(key: ApprovalSettlementKey, started_at: Duration) -> Self {
        Self { key, started_at }
    }

    pub fn key(&self) -> &ApprovalSettlementKey {
        &self.key
    }

    pub fn is_active(&self, now: Duration) -> bool {
        now.saturating_sub(self.started_at) < AUTHORIZATION_SETTLEMENT_WINDOW
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NoApprovalConfirmation {
    first_absent_at: Option<Duration>,
}

impl NoApprovalConfirmation {
    pub fn observe(&mut self, snapshot: &ChatSurfaceSnapshot, now: Duration) -> bool {
        if snapshot.authorization_surface_present
            || snapshot.authorization_settlement == AuthorizationSettlementState::Settling
        {
            self.first_absent_at = None;
            return false;
        }

        let first = *self.first_absent_at.get_or_insert(now);
        now.saturating_sub(first) >= NO_APPROVAL_RECHECK_WINDOW
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalStabilityTracker {
    boundary: Option<AssistantResponseBoundary>,
    since: Option<Duration>,
}

impl TerminalStabilityTracker {
    pub fn observe(
        &mut self,
        snapshot: &ChatSurfaceSnapshot,
        now: Duration,
        recovered: bool,
    ) -> bool {
        if !snapshot.ordinary_terminal_evidence() {
            self.boundary = None;
            self.since = None;
            return false;
        }

        let boundary = snapshot
            .assistant_response_boundary
            .clone()
            .expect("terminal boundary");
        if self.boundary.as_ref() != Some(&boundary) {
            self.boundary = Some(boundary);
            self.since = Some(now);
            return false;
        }

        let required = if recovered {
            RECOVERED_TERMINAL_STABILITY
        } else {
            ORDINARY_TERMINAL_STABILITY
        };
        now.saturating_sub(self.since.unwrap_or(now)) >= required
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryDecision {
    Stay,
    RecoverCurrentSurface,
    FreshConversationWithCarry,
    RetryCurrentResponseOnce,
    CoolDown(Duration),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryState {
    pub explicit_load_attempts: u32,
    pub explicit_load_first_seen_at: Option<Duration>,
    pub rate_limit_episodes: u32,
    pub cache_retry_failure_identity: Option<String>,
}

impl RecoveryState {
    pub fn decide(
        &mut self,
        snapshot: &ChatSurfaceSnapshot,
        now: Duration,
        failure_identity: Option<&str>,
    ) -> RecoveryDecision {
        if snapshot.connection_interrupted
            || snapshot.stream_polling_timeout
            || snapshot.conversation_length_limit
        {
            return RecoveryDecision::FreshConversationWithCarry;
        }

        if snapshot.stream_cache_expired {
            let identity = failure_identity.unwrap_or_default().to_owned();
            if self.cache_retry_failure_identity.as_deref() != Some(identity.as_str()) {
                self.cache_retry_failure_identity = Some(identity);
                return RecoveryDecision::RetryCurrentResponseOnce;
            }
            return RecoveryDecision::Stay;
        }

        if snapshot.unable_to_load_conversation {
            let first = *self.explicit_load_first_seen_at.get_or_insert(now);
            if now.saturating_sub(first) < EXPLICIT_LOAD_RETRY_WINDOW {
                return RecoveryDecision::Stay;
            }
            if self.explicit_load_attempts < EXPLICIT_LOAD_MAX_RECOVERIES {
                self.explicit_load_attempts += 1;
                self.explicit_load_first_seen_at = Some(now);
                return RecoveryDecision::RecoverCurrentSurface;
            }
            return RecoveryDecision::FreshConversationWithCarry;
        }

        self.explicit_load_attempts = 0;
        self.explicit_load_first_seen_at = None;

        if snapshot.rate_limit {
            self.rate_limit_episodes += 1;
            if self.rate_limit_episodes <= RATE_LIMIT_PRESERVE_EPISODES {
                return RecoveryDecision::CoolDown(RATE_LIMIT_COOLDOWN);
            }
            self.rate_limit_episodes = 0;
            return RecoveryDecision::FreshConversationWithCarry;
        }

        RecoveryDecision::Stay
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewReport {
    pub task_id: TaskId,
    pub round: Round,
    pub status: ReviewStatus,
    pub summary: String,
    pub next: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawReviewReport {
    #[serde(rename = "taskId")]
    task_id: String,
    round: u32,
    status: String,
    summary: String,
    #[serde(default)]
    next: String,
}

pub fn parse_strict_review_report(
    value: &str,
    task: &TaskId,
    round: Round,
) -> Result<ReviewReport> {
    let raw: RawReviewReport = serde_json::from_str(value.trim())?;

    if raw.task_id != task.as_str() || raw.round != round.get() {
        bail!("review report identity mismatch");
    }

    let status = match raw.status.as_str() {
        "complete" => ReviewStatus::Complete,
        "next" => ReviewStatus::Next,
        _ => bail!("invalid review status"),
    };

    let summary = raw.summary.trim().to_owned();
    if summary.is_empty() {
        bail!("empty review summary");
    }

    let next = raw.next.trim().to_owned();
    if status == ReviewStatus::Next && next.is_empty() {
        bail!("status=next requires next");
    }

    Ok(ReviewReport {
        task_id: task.clone(),
        round,
        status,
        summary,
        next: if next.is_empty() { None } else { Some(next) },
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContinuousTaskState {
    pub task_id: TaskId,
    pub phase: Phase,
    pub round: Round,
    pub goal_revision: GoalRevision,
    pub goal: String,
    pub previous_work_result: Option<String>,
    pub current_next: Option<String>,
    pub reasoning_preset: ReasoningPreset,
}

impl ContinuousTaskState {
    pub fn after_work_result(mut self, result: String) -> Self {
        self.previous_work_result = Some(result);
        self.phase = Phase::Review;
        self
    }

    pub fn apply_review(mut self, report: ReviewReport) -> Result<Self> {
        if report.task_id != self.task_id || report.round != self.round {
            bail!("review identity mismatch");
        }

        if report.status == ReviewStatus::Next {
            self.current_next = report.next;
            self.round = Round::new(self.round.get() + 1);
            self.phase = Phase::Work;
        }

        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fabushi_chatgpt_domain::{ConversationFingerprint, RunId};

    fn task_id() -> TaskId {
        TaskId::new("task-1")
    }

    fn terminal() -> ChatSurfaceSnapshot {
        ChatSurfaceSnapshot {
            app_healthy: true,
            assistant_response_boundary: Some(AssistantResponseBoundary::new("a1")),
            assistant_response_ownership: OwnershipConfidence::Strong,
            response_local_copy: true,
            ..Default::default()
        }
    }

    #[test]
    fn approval_latch_is_twelve_seconds() {
        let key = ApprovalSettlementKey {
            task_id: task_id(),
            run_id: RunId::new("r"),
            phase: Phase::Work,
            round: Round::new(1),
            conversation_fingerprint: ConversationFingerprint::new("c"),
        };
        let latch = AuthorizationSettlementLatch::new(key, Duration::from_secs(10));
        assert!(latch.is_active(Duration::from_secs(21)));
        assert!(!latch.is_active(Duration::from_secs(22)));
    }

    #[test]
    fn no_approval_requires_second_stable_window() {
        let mut confirmation = NoApprovalConfirmation::default();
        let snapshot = terminal();
        assert!(!confirmation.observe(&snapshot, Duration::ZERO));
        assert!(!confirmation.observe(&snapshot, Duration::from_secs(7)));
        assert!(confirmation.observe(&snapshot, Duration::from_secs(8)));
    }

    #[test]
    fn load_failure_recovers_seven_then_handoffs() {
        let snapshot = ChatSurfaceSnapshot {
            unable_to_load_conversation: true,
            ..Default::default()
        };
        let mut state = RecoveryState::default();

        assert_eq!(
            state.decide(&snapshot, Duration::ZERO, None),
            RecoveryDecision::Stay
        );
        for attempt in 1..=7 {
            assert_eq!(
                state.decide(&snapshot, Duration::from_secs(attempt * 30), None),
                RecoveryDecision::RecoverCurrentSurface
            );
        }
        assert_eq!(
            state.decide(&snapshot, Duration::from_secs(240), None),
            RecoveryDecision::FreshConversationWithCarry
        );
    }

    #[test]
    fn connection_interruption_handoffs_immediately() {
        let snapshot = ChatSurfaceSnapshot {
            connection_interrupted: true,
            ..Default::default()
        };
        assert_eq!(
            RecoveryState::default().decide(&snapshot, Duration::ZERO, None),
            RecoveryDecision::FreshConversationWithCarry
        );
    }

    #[test]
    fn fourth_rate_limit_episode_handoffs() {
        let snapshot = ChatSurfaceSnapshot {
            rate_limit: true,
            ..Default::default()
        };
        let mut state = RecoveryState::default();

        for _ in 0..3 {
            assert_eq!(
                state.decide(&snapshot, Duration::ZERO, None),
                RecoveryDecision::CoolDown(RATE_LIMIT_COOLDOWN)
            );
        }
        assert_eq!(
            state.decide(&snapshot, Duration::ZERO, None),
            RecoveryDecision::FreshConversationWithCarry
        );
    }

    #[test]
    fn review_is_bound_to_current_task_and_round() {
        let report = parse_strict_review_report(
            r#"{"taskId":"task-1","round":2,"status":"next","summary":"more","next":"fix"}"#,
            &task_id(),
            Round::new(2),
        )
        .unwrap();
        assert_eq!(report.next.as_deref(), Some("fix"));

        assert!(
            parse_strict_review_report(
                r#"{"taskId":"old","round":2,"status":"complete","summary":"done","next":""}"#,
                &task_id(),
                Round::new(2),
            )
            .is_err()
        );
    }
}
