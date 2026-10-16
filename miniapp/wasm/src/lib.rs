//! Web-standard adapter over the pure `fabushi-chatgpt-domain` crate.
//! No CDP sockets, browser automation, SQLite files, or OS privileges are exposed.
use fabushi_chatgpt_domain::{
    PageSnapshot, Phase, ReviewReport, Task, TaskMode, bounded, text_hash,
};
use serde::Serialize;
use wasm_bindgen::prelude::*;

const MAX_TASKS: u32 = 64;
const MAX_GOAL_BYTES: usize = 16 * 1024;
const MAX_TEXT_BYTES: usize = 64 * 1024;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PromptEnvelope {
    task_id: String,
    phase: Phase,
    round: u32,
    marker: String,
    prompt: String,
    prompt_hash: String,
    state_label: String,
    effort_label: String,
}

struct PromptInput<'a> {
    task_id: &'a str,
    title: &'a str,
    goal: &'a str,
    mode: &'a str,
    phase: &'a str,
    round: u32,
    token: &'a str,
    next_action: &'a str,
    work_result: &'a str,
    carry: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VerifiedReviewEnvelope {
    task_id: String,
    round: u32,
    status: String,
    summary: String,
    next: String,
    report_hash: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotEvaluation {
    is_terminal: bool,
    terminal_evidence: bool,
    response_in_flight: bool,
    canonical_url: Option<String>,
    activity_fingerprint: String,
}

#[wasm_bindgen]
pub fn max_tasks() -> u32 {
    MAX_TASKS
}

fn build_prompt_inner(input: PromptInput<'_>) -> Result<String, String> {
    if input.task_id.trim().is_empty()
        || input.task_id.len() > 64
        || input.title.trim().is_empty()
        || input.title.len() > 256
        || input.goal.trim().is_empty()
        || input.goal.len() > MAX_GOAL_BYTES
        || input.token.trim().is_empty()
        || input.token.len() > 128
        || input.round == 0
        || input.next_action.len() > MAX_GOAL_BYTES
        || input.work_result.len() > MAX_TEXT_BYTES
        || input.carry.len() > MAX_TEXT_BYTES
    {
        return Err("invalid bounded task input".into());
    }
    let task_mode = match input.mode {
        "once" => TaskMode::Once,
        "goal" => TaskMode::Goal,
        _ => return Err("unsupported task mode".into()),
    };
    let task_phase = match input.phase {
        "work" => Phase::Work,
        "review" => Phase::Review,
        _ => return Err("unsupported task phase".into()),
    };
    let mut task = Task::new(
        input.task_id.trim().into(),
        input.title.trim().into(),
        input.goal.trim().into(),
        task_mode,
        0,
    );
    task.phase = task_phase;
    task.round = input.round;
    task.token = input.token.trim().into();
    task.next = bounded(input.next_action.trim(), 4000);
    task.work_result = bounded(input.work_result.trim(), 8000);
    task.carry = bounded(input.carry.trim(), 4000);
    let prompt = task.prompt();
    let envelope = PromptEnvelope {
        task_id: task.id.clone(),
        phase: task.phase,
        round: task.round,
        marker: task.marker(),
        prompt_hash: text_hash(&prompt),
        prompt,
        state_label: task.state.label().into(),
        effort_label: task.profile.effort_label().into(),
    };
    serde_json::to_string(&envelope).map_err(|error| error.to_string())
}

#[wasm_bindgen]
#[allow(clippy::too_many_arguments)]
pub fn build_task_prompt(
    task_id: &str,
    title: &str,
    goal: &str,
    mode: &str,
    phase: &str,
    round: u32,
    token: &str,
    next_action: &str,
    work_result: &str,
    carry: &str,
) -> Result<String, JsValue> {
    build_prompt_inner(PromptInput {
        task_id,
        title,
        goal,
        mode,
        phase,
        round,
        token,
        next_action,
        work_result,
        carry,
    })
    .map_err(|error| JsValue::from_str(&error))
}

fn verify_review_inner(
    raw_text: &str,
    task_id: &str,
    round: u32,
    goal: &str,
) -> Result<String, String> {
    if raw_text.is_empty()
        || raw_text.len() > MAX_TEXT_BYTES
        || task_id.trim().is_empty()
        || task_id.len() > 64
        || round == 0
        || goal.trim().is_empty()
        || goal.len() > MAX_GOAL_BYTES
    {
        return Err("invalid bounded review input".into());
    }
    let mut task = Task::new(
        task_id.trim().into(),
        "review".into(),
        goal.trim().into(),
        TaskMode::Goal,
        0,
    );
    task.round = round;
    let parsed = ReviewReport::parse(raw_text, &task)?;
    let envelope = VerifiedReviewEnvelope {
        task_id: parsed.task_id,
        round: parsed.round,
        status: parsed.status,
        summary: parsed.summary,
        next: parsed.next,
        report_hash: text_hash(raw_text),
    };
    serde_json::to_string(&envelope).map_err(|error| error.to_string())
}

#[wasm_bindgen]
pub fn verify_review_report(
    raw_text: &str,
    task_id: &str,
    round: u32,
    goal: &str,
) -> Result<String, JsValue> {
    verify_review_inner(raw_text, task_id, round, goal).map_err(|error| JsValue::from_str(&error))
}

fn evaluate_snapshot_inner(snapshot_json: &str) -> Result<String, String> {
    if snapshot_json.is_empty() || snapshot_json.len() > MAX_TEXT_BYTES {
        return Err("invalid bounded snapshot input".into());
    }
    let snapshot: PageSnapshot =
        serde_json::from_str(snapshot_json).map_err(|error| error.to_string())?;
    let evaluation = SnapshotEvaluation {
        is_terminal: snapshot.is_terminal(),
        terminal_evidence: snapshot.terminal_evidence(),
        response_in_flight: snapshot.response_in_flight(),
        canonical_url: snapshot.canonical_conversation_url(),
        activity_fingerprint: snapshot.activity_fingerprint(),
    };
    serde_json::to_string(&evaluation).map_err(|error| error.to_string())
}

#[wasm_bindgen]
pub fn evaluate_snapshot(snapshot_json: &str) -> Result<String, JsValue> {
    evaluate_snapshot_inner(snapshot_json).map_err(|error| JsValue::from_str(&error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_work_and_review_prompts_with_marker() {
        let work_raw = build_prompt_inner(PromptInput {
            task_id: "task-1",
            title: "完善模块",
            goal: "实现离线小程序上架",
            mode: "goal",
            phase: "work",
            round: 1,
            token: "tok-001",
            next_action: "",
            work_result: "",
            carry: "",
        })
        .expect("work prompt");
        assert!(work_raw.contains("[Fabushi:tok-001]"));
        let review_raw = build_prompt_inner(PromptInput {
            task_id: "task-1",
            title: "完善模块",
            goal: "实现离线小程序上架",
            mode: "goal",
            phase: "review",
            round: 2,
            token: "tok-002",
            next_action: "",
            work_result: "已完成第一轮",
            carry: "",
        })
        .expect("review prompt");
        assert!(review_raw.contains("MAHAYANA_TASK_REPORT_V1"));
        assert!(review_raw.contains("[Fabushi:tok-002]"));
    }

    #[test]
    fn verifies_bound_review_report_and_rejects_mismatch() {
        let ok = verify_review_inner(
            r#"{"taskId":"task-1","round":2,"status":"next","summary":"还缺签名","next":"执行签名与扫描"}"#,
            "task-1",
            2,
            "实现离线小程序上架",
        )
        .expect("valid review");
        assert!(ok.contains("\"status\":\"next\""));
        assert!(
            verify_review_inner(
                r#"{"taskId":"other","round":2,"status":"complete","summary":"ok","next":""}"#,
                "task-1",
                2,
                "实现离线小程序上架",
            )
            .is_err()
        );
    }

    #[test]
    fn evaluates_terminal_snapshot_and_canonical_url() {
        let terminal = evaluate_snapshot_inner(
            r#"{"url":"https://chatgpt.com/c/conv-123?model=gpt","title":"Chat","user_turns":1,"assistant_turns":1,"stop_available":false,"waiting_for_approval":false,"rate_limit_notice":false,"copy_available_on_last_assistant":true,"response_actions_complete":true,"response_action_turn_bound_to_last":true,"awaiting_assistant":false,"assistant_text":"done","composer_text":""}"#,
        )
        .expect("snapshot evaluation");
        assert!(terminal.contains("\"isTerminal\":true"));
        assert!(terminal.contains("https://chatgpt.com/c/conv-123"));
    }
}
