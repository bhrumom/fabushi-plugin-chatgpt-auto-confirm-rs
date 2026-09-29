use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use fabushi_chatgpt_application::{
    Clock, OwnershipStore, RunPrompt, parse_task_report, parse_task_wait,
};
use fabushi_chatgpt_domain::{
    RecoveryEnvelope, RunEvent, RunEventKind, RunReport, RunState, TaskReportStatus,
};
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, Semaphore};

pub use fabushi_chatgpt_application::{QueueStore, RunOptions};
pub use fabushi_chatgpt_cdp::ChatGptCdp;
pub use fabushi_chatgpt_domain::{
    ExecutionProfile, ObservedExecutionProfile, QueueTask, RunRecord,
};
pub use fabushi_chatgpt_linux_browser::{BrowserLaunch, find_chromium_binary, launch_chromium};
pub use fabushi_chatgpt_sqlite_store::SqliteStore;

pub struct TokioClock {
    origin: Instant,
}

impl Default for TokioClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

#[async_trait]
impl Clock for TokioClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }

    fn unix_time_ms(&self) -> i64 {
        now_ms()
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

pub async fn run_prompt(
    browser: &ChatGptCdp,
    prompt: &str,
    options: RunOptions,
) -> Result<RunReport> {
    let clock = TokioClock::default();
    RunPrompt::new(browser, &clock)
        .execute(prompt, options)
        .await
}

#[derive(Debug, Clone)]
pub struct TargetLease {
    pub run_id: String,
    pub target_id: String,
    pub owner_id: String,
}

pub struct AccountBrowserActor {
    account_id: String,
    endpoint: String,
    store: SqliteStore,
    owner_id: String,
    lease_duration_ms: i64,
    target_mutation: Mutex<()>,
}

impl AccountBrowserActor {
    pub fn new(
        account_id: impl Into<String>,
        endpoint: impl Into<String>,
        store: SqliteStore,
        owner_id: impl Into<String>,
    ) -> Self {
        Self {
            account_id: account_id.into(),
            endpoint: endpoint.into(),
            store,
            owner_id: owner_id.into(),
            lease_duration_ms: 60_000,
            target_mutation: Mutex::new(()),
        }
    }

    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    fn acquire_browser_ownership(&self) -> Result<()> {
        let acquired = self.store.acquire_account_browser(
            &self.account_id,
            &self.owner_id,
            &format!("attached-cdp:{}", self.endpoint),
            &format!("user-owned-profile:{}", self.account_id),
            now_ms(),
            self.lease_duration_ms,
        )?;
        if !acquired {
            bail!(
                "account {} browser ownership is held by another live actor",
                self.account_id
            );
        }
        self.store.record_browser_lifecycle(
            &self.account_id,
            &self.owner_id,
            "attached",
            &serde_json::json!({"endpoint": self.endpoint}).to_string(),
            now_ms(),
        )?;
        Ok(())
    }

    fn renew_worker_ownership(&self, target_id: &str, worker_owner_id: &str) -> Result<bool> {
        let now = now_ms();
        let account_ok = self.store.renew_account_browser(
            &self.account_id,
            &self.owner_id,
            now,
            self.lease_duration_ms,
        )?;
        let target_ok =
            self.store
                .renew_target(target_id, worker_owner_id, now, self.lease_duration_ms)?;
        Ok(account_ok && target_ok)
    }

    fn release_target_ownership(&self, target_id: &str, worker_owner_id: &str) -> Result<()> {
        self.store.release_target(target_id, worker_owner_id)
    }

    pub async fn lease_target(
        &self,
        run_id: &str,
        worker_owner_id: &str,
        recovery_url: Option<&str>,
    ) -> Result<(TargetLease, ChatGptCdp)> {
        let _guard = self.target_mutation.lock().await;
        self.acquire_browser_ownership()?;
        let url = recovery_url.unwrap_or("https://chatgpt.com/");
        let browser = ChatGptCdp::create_target(&self.endpoint, url)
            .await
            .with_context(|| format!("account {} failed to create target", self.account_id))?;
        let target_id = browser.target_id().to_owned();
        let acquired = self.store.acquire_target(
            &target_id,
            run_id,
            &self.account_id,
            worker_owner_id,
            now_ms(),
            self.lease_duration_ms,
        )?;
        if !acquired {
            let _ = browser.close_owned_target().await;
            bail!("run {run_id} could not acquire durable ownership of target {target_id}");
        }
        if let Err(error) = self.store.record_browser_lifecycle(
            &self.account_id,
            &self.owner_id,
            "target_leased",
            &serde_json::json!({"run_id": run_id, "target_id": target_id}).to_string(),
            now_ms(),
        ) {
            let _ = self.store.release_target(&target_id, worker_owner_id);
            let _ = browser.close_owned_target().await;
            return Err(error);
        }
        let lease = TargetLease {
            run_id: run_id.into(),
            target_id,
            owner_id: worker_owner_id.into(),
        };
        Ok((lease, browser))
    }

    pub async fn recover_target(
        &self,
        lease: &TargetLease,
        canonical_url: &str,
    ) -> Result<ChatGptCdp> {
        if let Some(durable) = self.store.target_lease_for_run(&lease.run_id)?
            && durable.target_id == lease.target_id
            && durable.owner_id == lease.owner_id
            && durable.expires_at_ms > now_ms()
            && let Ok(browser) =
                ChatGptCdp::connect_target_id(&self.endpoint, &lease.target_id).await
        {
            browser.navigate(canonical_url).await?;
            return Ok(browser);
        }
        let _ = self.store.release_target(&lease.target_id, &lease.owner_id);
        let (_, browser) = self
            .lease_target(&lease.run_id, &lease.owner_id, Some(canonical_url))
            .await?;
        Ok(browser)
    }
}

impl Drop for AccountBrowserActor {
    fn drop(&mut self) {
        let _ = self.store.record_browser_lifecycle(
            &self.account_id,
            &self.owner_id,
            "actor_released",
            "{}",
            now_ms(),
        );
        let _ = self
            .store
            .release_account_browser(&self.account_id, &self.owner_id);
    }
}

pub struct RunWorker {
    owner_id: String,
    store: SqliteStore,
    account: Arc<AccountBrowserActor>,
    lease_duration_ms: i64,
}

impl RunWorker {
    pub fn new(
        owner_id: impl Into<String>,
        store: SqliteStore,
        account: Arc<AccountBrowserActor>,
    ) -> Self {
        Self {
            owner_id: owner_id.into(),
            store,
            account,
            lease_duration_ms: 60_000,
        }
    }

    pub async fn execute_claim(
        &self,
        mut task: QueueTask,
        run_id: String,
        recovery_url: Option<String>,
    ) -> Result<RunReport> {
        let recovery_url = recovery_url.or_else(|| {
            task.recovery_context
                .as_ref()
                .and_then(|envelope| envelope.conversation_url.clone())
        });
        let recovery_prompt = task
            .recovery_context
            .as_ref()
            .map(RecoveryEnvelope::render_prompt);
        let prompt = recovery_prompt.as_deref().unwrap_or(&task.prompt);
        let (target_lease, browser) = self
            .account
            .lease_target(&run_id, &self.owner_id, recovery_url.as_deref())
            .await?;

        let journal = self.store.journal(&run_id, &self.owner_id);
        let heartbeat_stop = Arc::new(AtomicBool::new(false));
        let heartbeat_flag = heartbeat_stop.clone();
        let heartbeat_store = self.store.clone();
        let heartbeat_account = self.account.clone();
        let heartbeat_target_id = target_lease.target_id.clone();
        let heartbeat_run_id = run_id.clone();
        let heartbeat_owner_id = self.owner_id.clone();
        let heartbeat_lease_ms = self.lease_duration_ms;
        let mut heartbeat = tokio::spawn(async move {
            while !heartbeat_flag.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_secs(20)).await;
                if heartbeat_flag.load(Ordering::Relaxed) {
                    break;
                }
                let worker_ok = heartbeat_store.renew_lease(
                    &heartbeat_run_id,
                    &heartbeat_owner_id,
                    now_ms(),
                    heartbeat_lease_ms,
                )?;
                let ownership_ok = heartbeat_account
                    .renew_worker_ownership(&heartbeat_target_id, &heartbeat_owner_id)?;
                if !worker_ok || !ownership_ok {
                    bail!("durable worker or target ownership lease was lost");
                }
            }
            Ok::<(), anyhow::Error>(())
        });
        let mut target_event = RunEvent::new(
            RunEventKind::TargetReattached,
            RunState::Dispatching,
            Default::default(),
        );
        target_event.target_id = Some(target_lease.target_id.clone());
        fabushi_chatgpt_application::RunJournal::record(&journal, &target_event)?;

        let options = RunOptions {
            timeout: Duration::from_secs(task.timeout_seconds),
            execution_profile: Some(task.execution_profile.clone()),
            ..RunOptions::default()
        };
        let clock = TokioClock::default();
        let runner = RunPrompt::with_journal(&browser, &clock, &journal);
        let run = runner.execute(prompt, options);
        tokio::pin!(run);
        let result = tokio::select! {
            result = &mut run => result,
            heartbeat_result = &mut heartbeat => {
                match heartbeat_result {
                    Ok(Ok(())) => Err(anyhow!("ownership heartbeat stopped unexpectedly")),
                    Ok(Err(error)) => Err(error),
                    Err(error) => Err(anyhow!("ownership heartbeat task failed: {error}")),
                }
            }
        };

        let report = match result {
            Ok(report) => report,
            Err(error) => {
                self.store.settle_task(
                    &task,
                    None,
                    task.recovery_context.as_ref(),
                    None,
                    Some(&error.to_string()),
                )?;
                heartbeat_stop.store(true, Ordering::Relaxed);
                if !heartbeat.is_finished() {
                    let _ = heartbeat.await;
                }
                let _ = self
                    .account
                    .release_target_ownership(&target_lease.target_id, &self.owner_id);
                self.store.release_lease(&run_id, &self.owner_id)?;
                let _ = browser.close_owned_target().await;
                return Err(error);
            }
        };

        task.recovery_context = None;
        if report.state == RunState::Complete {
            self.settle_terminal_response(&task, &run_id, &report)?;
        } else if report.state == RunState::Recovering {
            let current_run = self
                .store
                .snapshot()?
                .runs
                .into_iter()
                .find(|run| run.run_id == run_id);
            let current_checkpoint = current_run.as_ref().map(|run| run.checkpoint.clone());
            let recovery = RecoveryEnvelope {
                version: 2,
                task_id: task.id.clone(),
                run_id: run_id.clone(),
                exact_commit: task.known_exact_head.clone(),
                conversation_url: report.conversation_url.clone(),
                conversation_kind: task.conversation_kind.clone(),
                original_goal: task.original_prompt.clone(),
                acceptance_prompt: task.acceptance_prompt.clone(),
                interrupted_turn_visible_content: report.visible_progress_messages.clone(),
                progress_messages: report.visible_progress_messages.clone(),
                completed: task
                    .last_report
                    .as_ref()
                    .map(|previous| previous.completed.clone())
                    .unwrap_or_default(),
                remaining: if task.pending_work.is_empty() {
                    vec![report.message.clone()]
                } else {
                    task.pending_work.clone()
                },
                blockers: vec![report.message.clone()],
                known_ci_evidence: task.known_ci_evidence.clone(),
                current_stage: task.current_stage.clone(),
                pending_work: task.pending_work.clone(),
                context_references: task.context_references.clone(),
                last_committed_outbound_message: current_checkpoint
                    .as_ref()
                    .and_then(|checkpoint| checkpoint.last_committed_outbound_message.clone()),
                outbound_delivery_confirmed: current_checkpoint
                    .as_ref()
                    .is_some_and(|checkpoint| checkpoint.outbound_delivery_confirmed),
                checkpoint: current_checkpoint.clone(),
                continuation_instruction:
                    "在新会话中按 RecoveryEnvelope 继续，不要重复已经完成的步骤。".into(),
            };
            self.store
                .settle_task(&task, None, Some(&recovery), None, None)?;
        } else {
            self.store
                .settle_task(&task, None, None, None, Some(&report.message))?;
        }

        heartbeat_stop.store(true, Ordering::Relaxed);
        if !heartbeat.is_finished() {
            let _ = heartbeat.await;
        }
        self.account
            .release_target_ownership(&target_lease.target_id, &self.owner_id)?;
        self.store.release_lease(&run_id, &self.owner_id)?;
        let _ = browser.close_owned_target().await;
        Ok(report)
    }

    fn settle_terminal_response(
        &self,
        task: &QueueTask,
        run_id: &str,
        run_report: &RunReport,
    ) -> Result<()> {
        match parse_task_report(&run_report.assistant_text) {
            Ok(Some(report)) => {
                if report.task_id != task.id
                    || report.applied_task_revision != task.current_revision
                {
                    let error = "task_report_identity_or_revision_mismatch";
                    self.store
                        .settle_task(task, None, None, None, Some(error))?;
                    return Ok(());
                }
                if report.status == TaskReportStatus::Complete && report.all_tasks_complete {
                    self.store
                        .settle_task(task, Some(&report), None, None, None)?;
                    return Ok(());
                }

                let recovery = RecoveryEnvelope {
                    version: 1,
                    task_id: task.id.clone(),
                    run_id: run_id.into(),
                    exact_commit: task.known_exact_head.clone(),
                    conversation_url: run_report.conversation_url.clone(),
                    conversation_kind: task.conversation_kind.clone(),
                    original_goal: task.original_prompt.clone(),
                    acceptance_prompt: task.acceptance_prompt.clone(),
                    interrupted_turn_visible_content: run_report.visible_progress_messages.clone(),
                    progress_messages: run_report.visible_progress_messages.clone(),
                    completed: report.completed.clone(),
                    remaining: report.remaining.clone(),
                    blockers: report.blockers.clone(),
                    known_ci_evidence: task.known_ci_evidence.clone(),
                    current_stage: task.current_stage.clone(),
                    pending_work: task.pending_work.clone(),
                    context_references: task.context_references.clone(),
                    last_committed_outbound_message: None,
                    outbound_delivery_confirmed: true,
                    checkpoint: None,
                    continuation_instruction: report.next_task.clone(),
                };
                let wait_until = report
                    .wait_seconds
                    .filter(|seconds| *seconds > 0)
                    .map(|seconds| now_ms() + (seconds as i64 * 1000));
                self.store
                    .settle_task(task, Some(&report), Some(&recovery), wait_until, None)?;
            }
            Ok(None) => {
                if let Some(wait) = parse_task_wait(&run_report.assistant_text)?
                    && wait.task_id == task.id
                {
                    let recovery = RecoveryEnvelope {
                        version: 1,
                        task_id: task.id.clone(),
                        run_id: run_id.into(),
                        exact_commit: task.known_exact_head.clone(),
                        conversation_url: run_report.conversation_url.clone(),
                        conversation_kind: task.conversation_kind.clone(),
                        original_goal: task.original_prompt.clone(),
                        acceptance_prompt: task.acceptance_prompt.clone(),
                        interrupted_turn_visible_content: run_report
                            .visible_progress_messages
                            .clone(),
                        progress_messages: run_report.visible_progress_messages.clone(),
                        completed: vec![],
                        remaining: vec![wait.reason.clone()],
                        blockers: vec![],
                        known_ci_evidence: task.known_ci_evidence.clone(),
                        current_stage: task.current_stage.clone(),
                        pending_work: task.pending_work.clone(),
                        context_references: task.context_references.clone(),
                        last_committed_outbound_message: None,
                        outbound_delivery_confirmed: true,
                        checkpoint: None,
                        continuation_instruction: "等待条件结束后继续原任务".into(),
                    };
                    self.store.settle_task(
                        task,
                        None,
                        Some(&recovery),
                        Some(now_ms() + wait.wait_seconds as i64 * 1000),
                        None,
                    )?;
                    return Ok(());
                }
                self.store.settle_task(
                    task,
                    None,
                    None,
                    None,
                    Some("terminal_response_missing_valid_task_report"),
                )?;
            }
            Err(error) => {
                self.store.settle_task(
                    task,
                    None,
                    None,
                    None,
                    Some(&format!("invalid_task_report:{error}")),
                )?;
            }
        }
        Ok(())
    }
}

