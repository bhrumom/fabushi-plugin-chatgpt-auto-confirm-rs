use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PageSnapshot {
    pub url: String,
    pub title: String,
    pub user_turns: usize,
    pub assistant_turns: usize,
    pub stop_available: bool,
    pub waiting_for_approval: bool,
    pub copy_available_on_last_assistant: bool,
    pub response_actions_complete: bool,
    pub response_action_turn_bound_to_last: bool,
    pub awaiting_assistant: bool,
    pub assistant_text: String,
    pub composer_text: String,
}

impl PageSnapshot {
    pub fn terminal_evidence(&self) -> bool {
        self.response_actions_complete
            && self.response_action_turn_bound_to_last
            && !self.awaiting_assistant
            && self.copy_available_on_last_assistant
    }

    pub fn response_in_flight(&self) -> bool {
        self.stop_available || self.waiting_for_approval || self.awaiting_assistant
    }

    pub fn is_terminal(&self) -> bool {
        !self.response_in_flight() && self.terminal_evidence()
    }

    pub fn activity_fingerprint(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.url.as_bytes());
        hasher.update(self.user_turns.to_le_bytes());
        hasher.update(self.assistant_turns.to_le_bytes());
        hasher.update(self.assistant_text.as_bytes());
        format!("{:x}", hasher.finalize())
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunReport {
    pub state: RunState,
    pub conversation_url: Option<String>,
    pub assistant_text: String,
    pub approvals_clicked: u32,
    pub recoveries: u32,
    pub message: String,
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
}
