use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PageSnapshot {
    pub url: String,
    pub title: String,
    pub user_turns: usize,
    pub assistant_turns: usize,
    pub stop_available: bool,
    pub waiting_for_approval: bool,
    pub rate_limit_notice: bool,
    pub rate_limit_dialog_visible: bool,
    pub rate_limit_ack_available: bool,
    pub connection_interrupted: bool,
    pub conversation_too_long: bool,
    pub conversation_loaded: bool,
    pub authentication_required: bool,
    pub composer_ready: bool,
    pub send_unavailable: bool,
    pub assistant_streaming: bool,
    pub assistant_message_settled: bool,
    pub copy_available_on_last_assistant: bool,
    pub response_actions_complete: bool,
    pub response_action_turn_bound_to_last: bool,
    pub awaiting_assistant: bool,
    pub assistant_text: String,
    #[serde(default)]
    pub visible_assistant_messages: Vec<String>,
    pub composer_text: String,
    pub approval_card_key: Option<String>,
    pub observed_model: Option<String>,
    pub observed_thinking_effort: Option<String>,
}

impl PageSnapshot {
    pub fn terminal_evidence(&self) -> bool {
        self.response_actions_complete
            && self.response_action_turn_bound_to_last
            && self.assistant_message_settled
            && !self.awaiting_assistant
            && self.copy_available_on_last_assistant
    }

    pub fn response_in_flight(&self) -> bool {
        self.stop_available
            || self.assistant_streaming
            || self.waiting_for_approval
            || self.awaiting_assistant
    }

    pub fn is_terminal(&self) -> bool {
        !self.response_in_flight() && self.terminal_evidence()
    }

    pub fn canonical_conversation_url(&self) -> Option<String> {
        let prefixes = ["https://chatgpt.com/c/", "https://chat.openai.com/c/"];
        for prefix in prefixes {
            if let Some(rest) = self.url.strip_prefix(prefix) {
                let conversation_id = rest
                    .split(['?', '#', '/'])
                    .next()
                    .unwrap_or_default()
                    .trim();
                if !conversation_id.is_empty()
                    && conversation_id
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                {
                    return Some(format!("{prefix}{conversation_id}"));
                }
            }
        }
        None
    }