pub struct Supervisor {
    store: SqliteStore,
    accounts: Vec<Arc<AccountBrowserActor>>,
    semaphore: Arc<Semaphore>,
    owner_prefix: String,
}

impl Supervisor {
    pub fn open(
        sqlite_path: impl AsRef<Path>,
        accounts: Vec<(String, String)>,
        max_concurrent: usize,
    ) -> Result<Self> {
        if accounts.is_empty() {
            return Err(anyhow!("at least one account browser endpoint is required"));
        }
        let store = SqliteStore::open(sqlite_path)?;
        let owner_prefix = format!("supervisor-{}", now_ms());
        let actors = accounts
            .into_iter()
            .map(|(id, endpoint)| {
                let actor_owner_id = format!("{owner_prefix}-account-{id}");
                Arc::new(AccountBrowserActor::new(
                    id,
                    endpoint,
                    store.clone(),
                    actor_owner_id,
                ))
            })
            .collect();
        Ok(Self {
            store,
            accounts: actors,
            semaphore: Arc::new(Semaphore::new(max_concurrent.max(1))),
            owner_prefix,
        })
    }

    pub fn store(&self) -> &SqliteStore {
        &self.store
    }

    pub fn enqueue(&self, task: &QueueTask) -> Result<()> {
        self.store.enqueue_task(task)
    }

