use super::*;
use crate::{Clock, Supervisor, SupervisorPolicy, TaskDesktop, TaskStore};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
struct Store {
    tasks: Mutex<Vec<Task>>,
    settings: Mutex<Settings>,
    events: Mutex<Vec<Activity>>,
}
impl Store {
    fn new(t: Task) -> Self {
        Self {
            tasks: Mutex::new(vec![t]),
            settings: Mutex::new(Settings::default()),
            events: Mutex::new(vec![]),
        }
    }
}
impl TaskStore for Store {
    fn list(&self) -> Result<Vec<Task>> {
        Ok(self.tasks.lock().unwrap().clone())
    }
    fn insert(&self, t: &Task) -> Result<()> {
        self.tasks.lock().unwrap().push(t.clone());
        Ok(())
    }
    fn save(&self, t: &mut Task, expected: u64, a: Option<Activity>) -> Result<bool> {
        let mut tasks = self.tasks.lock().unwrap();
        let Some(live) = tasks
            .iter_mut()
            .find(|x| x.id == t.id && x.revision == expected)
        else {
            return Ok(false);
        };
        t.revision += 1;
        *live = t.clone();
        if let Some(a) = a {
            self.events.lock().unwrap().push(a);
        }
        Ok(true)
    }
    fn delete(&self, _id: &str) -> Result<()> {
        Ok(())
    }
    fn activity(&self, _id: &str, _since: u64, _limit: usize) -> Result<Vec<Activity>> {
        Ok(self.events.lock().unwrap().clone())
    }
    fn settings(&self) -> Result<Settings> {
        Ok(self.settings.lock().unwrap().clone())
    }
    fn save_settings(&self, s: &mut Settings) -> Result<bool> {
        let mut current = self.settings.lock().unwrap();
        if current.revision != s.revision {
            return Ok(false);
        }
        s.revision += 1;
        *current = s.clone();
        Ok(true)
    }
    fn lease(&self, _endpoint: &str, _owner: &str, _now: u64, _ttl: u64) -> Result<bool> {
        Ok(true)
    }
    fn release(&self, _endpoint: &str, _owner: &str) -> Result<()> {
        Ok(())
    }
    fn heartbeat(&self, _endpoint: &str) -> Result<Option<(String, u64)>> {
        Ok(None)
    }
}
struct Time(AtomicU64);
#[async_trait]
impl Clock for Time {
    fn now(&self) -> Duration {
        Duration::from_millis(self.0.load(Ordering::SeqCst))
    }
    async fn sleep(&self, d: Duration) {
        self.0.fetch_add(d.as_millis() as u64, Ordering::SeqCst);
    }
}
struct Desktop {
    observation: Mutex<TaskObservation>,
    dispatches: Mutex<Vec<String>>,
    store: Arc<Store>,
    pause_preflight: bool,
    fail_dispatch: bool,
    stay_success: bool,
    new_chats: AtomicU64,
}
#[async_trait]
impl TaskDesktop for Desktop {
    async fn new_chat(&self) -> Result<()> {
        self.new_chats.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn activate(&self, _b: &SessionBinding, _token: &str) -> Result<()> {
        Ok(())
    }
    async fn observe(&self, _token: &str) -> Result<TaskObservation> {
        Ok(self.observation.lock().unwrap().clone())
    }
    async fn ensure_profile(&self, _p: &ExecutionProfile) -> Result<()> {
        if self.pause_preflight {
            let mut t = self.store.list()?[0].clone();
            let rev = t.revision;
            t.pause();
            self.store.save(&mut t, rev, None)?;
        }
        Ok(())
    }
    async fn upload(&self, _f: &[Attachment]) -> Result<()> {
        Ok(())
    }
    async fn dispatch(&self, p: &str) -> Result<()> {
        self.dispatches.lock().unwrap().push(p.into());
        if self.fail_dispatch {
            bail!("transport closed after Send")
        }
        Ok(())
    }
    async fn approve(&self) -> Result<bool> {
        Ok(true)
    }
    async fn stay_chat(&self) -> Result<bool> {
        self.dispatches.lock().unwrap().push("STAY_CHAT".into());
        Ok(self.stay_success)
    }
    async fn retry_load(&self) -> Result<bool> {
        self.dispatches.lock().unwrap().push("RETRY_LOAD".into());
        Ok(true)
    }
    async fn reload(&self) -> Result<()> {
        Ok(())
    }
}
fn task() -> Task {
    Task::new(
        "task".into(),
        "goal".into(),
        "complete full migration".into(),
        TaskMode::Goal,
        1,
    )
}
fn bound_task() -> Task {
    let mut t = task();
    t.send_clicked = true;
    t.dispatch_goal_revision = 1;
    t.token = "owned".into();
    t.binding = Some(binding());
    t.state = TaskState::Waiting;
    t
}
fn binding() -> SessionBinding {
    SessionBinding {
        identity: "native-a".into(),
        sidebar_id: "sidebar-a".into(),
        title: "task".into(),
    }
}
fn terminal(reply: &str) -> TaskObservation {
    TaskObservation {
        page: PageSnapshot {
            user_turns: 1,
            assistant_turns: 1,
            copy_available_on_last_assistant: true,
            response_actions_complete: true,
            response_action_turn_bound_to_last: true,
            assistant_text: reply.into(),
            ..Default::default()
        },
        binding: binding(),
        marker_present: true,
        ..Default::default()
    }
}
fn setup(t: Task, o: TaskObservation, pause: bool, fail: bool) -> (Arc<Store>, Desktop, Time) {
    let store = Arc::new(Store::new(t));
    let desktop = Desktop {
        observation: Mutex::new(o),
        dispatches: Mutex::new(vec![]),
        store: store.clone(),
        pause_preflight: pause,
        fail_dispatch: fail,
        stay_success: true,
        new_chats: AtomicU64::new(0),
    };
    (store, desktop, Time(AtomicU64::new(1_000_000)))
}
async fn tick(s: &Store, d: &Desktop, c: &Time) {
    Supervisor {
        store: s,
        desktop: d,
        clock: c,
        policy: SupervisorPolicy::default(),
    }
    .tick()
    .await
    .unwrap();
}
#[tokio::test]
async fn user_pause_during_preflight_prevents_send() {
    let (s, d, c) = setup(task(), TaskObservation::default(), true, false);
    tick(&s, &d, &c).await;
    assert!(d.dispatches.lock().unwrap().is_empty());
    assert_eq!(s.list().unwrap()[0].state, TaskState::Paused);
}
#[tokio::test]
async fn possible_send_survives_error_without_duplicate_dispatch() {
    let (s, d, c) = setup(task(), TaskObservation::default(), false, true);
    tick(&s, &d, &c).await;
    assert_eq!(d.dispatches.lock().unwrap().len(), 1);
    assert!(s.list().unwrap()[0].send_clicked);
    *d.observation.lock().unwrap() = terminal("progress");
    c.0.fetch_add(60_000, Ordering::SeqCst);
    tick(&s, &d, &c).await;
    assert_eq!(d.dispatches.lock().unwrap().len(), 1);
    assert_eq!(
        s.list().unwrap()[0].binding.as_ref().unwrap().identity,
        "native-a"
    );
}
#[tokio::test]
async fn terminal_work_queues_fresh_independent_review_instead_of_completing_goal() {
    let (s, d, c) = setup(
        bound_task(),
        terminal("all done according to me"),
        false,
        false,
    );
    tick(&s, &d, &c).await;
    c.0.fetch_add(15_000, Ordering::SeqCst);
    tick(&s, &d, &c).await;
    let t = s.list().unwrap().remove(0);
    assert_eq!(t.phase, Phase::Review);
    assert_eq!(t.state, TaskState::Queued);
    assert!(t.work_result.contains("all done"));
    assert!(!t.send_clicked);
}
#[tokio::test]
async fn foreign_reply_cannot_finish_or_replace_owned_preview() {
    let mut o = terminal("another task done");
    o.foreign_marker = true;
    let (s, d, c) = setup(bound_task(), o, false, false);
    tick(&s, &d, &c).await;
    let t = s.list().unwrap().remove(0);
    assert_eq!(t.phase, Phase::Work);
    assert!(t.latest_reply.is_empty());
    assert!(t.last_error.contains("另一个任务"));
}
#[tokio::test]
async fn authorization_presence_preempts_rate_limit_and_fresh_handoff() {
    let mut o = terminal("partial");
    o.page.rate_limit_notice = true;
    o.interrupted = true;
    o.approval_present = true;
    o.approval_actionable = false;
    let (s, d, c) = setup(bound_task(), o, false, false);
    tick(&s, &d, &c).await;
    let t = s.list().unwrap().remove(0);
    assert_eq!(t.state, TaskState::Approval);
    assert_eq!(t.rate_limit_episodes, 0);
    assert!(t.binding.is_some());
}
#[tokio::test]
async fn invalid_review_repairs_review_only_and_preserves_work_result() {
    let mut t = bound_task();
    t.phase = Phase::Review;
    t.work_result = "verified implementation trace".into();
    let (s, d, c) = setup(
        t,
        terminal(r#"{"taskId":"foreign","round":1,"status":"complete","summary":"yes","next":""}"#),
        false,
        false,
    );
    tick(&s, &d, &c).await;
    c.0.fetch_add(15_000, Ordering::SeqCst);
    tick(&s, &d, &c).await;
    let t = s.list().unwrap().remove(0);
    assert_eq!(t.phase, Phase::Review);
    assert_eq!(t.round, 1);
    assert_eq!(t.review_repairs, 1);
    assert!(t.work_result.contains("implementation"));
}
#[tokio::test]
async fn correctly_bound_review_alone_can_complete_goal() {
    let mut t = bound_task();
    t.phase = Phase::Review;
    let (s, d, c) = setup(
        t,
        terminal(
            r#"{"taskId":"task","round":1,"status":"complete","summary":"current HEAD and packaged evidence verified","next":""}"#,
        ),
        false,
        false,
    );
    tick(&s, &d, &c).await;
    c.0.fetch_add(15_000, Ordering::SeqCst);
    tick(&s, &d, &c).await;
    assert_eq!(s.list().unwrap()[0].state, TaskState::Done);
}

#[tokio::test]
async fn interrupted_turn_with_copy_actions_recovers_before_review_and_carries_context() {
    let mut t = bound_task();
    t.carry = "previous checkpoint exact HEAD abc".into();
    let mut o = terminal("partial implementation");
    o.interrupted = true;
    o.visible_work = "GitHub file update succeeded; CI pending; connection interrupted".into();
    let (s, d, c) = setup(t, o, false, false);
    tick(&s, &d, &c).await;
    assert!(s.list().unwrap()[0].binding.is_some());
    c.0.fetch_add(16_000, Ordering::SeqCst);
    tick(&s, &d, &c).await;
    let saved = s.list().unwrap()[0].clone();
    assert!(saved.binding.is_none());
    assert_eq!(saved.phase, Phase::Work);
    assert_eq!(saved.state, TaskState::Queued);
    assert!(saved.carry.contains("GitHub file update succeeded"));
    assert!(saved.carry.contains("previous checkpoint exact HEAD abc"));
    assert!(saved.carry.contains("sidebar-a"));
    assert!(saved.prompt().contains("complete full migration"));
    assert!(saved.prompt().contains("CI pending"));
    // Fresh dispatch remains paced, with the old send intent cleared.
    assert!(!saved.send_clicked);
    assert!(saved.cooldown_until > c.0.load(Ordering::SeqCst));
}

#[tokio::test]
async fn interruption_with_empty_render_preserves_last_checkpoint() {
    let mut t = bound_task();
    t.latest_reply = "last known commit and pending CI".into();
    let o = TaskObservation {
        interrupted: true,
        binding: binding(),
        marker_present: true,
        ..Default::default()
    };
    let (s, d, c) = setup(t, o, false, false);
    tick(&s, &d, &c).await;
    c.0.fetch_add(16_000, Ordering::SeqCst);
    tick(&s, &d, &c).await;
    assert!(
        s.list().unwrap()[0]
            .carry
            .contains("last known commit and pending CI")
    );
}

#[tokio::test]
async fn load_failure_retries_same_conversation_at_intervals_and_preserves_context() {
    let mut t = bound_task();
    t.latest_reply = "checkpoint".into();
    let o = TaskObservation {
        load_failed: true,
        binding: binding(),
        ..Default::default()
    };
    let (s, d, c) = setup(t, o, false, false);
    tick(&s, &d, &c).await;
    assert_eq!(d.dispatches.lock().unwrap().len(), 1);
    c.0.fetch_add(16_000, Ordering::SeqCst);
    tick(&s, &d, &c).await;
    assert_eq!(d.dispatches.lock().unwrap().len(), 1);
    c.0.fetch_add(16_000, Ordering::SeqCst);
    tick(&s, &d, &c).await;
    assert_eq!(d.dispatches.lock().unwrap().len(), 2);
    let t = s.list().unwrap()[0].clone();
    assert_eq!(t.binding.as_ref().unwrap().sidebar_id, binding().sidebar_id);
    assert_eq!(t.latest_reply, "checkpoint");
    assert_eq!(t.phase, Phase::Work);
}

#[tokio::test]
async fn unconfirmed_load_failure_never_requeues_or_discards_possible_send() {
    let mut t = bound_task();
    t.binding = None;
    t.dispatch_at = 1;
    t.prepared_prompt = "original prompt".into();
    t.latest_reply = "saved checkpoint".into();
    let o = TaskObservation {
        load_failed: true,
        binding: binding(),
        ..Default::default()
    };
    let (s, d, c) = setup(t, o, false, false);
    tick(&s, &d, &c).await;
    assert!(s.list().unwrap()[0].load_failure_pending);
    c.0.fetch_add(120_000, Ordering::SeqCst);
    *d.observation.lock().unwrap() = TaskObservation {
        empty_final: true,
        interrupted: true,
        binding: binding(),
        ..Default::default()
    };
    tick(&s, &d, &c).await;
    let t = s.list().unwrap()[0].clone();
    assert!(t.send_clicked);
    assert_eq!(t.token, "owned");
    assert_eq!(t.round, 1);
    assert_eq!(t.prepared_prompt, "original prompt");
    assert_eq!(t.latest_reply, "saved checkpoint");
    assert!(t.load_failure_pending);
    assert!(t.carry.is_empty());
    assert!(
        d.dispatches
            .lock()
            .unwrap()
            .iter()
            .all(|x| x == "RETRY_LOAD")
    );
}
#[tokio::test]
async fn unknown_send_after_confirmation_timeout_waits_without_duplicate_dispatch() {
    let mut t = bound_task();
    t.binding = None;
    t.dispatch_at = 1;
    let (s, d, c) = setup(t, TaskObservation::default(), false, false);
    tick(&s, &d, &c).await;
    let t = s.list().unwrap()[0].clone();
    assert!(t.send_clicked);
    assert_eq!(t.token, "owned");
    assert_eq!(t.state, TaskState::Sending);
    assert!(d.dispatches.lock().unwrap().is_empty());
}
#[tokio::test]
async fn work_offer_click_is_recorded_only_after_success_and_blocks_terminal_review() {
    let mut o = terminal("partial reply asking to continue in Work");
    o.chat_work_offer = true;
    let (s, mut d, c) = setup(bound_task(), o, false, false);
    d.stay_success = false;
    tick(&s, &d, &c).await;
    assert!(
        s.events
            .lock()
            .unwrap()
            .iter()
            .all(|a| !a.text.contains("点击 Work 转换"))
    );
    c.0.fetch_add(16_000, Ordering::SeqCst);
    d.stay_success = true;
    tick(&s, &d, &c).await;
    assert!(
        s.events
            .lock()
            .unwrap()
            .iter()
            .any(|a| a.role == "action" && a.text.contains("留在聊天模式"))
    );
    assert_eq!(s.list().unwrap()[0].phase, Phase::Work);
}

#[tokio::test]
async fn desktop_rotation_interval_is_global_not_per_task() {
    let (s, d, c) = setup(bound_task(), terminal("progress"), false, false);
    let mut other = bound_task();
    other.id = "other-owned-task".into();
    s.insert(&other).unwrap();
    {
        let mut settings = s.settings.lock().unwrap();
        settings.last_navigation_task = "task".into();
        settings.last_navigation_at = 1_000_000;
        s.tasks.lock().unwrap()[0].last_scan_at = 1_000_000;
    }
    c.0.fetch_add(5_000, Ordering::SeqCst);
    tick(&s, &d, &c).await;
    assert_eq!(s.list().unwrap()[1].last_scan_at, 0);
    c.0.fetch_add(11_000, Ordering::SeqCst);
    tick(&s, &d, &c).await;
    assert_eq!(s.list().unwrap()[1].last_scan_at, 1_016_000);
}

#[tokio::test]
async fn preflight_settles_work_offer_before_creating_or_sending_new_stage() {
    let o = TaskObservation {
        chat_work_offer: true,
        ..Default::default()
    };
    let (s, d, c) = setup(task(), o, false, false);
    tick(&s, &d, &c).await;
    let t = s.list().unwrap()[0].clone();
    assert!(!t.send_clicked);
    assert_eq!(d.new_chats.load(Ordering::SeqCst), 0);
    assert!(t.binding.is_none());
    assert_eq!(d.dispatches.lock().unwrap().as_slice(), ["STAY_CHAT"]);
    assert!(
        s.events
            .lock()
            .unwrap()
            .iter()
            .any(|a| a.role == "action" && a.text.contains("留在聊天模式"))
    );
}

#[tokio::test]
async fn review_stability_check_is_not_starved_by_other_task_approval() {
    let mut t = bound_task();
    t.phase = Phase::Review;
    t.terminal_polls = 1;
    t.terminal_since = 980_000;
    t.last_scan_at = 980_000;
    let reply = r#"{"taskId":"task","round":1,"status":"next","summary":"missing production parity","next":"implement the missing owner"}"#;
    let observation = terminal(reply);
    t.fingerprint = observation.page.activity_fingerprint();
    let (s, d, c) = setup(t, observation, false, false);
    let mut other = bound_task();
    other.id = "approval-task".into();
    other.state = TaskState::Approval;
    other.last_scan_at = 995_000;
    s.insert(&other).unwrap();
    tick(&s, &d, &c).await;
    let t = s
        .list()
        .unwrap()
        .into_iter()
        .find(|t| t.id == "task")
        .unwrap();
    assert_eq!(t.round, 2);
    assert_eq!(t.phase, Phase::Work);
    assert_eq!(t.next, "implement the missing owner");
}
#[tokio::test]
async fn approval_priority_is_bounded_for_overdue_other_tasks() {
    let mut t = bound_task();
    t.state = TaskState::Approval;
    t.last_scan_at = 995_000;
    let (s, d, c) = setup(t, terminal("progress"), false, false);
    let mut other = bound_task();
    other.id = "overdue-task".into();
    other.last_scan_at = 900_000;
    s.insert(&other).unwrap();
    tick(&s, &d, &c).await;
    assert_eq!(s.settings().unwrap().last_task, "overdue-task");
}
#[tokio::test]
async fn exact_marker_confirms_send_even_when_cached_turn_baseline_matches() {
    let mut t = bound_task();
    t.binding = None;
    t.baseline_users = 1;
    t.state = TaskState::Sending;
    let (s, d, c) = setup(t, terminal("progress"), false, false);
    tick(&s, &d, &c).await;
    assert_eq!(
        s.list().unwrap()[0].binding.as_ref().unwrap().sidebar_id,
        "sidebar-a"
    );
}