    pub fn activity_fingerprint(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.url.as_bytes());
        hasher.update(self.user_turns.to_le_bytes());
        hasher.update(self.assistant_turns.to_le_bytes());
        hasher.update(self.assistant_text.as_bytes());
        hasher.update([self.stop_available as u8, self.waiting_for_approval as u8]);
        format!("{:x}", hasher.finalize())
    }

    pub fn approval_fingerprint(&self) -> Option<ApprovalFingerprint> {
        self.approval_card_key
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(ApprovalFingerprint::from_source)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Dispatching,
    Running,
    WaitingApproval,
    Recovering,
    Complete,
    TimedOut,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RunCounters {
    pub approvals_clicked: u32,
    pub recoveries: u32,
    pub rate_limit_pauses: u32,
    pub dispatch_retries: u32,
    pub continuations: u32,
    pub connection_interruptions: u32,
    pub refresh_attempts: u32,
    pub fresh_conversation_recoveries: u32,
    pub target_recoveries: u32,
    pub browser_recoveries: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationKind {
    #[default]
    Work,
    Acceptance,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QueuePhase {
    #[default]
    Queued,
    Dispatched,
    Submitting,
    Submitted,
    AwaitingAcknowledgement,
    AwaitingResponse,
    Interrupted,
    RateLimited,
    Recovering,
    Continuing,
    Completed,
    FailedRetryable,
    PermanentlyFailed,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RunCheckpoint {
    pub conversation_kind: ConversationKind,
    pub counters: RunCounters,
    pub run_deadline_ms: i64,
    pub baseline_user_turns: usize,
    pub outbound_baseline_user_turns: usize,
    pub dispatch_attempts: u32,
    pub outbound_delivery_confirmed: bool,
    pub dispatch_deadline_ms: i64,
    pub stale_deadline_ms: i64,
    pub connection_recovery_deadline_ms: Option<i64>,
    pub rate_limit_resume_at_ms: Option<i64>,
    pub continuation_deadline_ms: i64,
    pub refresh_attempts: u32,
    pub terminal_evidence_count: u8,
    pub last_activity_fingerprint: Option<String>,
    pub last_committed_outbound_message: Option<String>,
    pub last_conversation_url: Option<String>,
    pub pending_recovery: Option<String>,
    pub last_observed_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunReport {
    pub state: RunState,
    pub conversation_url: Option<String>,
    pub assistant_text: String,
    #[serde(default)]
    pub visible_progress_messages: Vec<String>,
    pub approvals_clicked: u32,
    pub recoveries: u32,
    pub rate_limit_pauses: u32,
    pub dispatch_retries: u32,
    pub continuations: u32,
    pub message: String,
}

impl RunReport {
    pub fn counters(&self) -> RunCounters {
        RunCounters {
            approvals_clicked: self.approvals_clicked,
            recoveries: self.recoveries,
            rate_limit_pauses: self.rate_limit_pauses,
            dispatch_retries: self.dispatch_retries,
            continuations: self.continuations,
            connection_interruptions: 0,
            refresh_attempts: 0,
            fresh_conversation_recoveries: 0,
            target_recoveries: 0,
            browser_recoveries: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionProfile {
    pub model: String,
    pub thinking_effort: String,
    #[serde(default)]
    pub connector_requirements: Vec<String>,
    pub tool_mode: Option<String>,
}

impl Default for ExecutionProfile {
    fn default() -> Self {
        Self {
            model: "GPT-5.6 Sol".into(),
            thinking_effort: "Extra High".into(),
            connector_requirements: Vec::new(),
            tool_mode: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObservedExecutionProfile {
    pub model: Option<String>,
    pub thinking_effort: Option<String>,
}

impl ObservedExecutionProfile {
    pub fn satisfies(&self, requested: &ExecutionProfile) -> bool {
        matches_requested(self.model.as_deref(), &requested.model)
            && matches_requested(self.thinking_effort.as_deref(), &requested.thinking_effort)
    }
}

fn matches_requested(observed: Option<&str>, requested: &str) -> bool {
    let requested = normalize_label(requested);
    observed
        .map(normalize_label)
        .is_some_and(|value| value == requested || value.contains(&requested))
}

fn normalize_label(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ApprovalFingerprint(pub String);

impl ApprovalFingerprint {
    pub fn from_source(source: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(source.trim().as_bytes());
        Self(format!("{:x}", hasher.finalize()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Waiting,
    Running,
    Completed,
    Blocked,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueTask {
    pub id: String,
    pub account_id: String,
    pub title: String,
    pub prompt: String,
    pub original_prompt: String,
    pub acceptance_prompt: Option<String>,
    #[serde(default)]
    pub conversation_kind: ConversationKind,
    #[serde(default)]
    pub known_exact_head: Option<String>,
    #[serde(default)]
    pub known_ci_evidence: Vec<String>,
    #[serde(default)]
    #[serde(default)]
    pub current_stage: Option<String>,
    #[serde(default)]
    pub pending_work: Vec<String>,
    #[serde(default)]
    pub context_references: Vec<String>,
    #[serde(default)]
    pub phase: QueuePhase,
    pub current_revision: u64,
    pub applied_revision: Option<u64>,
    pub spec_digest: Option<String>,
    pub connector: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub resource_locks: Vec<String>,
    pub priority: i32,
    pub timeout_seconds: u64,
    pub max_task_continuations: u32,
    pub max_runtime_retries: u32,
    pub continuation_depth: u32,
    pub runtime_retries: u32,
    pub attempts: u32,
    pub status: TaskState,
    pub waiting_until_ms: Option<i64>,
    pub execution_profile: ExecutionProfile,
    pub recovery_context: Option<RecoveryEnvelope>,
    pub last_report: Option<AutomationTaskReport>,
    pub last_error: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl QueueTask {
    pub fn new(
        id: impl Into<String>,
        account_id: impl Into<String>,
        prompt: impl Into<String>,
    ) -> Self {
        let prompt = prompt.into();
        Self {
            id: id.into(),
            account_id: account_id.into(),
            title: String::new(),
            prompt: prompt.clone(),
            original_prompt: prompt,
            acceptance_prompt: None,
            conversation_kind: ConversationKind::Work,
            known_exact_head: None,
            known_ci_evidence: Vec::new(),
            current_stage: None,
            pending_work: Vec::new(),
            context_references: Vec::new(),
            phase: QueuePhase::Queued,
            current_revision: 1,
            applied_revision: None,
            spec_digest: None,
            connector: "GitHub".into(),
            depends_on: Vec::new(),
            resource_locks: Vec::new(),
            priority: 0,
            timeout_seconds: 21_600,
            max_task_continuations: 6,
            max_runtime_retries: 2,
            continuation_depth: 0,
            runtime_retries: 0,
            attempts: 0,
            status: TaskState::Queued,
            waiting_until_ms: None,
            execution_profile: ExecutionProfile::default(),
            recovery_context: None,
            last_report: None,
            last_error: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunRecord {
    pub run_id: String,
    pub task_id: String,
    pub state: RunState,
    pub revision: u64,
    pub canonical_conversation_url: Option<String>,
    pub target_id: Option<String>,
    pub last_activity_fingerprint: Option<String>,
    pub latest_assistant_text: Option<String>,
    #[serde(default)]
    pub visible_progress_messages: Vec<String>,
    pub counters: RunCounters,
    #[serde(default)]
    pub checkpoint: RunCheckpoint,
    pub started_at_ms: i64,
    pub finished_at_ms: Option<i64>,
}

impl RunRecord {
    pub fn new(run_id: impl Into<String>, task_id: impl Into<String>, started_at_ms: i64) -> Self {
        Self {
            run_id: run_id.into(),
            task_id: task_id.into(),
            state: RunState::Dispatching,
            revision: 0,
            canonical_conversation_url: None,
            target_id: None,
            last_activity_fingerprint: None,
            latest_assistant_text: None,
            visible_progress_messages: Vec::new(),
            counters: RunCounters::default(),
            checkpoint: RunCheckpoint::default(),
            started_at_ms,
            finished_at_ms: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunEventKind {
    RunStarted,
    ExecutionProfileVerified,
    PromptDispatchRequested,
    PromptDispatchConfirmed,
    SnapshotProgressed,
    ApprovalObserved,
    ApprovalApplied,
    RateLimitObserved,
    RateLimitDismissed,
    RateLimitBackoffStarted,
    RateLimitBackoffFinished,
    ConnectionInterrupted,
    ConversationTooLongObserved,
    RecoveryReloadRequested,
    RecoveryReloadApplied,
    ContinuationRequested,
    OutboundDeliveryConfirmed,
    FreshConversationRequested,
    FreshConversationStarted,
    CanonicalConversationBound,
    TargetLost,
    BrowserLost,
    TargetReattached,
    TerminalEvidenceObserved,
    RunCompleted,
    RunFailed,
    RunCancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunEvent {
    pub kind: RunEventKind,
    pub state: RunState,
    pub canonical_conversation_url: Option<String>,
    pub target_id: Option<String>,
    pub activity_fingerprint: Option<String>,
    pub latest_assistant_text: Option<String>,
    #[serde(default)]
    pub visible_progress_messages: Vec<String>,
    pub counters: RunCounters,
    pub payload_json: String,
}

impl RunEvent {
    pub fn new(kind: RunEventKind, state: RunState, counters: RunCounters) -> Self {
        Self {
            kind,
            state,
            canonical_conversation_url: None,
            target_id: None,
            activity_fingerprint: None,
            latest_assistant_text: None,
            visible_progress_messages: Vec::new(),
            counters,
            payload_json: "{}".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecoveryEnvelope {
    pub version: u32,
    pub task_id: String,
    pub run_id: String,
    pub exact_commit: Option<String>,
    pub conversation_url: Option<String>,
    #[serde(default)]
    pub conversation_kind: ConversationKind,
    pub original_goal: String,
    pub acceptance_prompt: Option<String>,
    #[serde(default)]
    pub interrupted_turn_visible_content: Vec<String>,
    #[serde(default)]
    pub progress_messages: Vec<String>,
    #[serde(default)]
    pub completed: Vec<String>,
    #[serde(default)]
    pub remaining: Vec<String>,
    #[serde(default)]
    pub blockers: Vec<String>,
    #[serde(default)]
    pub known_ci_evidence: Vec<String>,
    pub current_stage: Option<String>,
    #[serde(default)]
    pub pending_work: Vec<String>,
    #[serde(default)]
    pub context_references: Vec<String>,
    #[serde(default)]
    pub last_committed_outbound_message: Option<String>,
    #[serde(default)]
    pub outbound_delivery_confirmed: bool,
    #[serde(default)]
    pub checkpoint: Option<RunCheckpoint>,
    pub continuation_instruction: String,
}

impl RecoveryEnvelope {
    pub fn render_prompt(&self) -> String {
        fn section(title: &str, values: &[String]) -> String {
            if values.is_empty() {
                return format!("## {title}\n- 无");
            }
            format!(
                "## {title}\n{}",
                values
                    .iter()
                    .map(|value| format!("- {value}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        }

        let mut parts = vec![
            "这是一次异常会话后的接力恢复。不要重复已经完成的工作。".to_string(),
            format!("RecoveryEnvelope v{}", self.version),
            format!("task_id: {}", self.task_id),
            format!("run_id: {}", self.run_id),
        ];
        if let Some(commit) = &self.exact_commit {
            parts.push(format!("exact_commit: {commit}"));
        }
        if let Some(url) = &self.conversation_url {
            parts.push(format!("canonical_conversation_url: {url}"));
        }
        parts.push(format!("conversation_kind: {:?}", self.conversation_kind));
        parts.push(format!("## 原始目标\n{}", self.original_goal));
        if let Some(prompt) = &self.acceptance_prompt {
            parts.push(format!("## 验收/规划会话最终提示词\n{prompt}"));
        }
        parts.push(section("上一轮异常中断前的实时回复内容", &self.interrupted_turn_visible_content));
        parts.push(section("异常前实时工作进展", &self.progress_messages));
        parts.push(section("已完成", &self.completed));
        parts.push(section("当前待继续事项", &self.pending_work));
        parts.push(section("剩余", &self.remaining));
        parts.push(section("已知 CI evidence", &self.known_ci_evidence));
        parts.push(section("阻塞", &self.blockers));
        parts.push(section("附件/上下文引用", &self.context_references));
        if let Some(stage) = &self.current_stage {
            parts.push(format!("## 当前阶段\n{stage}"));
        }
        if let Some(message) = &self.last_committed_outbound_message {
            parts.push(format!("## 最近一次已提交 outbound\n{message}\nconfirmed={}", self.outbound_delivery_confirmed));
        }
        parts.push(format!("## 接力要求\n{}", self.continuation_instruction));
        parts.join("\n\n")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskReportStatus {
    Complete,
    Incomplete,
    Blocked,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AutomationTaskReport {
    #[serde(rename = "protocol")]
    pub protocol_name: String,
    pub task_id: String,
    pub applied_task_revision: u64,
    pub applied_spec_digest: String,
    pub status: TaskReportStatus,
    pub all_tasks_complete: bool,
    pub summary: String,
    pub completed: Vec<String>,
    pub remaining: Vec<String>,
    pub blockers: Vec<String>,
    pub verification: Vec<String>,
    pub next_task: String,
    pub wait_seconds: Option<u64>,
    pub wait_reason: Option<String>,
    pub next_connector: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskWait {
    pub task_id: String,
    pub wait_seconds: u64,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskReportError(pub String);

impl fmt::Display for TaskReportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TaskReportError {}

pub fn parse_task_report(content: &str) -> Result<Option<AutomationTaskReport>, TaskReportError> {
    const BEGIN: &str = "MAHAYANA_TASK_REPORT_V1_BEGIN";
    const END: &str = "MAHAYANA_TASK_REPORT_V1_END";
    let Some(start) = content.rfind(BEGIN) else {
        return Ok(None);
    };
    let after = &content[start + BEGIN.len()..];
    let Some(end) = after.find(END) else {
        return Ok(None);
    };
    let mut raw = after[..end].trim();
    if let Some(rest) = raw.strip_prefix("```json") {
        raw = rest.trim();
    } else if let Some(rest) = raw.strip_prefix("```") {
        raw = rest.trim();
    }
    if let Some(rest) = raw.strip_suffix("```") {
        raw = rest.trim();
    }

    let report: AutomationTaskReport =
        serde_json::from_str(raw).map_err(|error| TaskReportError(error.to_string()))?;
    validate_task_report(&report)?;
    Ok(Some(report))
}

fn validate_task_report(report: &AutomationTaskReport) -> Result<(), TaskReportError> {
    if report.protocol_name != "mahayana.task-report.v1" {
        return Err(TaskReportError("unsupported task report protocol".into()));
    }
    if report.task_id.trim().is_empty() || report.applied_task_revision == 0 {
        return Err(TaskReportError(
            "task report identity/revision is invalid".into(),
        ));
    }
    if report.wait_seconds.unwrap_or(0) > 604_800 {
        return Err(TaskReportError(
            "task report wait_seconds exceeds seven days".into(),
        ));
    }
    for value in report
        .completed
        .iter()
        .chain(&report.remaining)
        .chain(&report.blockers)
    {
        if value.trim().is_empty() {
            return Err(TaskReportError(
                "task report lists may not contain empty items".into(),
            ));
        }
    }

    match report.status {
        TaskReportStatus::Complete => {
            if !report.all_tasks_complete
                || !report.remaining.is_empty()
                || !report.blockers.is_empty()
                || report.wait_seconds.unwrap_or(0) != 0
                || !report.next_task.trim().is_empty()
            {
                return Err(TaskReportError(
                    "complete report violates terminal completion contract".into(),
                ));
            }
        }
        TaskReportStatus::Incomplete | TaskReportStatus::Blocked => {
            if report.all_tasks_complete || report.next_task.trim().is_empty() {
                return Err(TaskReportError(
                    "non-terminal report requires all_tasks_complete=false and next_task".into(),
                ));
            }
        }
    }
    Ok(())
}

pub fn parse_task_wait(content: &str) -> Result<Option<TaskWait>, TaskReportError> {
    const MARKER: &str = "MAHAYANA_TASK_WAIT_V1";
    let Some(start) = content.rfind(MARKER) else {
        return Ok(None);
    };
    let suffix = content[start + MARKER.len()..].trim();
    let Some(open) = suffix.find('{') else {
        return Ok(None);
    };
    let Some(close_rel) = suffix[open..].find('}') else {
        return Ok(None);
    };
    let close = open + close_rel + 1;
    let wait: TaskWait = serde_json::from_str(&suffix[open..close])
        .map_err(|error| TaskReportError(error.to_string()))?;
    if wait.task_id.trim().is_empty()
        || !(60..=604_800).contains(&wait.wait_seconds)
        || wait.reason.trim().is_empty()
    {
        return Err(TaskReportError("invalid task wait marker".into()));
    }
    Ok(Some(wait))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_disappearing_is_not_completion() {
        let snapshot = PageSnapshot {
            stop_available: false,
            copy_available_on_last_assistant: false,
            ..Default::default()
        };
        assert!(!snapshot.is_terminal());
    }

    #[test]
    fn stable_last_turn_actions_are_terminal() {
        let snapshot = PageSnapshot {
            user_turns: 1,
            assistant_turns: 1,
            copy_available_on_last_assistant: true,
            response_actions_complete: true,
            response_action_turn_bound_to_last: true,
            ..Default::default()
        };
        assert!(snapshot.is_terminal());
    }

    #[test]
    fn canonical_conversation_url_rejects_transient_routes() {
        let transient = PageSnapshot {
            url: "https://chatgpt.com/".into(),
            ..Default::default()
        };
        assert_eq!(transient.canonical_conversation_url(), None);

        let durable = PageSnapshot {
            url: "https://chatgpt.com/c/abc-123?model=gpt".into(),
            ..Default::default()
        };
        assert_eq!(
            durable.canonical_conversation_url().as_deref(),
            Some("https://chatgpt.com/c/abc-123")
        );
    }

    #[test]
    fn approval_keeps_turn_in_flight() {
        let snapshot = PageSnapshot {
            waiting_for_approval: true,
            copy_available_on_last_assistant: true,
            response_actions_complete: true,
            response_action_turn_bound_to_last: true,
            ..Default::default()
        };
        assert!(!snapshot.is_terminal());
    }

    #[test]
    fn approval_fingerprint_is_stable() {
        let a = ApprovalFingerprint::from_source(" approval-card-42 ");
        let b = ApprovalFingerprint::from_source("approval-card-42");
        assert_eq!(a, b);
    }

    #[test]
    fn execution_profile_is_fail_closed() {
        let requested = ExecutionProfile::default();
        let observed = ObservedExecutionProfile {
            model: Some("GPT-5.6 Sol".into()),
            thinking_effort: Some("High".into()),
        };
        assert!(!observed.satisfies(&requested));
    }

    #[test]
    fn recovery_envelope_carries_progress_messages() {
        let envelope = RecoveryEnvelope {
            version: 1,
            task_id: "task-1".into(),
            run_id: "run-1".into(),
            exact_commit: Some("abc".into()),
            conversation_url: Some("https://chatgpt.com/c/abc".into()),
            original_goal: "完成所有".into(),
            acceptance_prompt: Some("下一轮先读 exact HEAD".into()),
            progress_messages: vec!["正在检查 CI".into(), "已修复第一个根因".into()],
            completed: vec!["架构 gate".into()],
            remaining: vec!["真实 E2E".into()],
            blockers: vec![],
            continuation_instruction: "继续完成剩余工作".into(),
            ..Default::default()
        };
        let rendered = envelope.render_prompt();
        assert!(rendered.contains("正在检查 CI"));
        assert!(rendered.contains("下一轮先读 exact HEAD"));
        assert!(rendered.contains("继续完成剩余工作"));
    }

    #[test]
    fn parses_source_task_report_protocol_and_only_terminal_switch() {
        let text = r#"
MAHAYANA_TASK_REPORT_V1_BEGIN
```json
{
  "protocol":"mahayana.task-report.v1",
  "task_id":"task-1",
  "applied_task_revision":2,
  "applied_spec_digest":"sha256:abc",
  "status":"complete",
  "all_tasks_complete":true,
  "summary":"done",
  "completed":["all"],
  "remaining":[],
  "blockers":[],
  "verification":["ci"],
  "next_task":"",
  "wait_seconds":0
}
```
MAHAYANA_TASK_REPORT_V1_END
"#;
        let report = parse_task_report(text).unwrap().unwrap();
        assert!(report.all_tasks_complete);
        assert_eq!(report.status, TaskReportStatus::Complete);
    }

    #[test]
    fn rejects_false_complete_report() {
        let text = r#"
MAHAYANA_TASK_REPORT_V1_BEGIN
{"protocol":"mahayana.task-report.v1","task_id":"task-1","applied_task_revision":1,"applied_spec_digest":"","status":"complete","all_tasks_complete":false,"summary":"","completed":[],"remaining":["x"],"blockers":[],"verification":[],"next_task":"continue"}
MAHAYANA_TASK_REPORT_V1_END
"#;
        assert!(parse_task_report(text).is_err());
    }
}