    pub fn recover_startup(&self) -> Result<u32> {
        self.store.recover_expired_leases(now_ms())
    }

    pub async fn run_one(&self) -> Result<Option<RunReport>> {
        let _permit = self.semaphore.acquire().await?;
        self.store.recover_expired_leases(now_ms())?;
        let owner_id = format!("{}-{}", self.owner_prefix, now_ms());
        let Some(claim) = self
            .store
            .claim_next_runnable(&owner_id, now_ms(), 60_000)?
        else {
            return Ok(None);
        };

        let account = self
            .accounts
            .iter()
            .find(|actor| actor.account_id() == claim.task.account_id)
            .cloned()
            .or_else(|| self.accounts.first().cloned())
            .ok_or_else(|| anyhow!("no account browser actor available"))?;
        let worker = RunWorker::new(owner_id, self.store.clone(), account);
        let recovery_url = claim.run.canonical_conversation_url.clone();
        let report = worker
            .execute_claim(claim.task, claim.run.run_id, recovery_url)
            .await?;
        Ok(Some(report))
    }

    pub fn snapshot_json(&self) -> Result<serde_json::Value> {
        let snapshot = self.store.snapshot()?;
        Ok(serde_json::json!({
            "tasks": snapshot.tasks,
            "runs": snapshot.runs,
        }))
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_envelope_is_used_for_requeued_task() {
        let mut task = QueueTask::new("task-1", "account-a", "original");
        task.recovery_context = Some(RecoveryEnvelope {
            version: 1,
            task_id: "task-1".into(),
            run_id: "run-1".into(),
            exact_commit: Some("abc".into()),
            conversation_url: Some("https://chatgpt.com/c/abc".into()),
            original_goal: "original".into(),
            acceptance_prompt: Some("验收提示".into()),
            progress_messages: vec!["已跑 CI".into()],
            completed: vec!["A".into()],
            remaining: vec!["B".into()],
            blockers: vec![],
            continuation_instruction: "继续 B".into(),
            ..Default::default()
        });
        let prompt = task.recovery_context.as_ref().unwrap().render_prompt();
        assert!(prompt.contains("已跑 CI"));
        assert!(prompt.contains("验收提示"));
        assert!(prompt.contains("继续 B"));
    }

    #[test]
    fn supervisor_requires_account_actor() {
        let path = std::env::temp_dir().join(format!("fabushi-supervisor-{}.sqlite", now_ms()));
        let result = Supervisor::open(path, vec![], 1);
        assert!(result.is_err());
    }
}
