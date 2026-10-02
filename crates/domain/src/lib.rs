use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

macro_rules! opaque_id {
    ($name:ident) => {
        #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

opaque_id!(TaskId);
opaque_id!(RunId);
opaque_id!(DispatchId);
opaque_id!(ConversationRef);
opaque_id!(ConversationFingerprint);
opaque_id!(UserTurnBoundary);
opaque_id!(AssistantResponseBoundary);
opaque_id!(DraftFingerprint);
opaque_id!(ProgressFingerprint);
opaque_id!(SurfaceGeneration);
opaque_id!(AttachmentId);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Work,
    Review,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(transparent)]
pub struct Round(u32);

impl Round {
    pub fn new(value: u32) -> Self {
        Self(value)
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(transparent)]
pub struct GoalRevision(u64);

impl GoalRevision {
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningPreset {
    Instant,
    Medium,
    High,
    #[default]
    ExtraHigh,
    Pro,
}

impl ReasoningPreset {
    pub fn index(self) -> u8 {
        match self {
            Self::Instant => 0,
            Self::Medium => 1,
            Self::High => 2,
            Self::ExtraHigh => 3,
            Self::Pro => 4,
        }
    }

    pub fn from_index(index: u8) -> Option<Self> {
        Some(match index {
            0 => Self::Instant,
            1 => Self::Medium,
            2 => Self::High,
            3 => Self::ExtraHigh,
            4 => Self::Pro,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum OwnershipConfidence {
    #[default]
    None,
    Weak,
    Strong,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthorizationSettlementState {
    #[default]
    Inactive,
    Settling,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum HydrationState {
    #[default]
    Ready,
    Loading,
    ShellOnly,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct ChatSurfaceSnapshot {
    pub app_healthy: bool,
    pub composer_ready: bool,
    pub draft_fingerprint: Option<DraftFingerprint>,
    pub user_turn_boundary: Option<UserTurnBoundary>,
    pub current_dispatch_id: Option<DispatchId>,
    pub user_turn_ownership: OwnershipConfidence,
    pub assistant_response_boundary: Option<AssistantResponseBoundary>,
    pub assistant_response_ownership: OwnershipConfidence,
    pub conversation_ref: Option<ConversationRef>,
    pub conversation_fingerprint: Option<ConversationFingerprint>,
    pub surface_generation: Option<SurfaceGeneration>,
    pub assistant_visible_prose: String,
    pub assistant_visible_work_trace: Vec<String>,
    pub streaming_or_busy: bool,
    pub stop_available: bool,
    pub authorization_surface_present: bool,
    pub authorization_actionable: bool,
    pub authorization_settlement: AuthorizationSettlementState,
    pub response_local_copy: bool,
    pub strict_review_report: Option<StrictReviewReportEvidence>,
    pub rate_limit: bool,
    pub retryable_error: bool,
    pub unable_to_load_conversation: bool,
    pub connection_interrupted: bool,
    pub conversation_length_limit: bool,
    pub stream_polling_timeout: bool,
    pub stream_cache_expired: bool,
    pub hydration: HydrationState,
    pub reasoning_picker_available: bool,
    pub selected_reasoning_preset: Option<ReasoningPreset>,
    pub attachment_ready: bool,
    pub harmless_popup_present: bool,
    pub sensitive_or_unknown_popup_present: bool,
    pub blocker_or_modal: bool,
    pub progress_fingerprint: Option<ProgressFingerprint>,
}

impl ChatSurfaceSnapshot {
    pub fn ordinary_terminal_evidence(&self) -> bool {
        self.app_healthy
            && self.assistant_response_boundary.is_some()
            && self.assistant_response_ownership == OwnershipConfidence::Strong
            && !self.streaming_or_busy
            && !self.stop_available
            && !self.authorization_surface_present
            && self.authorization_settlement == AuthorizationSettlementState::Inactive
            && !self.rate_limit
            && !self.retryable_error
            && !self.unable_to_load_conversation
            && !self.connection_interrupted
            && !self.conversation_length_limit
            && !self.stream_polling_timeout
            && !self.stream_cache_expired
            && !self.blocker_or_modal
            && self.response_local_copy
    }

    pub fn activity_fingerprint(&self) -> String {
        let mut h = Sha256::new();
        if let Some(v) = &self.conversation_fingerprint {
            h.update(v.as_str().as_bytes());
        }
        if let Some(v) = &self.user_turn_boundary {
            h.update(v.as_str().as_bytes());
        }
        if let Some(v) = &self.assistant_response_boundary {
            h.update(v.as_str().as_bytes());
        }
        h.update(self.assistant_visible_prose.as_bytes());
        for v in &self.assistant_visible_work_trace {
            h.update(v.as_bytes());
        }
        format!("{:x}", h.finalize())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApprovalSettlementKey {
    pub task_id: TaskId,
    pub run_id: RunId,
    pub phase: Phase,
    pub round: Round,
    pub conversation_fingerprint: ConversationFingerprint,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    Complete,
    Next,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StrictReviewReportEvidence {
    pub task_id: TaskId,
    pub round: Round,
    pub status: ReviewStatus,
    pub summary: String,
    pub next: Option<String>,
    pub response_boundary: AssistantResponseBoundary,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecoveryEnvelopeV1 {
    pub task_id: TaskId,
    pub run_id: RunId,
    pub phase: Phase,
    pub round: Round,
    pub goal_revision: GoalRevision,
    pub authoritative_instruction: String,
    pub visible_assistant_prose: String,
    pub visible_work_trace: Vec<String>,
    pub previous_work_result: Option<String>,
    pub current_next: Option<String>,
    pub original_goal: String,
    pub completed: Vec<String>,
    pub remaining: Vec<String>,
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "version", content = "payload")]
pub enum RecoveryEnvelope {
    #[serde(rename = "1")]
    V1(RecoveryEnvelopeV1),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PreparedDispatch {
    pub task_id: TaskId,
    pub run_id: RunId,
    pub dispatch_id: DispatchId,
    pub phase: Phase,
    pub round: Round,
    pub goal_revision: GoalRevision,
    pub reasoning_preset: ReasoningPreset,
    pub instruction: String,
    pub fabushi_marker: String,
    pub attachments: Vec<AttachmentId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Dispatching,
    Running,
    WaitingApproval,
    Recovering,
    CoolingDown,
    Complete,
    Paused,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunReport {
    pub state: RunState,
    pub conversation_ref: Option<ConversationRef>,
    pub assistant_text: String,
    pub approvals_clicked: u32,
    pub recoveries: u32,
    pub rate_limit_pauses: u32,
    pub dispatch_retries: u32,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn stop_absent_alone_is_not_terminal() {
        assert!(
            !ChatSurfaceSnapshot {
                app_healthy: true,
                ..Default::default()
            }
            .ordinary_terminal_evidence()
        );
    }

    #[test]
    fn disabled_authorization_still_blocks_terminal() {
        let mut s = terminal();
        s.authorization_surface_present = true;
        s.authorization_actionable = false;
        assert!(!s.ordinary_terminal_evidence());
    }

    #[test]
    fn settlement_blocks_terminal() {
        let mut s = terminal();
        s.authorization_settlement = AuthorizationSettlementState::Settling;
        assert!(!s.ordinary_terminal_evidence());
    }

    #[test]
    fn visible_work_changes_progress() {
        let mut s = terminal();
        let a = s.activity_fingerprint();
        s.assistant_visible_work_trace.push("checking".into());
        assert_ne!(a, s.activity_fingerprint());
    }

    #[test]
    fn reasoning_positions_are_exact() {
        for i in 0..=4 {
            assert_eq!(ReasoningPreset::from_index(i).unwrap().index(), i);
        }
        assert_eq!(ReasoningPreset::default(), ReasoningPreset::ExtraHigh);
    }
}
