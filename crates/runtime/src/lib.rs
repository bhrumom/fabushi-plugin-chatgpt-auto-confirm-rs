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
    ConversationKind, ExecutionProfile, ObservedExecutionProfile, QueueTask, RunRecord,
};
pub use fabushi_chatgpt_linux_browser::{
    BrowserLaunch, ManagedBrowserConfig, ManagedChromium, find_chromium_binary, launch_chromium,
};
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
    managed_config: Option<ManagedBrowserConfig>,
    managed_browser: Mutex<Option<ManagedChromium>>,
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
            managed_config: None,
            managed_browser: Mutex::new(None),
            store,
            owner_id: owner_id.into(),
            lease_duration_ms: 60_000,
            target_mutation: Mutex::new(()),
        }
    }

    pub fn new_managed(
        account_id: impl Into<String>,
        config: ManagedBrowserConfig,
        store: SqliteStore,
        owner_id: impl Into<String>,
    ) -> Self {
        let endpoint = format!("http://127.0.0.1:{}", config.port);
        Self {
            account_id: account_id.into(),
            endpoint,
            managed_config: Some(config),
            managed_browser: Mutex::new(None),
            store,
            owner_id: owner_id.into(),
            lease_duration_ms: 60_000,
            target_mutation: Mutex::new(()),
        }
    }

    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    fn acquire_browser_ownership(&self, process_identity: &str, profile_dir: &str) -> Result<()> {
        let acquired = self.store.acquire_account_browser(
            &self.account_id,
            &self.owner_id,
            process_identity,
            profile_dir,
            now_ms(),
            self.lease_duration_ms,
        )?;
        if !acquired {
            bail!(
                "account {} browser ownership is held by another live actor",
                self.account_id
            );
        }
        Ok(())
    }

    async fn ensure_browser_endpoint(&self, force_restart: bool) -> Result<String> {
        let Some(config) = self.managed_config.as_ref() else {
            self.acquire_browser_ownership(
                &format!("attached-cdp:{}", self.endpoint),
                &format!("user-owned-profile:{}", self.account_id),
            )?;
            self.store.record_browser_lifecycle(
                &self.account_id,
                &self.owner_id,
                "attached",
                &serde_json::json!({"endpoint": self.endpoint}).to_string(),
                now_ms(),
            )?;
            return Ok(self.endpoint.clone());
        };

        let mut slot = self.managed_browser.lock().await;
        let alive = match slot.as_ref() {
            Some(browser) => browser.is_alive()?,
            None => false,
        };
        if alive && !force_restart {
            let browser = slot
                .as_ref()
                .ok_or_else(|| anyhow!("managed Chromium disappeared while checking liveness"))?;
            let info = browser.launch_info();
            let renewed = self.store.renew_account_browser(
                &self.account_id,
                &self.owner_id,
                now_ms(),
                self.lease_duration_ms,
            )?;
            if !renewed {
                self.acquire_browser_ownership(
                    &format!("managed-chromium:{}", info.pid),
                    &info.profile_dir.display().to_string(),
                )?;
                self.store.bind_account_browser_process(
                    &self.account_id,
                    &self.owner_id,
                    info.pid,
                    &info.endpoint,
                    now_ms(),
                )?;
            }
            return Ok(info.endpoint.clone());
        }

        if let Some(browser) = slot.take() {
            let _ = browser.force_terminate();
            let _ = self.store.record_browser_lifecycle(
                &self.account_id,
                &self.owner_id,
                "browser_terminated_for_recovery",
                "{}",
                now_ms(),
            );
        }

        self.acquire_browser_ownership(
            &format!("managed-starting:{}", config.browser_binary.display()),
            &config.profile_dir.display().to_string(),
        )?;
        let browser = match ManagedChromium::launch(config.clone()) {
            Ok(browser) => browser,
            Err(error) => {
                let _ = self
                    .store
                    .release_account_browser(&self.account_id, &self.owner_id);
                return Err(error).context("launch managed Chromium");
            }
        };
        let info = browser.launch_info().clone();
        self.store.bind_account_browser_process(
            &self.account_id,
            &self.owner_id,
            info.pid,
            &info.endpoint,
            now_ms(),
        )?;
        self.store.record_browser_lifecycle(
            &self.account_id,
            &self.owner_id,
            if force_restart {
                "browser_restarted"
            } else {
                "browser_started"
            },
            &serde_json::json!({
                "pid": info.pid,
                "endpoint": info.endpoint,
                "profile_dir": info.profile_dir,
                "stdout_log": browser.stdout_log(),
                "stderr_log": browser.stderr_log()
            })
            .to_string(),
            now_ms(),
        )?;
        let endpoint = info.endpoint.clone();
        *slot = Some(browser);
        drop(slot);

        let started = Instant::now();
        while !ChatGptCdp::endpoint_available(&endpoint).await {
            if started.elapsed() >= Duration::from_secs(15) {
                bail!("managed Chromium CDP endpoint did not become ready: {endpoint}");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(endpoint)
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

    pub async fn force_terminate_managed_browser(&self) -> Result<bool> {
        let mut slot = self.managed_browser.lock().await;
        let Some(browser) = slot.take() else {
            return Ok(false);
        };
        browser.force_terminate()?;
        self.store.record_browser_lifecycle(
            &self.account_id,
            &self.owner_id,
            "browser_forced_termination",
            "{}",
            now_ms(),
        )?;
        Ok(true)
    }

    pub async fn lease_target(
        &self,
        run_id: &str,
        worker_owner_id: &str,
        recovery_url: Option<&str>,
    ) -> Result<(TargetLease, ChatGptCdp)> {
        let _guard = self.target_mutation.lock().await;
        let endpoint = self.ensure_browser_endpoint(false).await?;
        let url = recovery_url.unwrap_or("https://chatgpt.com/");
        let browser = ChatGptCdp::create_target(&endpoint, url)
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
    ) -> Result<(TargetLease, ChatGptCdp)> {
        let mut endpoint = self.ensure_browser_endpoint(false).await?;
        if let Some(durable) = self.store.target_lease_for_run(&lease.run_id)?
            && durable.target_id == lease.target_id
            && durable.owner_id == lease.owner_id
            && durable.expires_at_ms > now_ms()
            && let Ok(browser) = ChatGptCdp::connect_target_id(&endpoint, &lease.target_id).await
        {
            browser.navigate(canonical_url).await?;
            return Ok((lease.clone(), browser));
        }

        if !ChatGptCdp::endpoint_available(&endpoint).await && self.managed_config.is_some() {
            endpoint = self.ensure_browser_endpoint(true).await?;
            if !ChatGptCdp::endpoint_available(&endpoint).await {
                bail!("managed Chromium restart did not restore CDP endpoint");
            }
        }

        let _ = self.store.release_target(&lease.target_id, &lease.owner_id);
        self.lease_target(&lease.run_id, &lease.owner_id, Some(canonical_url))
            .await
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
        let (mut target_lease, mut browser) = self
            .account
            .lease_target(&run_id, &self.owner_id, recovery_url.as_deref())
            .await?;

        let journal = self.store.journal(&run_id, &self.owner_id);
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

        let result: Result<RunReport> = async {
            loop {
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

            let result = {
                let runner = RunPrompt::with_journal(&browser, &clock, &journal);
                let run = runner.execute(prompt, options.clone());
                tokio::pin!(run);
                tokio::select! {
                    result = &mut run => result,
                    heartbeat_result = &mut heartbeat => {
                        match heartbeat_result {
                            Ok(Ok(())) => Err(anyhow!("ownership heartbeat stopped unexpectedly")),
                            Ok(Err(error)) => Err(error),
                            Err(error) => Err(anyhow!("ownership heartbeat task failed: {error}")),
                        }
                    }
                }
            };

            heartbeat_stop.store(true, Ordering::Relaxed);
            if !heartbeat.is_finished() {
                heartbeat.abort();
                let _ = heartbeat.await;
            }

                match result {
                    Ok(report) => break Ok(report),
                Err(error) => {
                    let target_exists = browser.target_exists().await;
                    match target_exists {
                        Ok(false) => {
                            let current_run = self
                                .store
                                .snapshot()?
                                .runs
                                .into_iter()
                                .find(|run| run.run_id == run_id)
                                .ok_or_else(|| {
                                    anyhow!("run {run_id} disappeared during target recovery")
                                })?;
                            let mut checkpoint = current_run.checkpoint;
                            if checkpoint.counters.target_recoveries >= 3 {
                                checkpoint.pending_recovery =
                                    Some("target_recovery_limit_exceeded".into());
                                fabushi_chatgpt_application::RunJournal::record_with_checkpoint(
                                    &journal,
                                    &RunEvent {
                                        kind: RunEventKind::RunFailed,
                                        state: RunState::Failed,
                                        canonical_conversation_url: checkpoint
                                            .last_conversation_url
                                            .clone(),
                                        target_id: Some(target_lease.target_id.clone()),
                                        activity_fingerprint: checkpoint
                                            .last_activity_fingerprint
                                            .clone(),
                                        latest_assistant_text: current_run
                                            .latest_assistant_text
                                            .clone(),
                                        visible_progress_messages: current_run
                                            .visible_progress_messages
                                            .clone(),
                                        queue_phase: None,
                                        counters: checkpoint.counters.clone(),
                                        payload_json: serde_json::json!({
                                            "reason": "target_recovery_limit_exceeded"
                                        })
                                        .to_string(),
                                    },
                                    &checkpoint,
                                )?;
                                return Err(anyhow!(
                                    "target recovery limit exceeded after transport failure: {error}"
                                ));
                            }

                            let canonical_url = checkpoint
                                .last_conversation_url
                                .clone()
                                .or(current_run.canonical_conversation_url.clone())
                                .ok_or_else(|| {
                                    anyhow!(
                                        "target lost before a canonical conversation URL was durable: {error}"
                                    )
                                })?;
                            checkpoint.counters.target_recoveries += 1;
                            checkpoint.pending_recovery = Some("target_lost".into());
                            let mut lost = RunEvent::new(
                                RunEventKind::TargetLost,
                                RunState::Recovering,
                                checkpoint.counters.clone(),
                            );
                            lost.target_id = Some(target_lease.target_id.clone());
                            lost.canonical_conversation_url = Some(canonical_url.clone());
                            lost.latest_assistant_text = current_run.latest_assistant_text.clone();
                            lost.visible_progress_messages =
                                current_run.visible_progress_messages.clone();
                            lost.payload_json = serde_json::json!({
                                "recovery_attempt": checkpoint.counters.target_recoveries
                            })
                            .to_string();
                            fabushi_chatgpt_application::RunJournal::record_with_checkpoint(
                                &journal,
                                &lost,
                                &checkpoint,
                            )?;

                            let (replacement_lease, replacement_browser) = self
                                .account
                                .recover_target(&target_lease, &canonical_url)
                                .await
                                .context("recover lost run target through account actor")?;
                            target_lease = replacement_lease;
                            browser = replacement_browser;
                            checkpoint.pending_recovery = None;
                            let mut reattached = RunEvent::new(
                                RunEventKind::TargetReattached,
                                RunState::Running,
                                checkpoint.counters.clone(),
                            );
                            reattached.target_id = Some(target_lease.target_id.clone());
                            reattached.canonical_conversation_url = Some(canonical_url);
                            reattached.payload_json = serde_json::json!({
                                "recovery_attempt": checkpoint.counters.target_recoveries
                            })
                            .to_string();
                            fabushi_chatgpt_application::RunJournal::record_with_checkpoint(
                                &journal,
                                &reattached,
                                &checkpoint,
                            )?;
                            continue;
                        }
                        Err(probe_error) => {
                            let current_run = self
                                .store
                                .snapshot()?
                                .runs
                                .into_iter()
                                .find(|run| run.run_id == run_id)
                                .ok_or_else(|| {
                                    anyhow!("run {run_id} disappeared during browser recovery")
                                })?;
                            let mut checkpoint = current_run.checkpoint;
                            if checkpoint.counters.browser_recoveries >= 3 {
                                checkpoint.pending_recovery =
                                    Some("browser_recovery_limit_exceeded".into());
                                return Err(anyhow!(
                                    "browser recovery limit exceeded; probe error: {probe_error}; original error: {error}"
                                ));
                            }
                            let canonical_url = checkpoint
                                .last_conversation_url
                                .clone()
                                .or(current_run.canonical_conversation_url.clone())
                                .ok_or_else(|| {
                                    anyhow!(
                                        "browser lost before a canonical conversation URL was durable: {probe_error}"
                                    )
                                })?;
                            checkpoint.counters.browser_recoveries += 1;
                            checkpoint.pending_recovery = Some("browser_lost".into());
                            let mut lost = RunEvent::new(
                                RunEventKind::BrowserLost,
                                RunState::Recovering,
                                checkpoint.counters.clone(),
                            );
                            lost.target_id = Some(target_lease.target_id.clone());
                            lost.canonical_conversation_url = Some(canonical_url.clone());
                            lost.latest_assistant_text = current_run.latest_assistant_text.clone();
                            lost.visible_progress_messages =
                                current_run.visible_progress_messages.clone();
                            lost.payload_json = serde_json::json!({
                                "probe_error": probe_error.to_string(),
                                "recovery_attempt": checkpoint.counters.browser_recoveries
                            })
                            .to_string();
                            fabushi_chatgpt_application::RunJournal::record_with_checkpoint(
                                &journal,
                                &lost,
                                &checkpoint,
                            )?;

                            let (replacement_lease, replacement_browser) = self
                                .account
                                .recover_target(&target_lease, &canonical_url)
                                .await
                                .context("recover browser and target through account actor")?;
                            target_lease = replacement_lease;
                            browser = replacement_browser;
                            checkpoint.pending_recovery = None;
                            let mut reattached = RunEvent::new(
                                RunEventKind::TargetReattached,
                                RunState::Running,
                                checkpoint.counters.clone(),
                            );
                            reattached.target_id = Some(target_lease.target_id.clone());
                            reattached.canonical_conversation_url = Some(canonical_url);
                            reattached.payload_json = serde_json::json!({
                                "browser_recovery_attempt": checkpoint.counters.browser_recoveries
                            })
                            .to_string();
                            fabushi_chatgpt_application::RunJournal::record_with_checkpoint(
                                &journal,
                                &reattached,
                                &checkpoint,
                            )?;
                            continue;
                        }
                        Ok(true) => return Err(error),
                    }
                }
            }
        }
        }
        .await;

        let report = match result {
            Ok(report) => report,
            Err(error) => {
                let current_run = self
                    .store
                    .snapshot()?
                    .runs
                    .into_iter()
                    .find(|run| run.run_id == run_id);
                let recovery = current_run.as_ref().map(|run| {
                    let checkpoint = run.checkpoint.clone();
                    RecoveryEnvelope {
                        version: 2,
                        task_id: task.id.clone(),
                        run_id: run_id.clone(),
                        exact_commit: task.known_exact_head.clone(),
                        conversation_url: checkpoint
                            .last_conversation_url
                            .clone()
                            .or(run.canonical_conversation_url.clone()),
                        conversation_kind: task.conversation_kind.clone(),
                        original_goal: task.original_prompt.clone(),
                        acceptance_prompt: task.acceptance_prompt.clone(),
                        interrupted_turn_visible_content: run.visible_progress_messages.clone(),
                        progress_messages: run.visible_progress_messages.clone(),
                        completed: task
                            .last_report
                            .as_ref()
                            .map(|previous| previous.completed.clone())
                            .unwrap_or_default(),
                        remaining: if task.pending_work.is_empty() {
                            vec![error.to_string()]
                        } else {
                            task.pending_work.clone()
                        },
                        blockers: vec![error.to_string()],
                        known_ci_evidence: task.known_ci_evidence.clone(),
                        current_stage: task.current_stage.clone(),
                        pending_work: task.pending_work.clone(),
                        context_references: task.context_references.clone(),
                        last_committed_outbound_message: checkpoint
                            .last_committed_outbound_message
                            .clone(),
                        outbound_delivery_confirmed: checkpoint.outbound_delivery_confirmed,
                        checkpoint: Some(checkpoint),
                        continuation_instruction:
                            "从 durable checkpoint 恢复；不要重复已确认提交或已经完成的步骤。"
                                .into(),
                    }
                });
                self.store.settle_task(
                    &task,
                    None,
                    recovery.as_ref(),
                    None,
                    Some(&error.to_string()),
                )?;
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
            let fresh_conversation = current_checkpoint
                .as_ref()
                .is_some_and(recovery_requires_fresh_conversation);
            let recovery_checkpoint = current_checkpoint.as_ref().map(|checkpoint| {
                if fresh_conversation {
                    fresh_conversation_checkpoint(checkpoint, task.conversation_kind.clone())
                } else {
                    checkpoint.clone()
                }
            });
            let recovery = RecoveryEnvelope {
                version: 2,
                task_id: task.id.clone(),
                run_id: run_id.clone(),
                exact_commit: task.known_exact_head.clone(),
                conversation_url: if fresh_conversation {
                    None
                } else {
                    report.conversation_url.clone()
                },
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
                checkpoint: recovery_checkpoint,
                continuation_instruction:
                    "在新会话中按 RecoveryEnvelope 继续，不要重复已经完成的步骤。".into(),
            };
            self.store
                .settle_task(&task, None, Some(&recovery), None, None)?;
        } else {
            self.store
                .settle_task(&task, None, None, None, Some(&report.message))?;
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
                    if task.conversation_kind == ConversationKind::Work
                        && task
                            .acceptance_prompt
                            .as_deref()
                            .is_some_and(|value| !value.trim().is_empty())
                    {
                        let mut acceptance_task = task.clone();
                        acceptance_task.conversation_kind = ConversationKind::Acceptance;
                        acceptance_task.prompt = acceptance_handoff_prompt(task, run_report);
                        acceptance_task.recovery_context = None;
                        self.store.requeue_conversation(
                            &acceptance_task,
                            &report,
                            "work_completed_start_acceptance",
                        )?;
                    } else {
                        self.store
                            .settle_task(task, Some(&report), None, None, None)?;
                    }
                    return Ok(());
                }

                if task.conversation_kind == ConversationKind::Acceptance {
                    let mut work_task = task.clone();
                    work_task.conversation_kind = ConversationKind::Work;
                    work_task.prompt = work_handoff_prompt(task, run_report, &report);
                    work_task.recovery_context = None;
                    self.store.requeue_conversation(
                        &work_task,
                        &report,
                        "acceptance_incomplete_return_to_work",
                    )?;
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

    pub fn open_managed(
        sqlite_path: impl AsRef<Path>,
        accounts: Vec<(String, ManagedBrowserConfig)>,
        max_concurrent: usize,
    ) -> Result<Self> {
        if accounts.is_empty() {
            return Err(anyhow!("at least one managed account browser is required"));
        }
        let store = SqliteStore::open(sqlite_path)?;
        let owner_prefix = format!("supervisor-{}", now_ms());
        let actors = accounts
            .into_iter()
            .map(|(id, config)| {
                let actor_owner_id = format!("{owner_prefix}-account-{id}");
                Arc::new(AccountBrowserActor::new_managed(
                    id,
                    config,
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

    pub async fn run_until_idle(&self, max_runs: usize) -> Result<Vec<RunReport>> {
        let mut reports = Vec::new();
        for _ in 0..max_runs.max(1) {
            let Some(report) = self.run_one().await? else {
                return Ok(reports);
            };
            reports.push(report);
        }
        if self
            .store
            .snapshot()?
            .tasks
            .iter()
            .any(|task| matches!(task.status, fabushi_chatgpt_domain::TaskState::Queued))
        {
            bail!("run_until_idle reached max_runs while runnable tasks remain");
        }
        Ok(reports)
    }

    pub fn snapshot_json(&self) -> Result<serde_json::Value> {
        let snapshot = self.store.snapshot()?;
        Ok(serde_json::json!({
            "tasks": snapshot.tasks,
            "runs": snapshot.runs,
        }))
    }
}

fn recovery_requires_fresh_conversation(
    checkpoint: &fabushi_chatgpt_domain::RunCheckpoint,
) -> bool {
    matches!(
        checkpoint.pending_recovery.as_deref(),
        Some(
            "dispatch_confirmation_retry_limit"
                | "conversation_too_long"
                | "rate_limit_threshold_exceeded"
                | "refresh_attempt_limit_exceeded"
                | "continuation_attempt_limit_exceeded"
        )
    )
}

fn fresh_conversation_checkpoint(
    previous: &fabushi_chatgpt_domain::RunCheckpoint,
    conversation_kind: ConversationKind,
) -> fabushi_chatgpt_domain::RunCheckpoint {
    fabushi_chatgpt_domain::RunCheckpoint {
        conversation_kind,
        counters: previous.counters.clone(),
        ..Default::default()
    }
}

fn acceptance_handoff_prompt(task: &QueueTask, run_report: &RunReport) -> String {
    let acceptance_prompt = task
        .acceptance_prompt
        .as_deref()
        .unwrap_or("独立判断原始目标是否已经真正完成；不要代替 Work 执行。");
    format!(
        r#"请作为独立的规划与验收会话。你只负责验收和安排下一步，不要代替 Work 执行。

## 原始目标
{original_goal}

## 本轮验收要求
{acceptance_prompt}

## 最新 Work 会话自然语言结果
{work_result}

## 验收输出协议
完成判断后必须在自然语言结论末尾输出且只输出一个机器可读报告块：
MAHAYANA_TASK_REPORT_V1_BEGIN
{{"protocol":"mahayana.task-report.v1","task_id":"{task_id}","applied_task_revision":{revision},"applied_spec_digest":"","status":"complete|incomplete|blocked","all_tasks_complete":true|false,"summary":"验收结论","completed":["已有证据证明的完成项"],"remaining":["仍未完成项"],"blockers":["真实阻塞"],"verification":["验收证据"],"next_task":"未完成时给下一 Work 轮完整执行要求；完成时必须为空"}}
MAHAYANA_TASK_REPORT_V1_END

只有全部目标确有证据完成时才使用 status=complete 且 all_tasks_complete=true。未完成时 next_task 必须包含下一 Work 轮可直接执行的完整要求。"#,
        original_goal = task.original_prompt,
        acceptance_prompt = acceptance_prompt,
        work_result = run_report.assistant_text,
        task_id = task.id,
        revision = task.current_revision,
    )
}

fn work_handoff_prompt(
    task: &QueueTask,
    run_report: &RunReport,
    report: &fabushi_chatgpt_application::AutomationTaskReport,
) -> String {
    format!(
        r#"这是独立验收会话判定“尚未完成”后的下一 Work 轮。继续实际执行，不要只做方案或审查。

## 原始目标
{original_goal}

## 验收会话自然语言结果
{acceptance_result}

## 验收摘要
{summary}

## 已确认完成
{completed}

## 仍未完成
{remaining}

## 阻塞
{blockers}

## 本轮必须继续执行
{next_task}

完成本轮实际工作后，在自然语言结果末尾输出 MAHAYANA_TASK_REPORT_V1_BEGIN / MAHAYANA_TASK_REPORT_V1_END 报告，task_id 必须为 "{task_id}"，applied_task_revision 必须为 {revision}。只有所有原始目标确实完成时才报告 complete。"#,
        original_goal = task.original_prompt,
        acceptance_result = run_report.assistant_text,
        summary = report.summary,
        completed = report.completed.join("\n"),
        remaining = report.remaining.join("\n"),
        blockers = report.blockers.join("\n"),
        next_task = report.next_task,
        task_id = task.id,
        revision = task.current_revision,
    )
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

    #[tokio::test]
    #[ignore = "requires FABUSHI_CHROMIUM_BIN on a Linux runner"]
    async fn managed_browser_restart_recreates_target() {
        let browser_binary =
            std::env::var("FABUSHI_CHROMIUM_BIN").expect("FABUSHI_CHROMIUM_BIN is required");
        let root = std::env::temp_dir().join(format!("fabushi-managed-browser-{}", now_ms()));
        let profile_dir = root.join("profile");
        let log_dir = root.join("logs");
        let fixture = root.join("fixture.html");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            &fixture,
            "<!doctype html><html><head><title>managed-recovery</title></head><body>ok</body></html>",
        )
        .unwrap();
        let fixture_url = format!("file://{}", fixture.display());

        let store = SqliteStore::open_in_memory().unwrap();
        let actor = AccountBrowserActor::new_managed(
            "account-a",
            ManagedBrowserConfig {
                browser_binary: browser_binary.into(),
                profile_dir,
                port: 9333,
                headed: false,
                initial_url: "about:blank".into(),
                log_dir,
            },
            store.clone(),
            "actor-a",
        );

        let (lease, browser) = actor
            .lease_target("run-a", "worker-a", Some(&fixture_url))
            .await
            .unwrap();
        let original_target = lease.target_id.clone();
        let title = browser.evaluate("document.title").await.unwrap();
        assert_eq!(title.as_str(), Some("managed-recovery"));

        assert!(actor.force_terminate_managed_browser().await.unwrap());
        let (replacement_lease, replacement_browser) =
            actor.recover_target(&lease, &fixture_url).await.unwrap();
        assert_ne!(replacement_lease.target_id, original_target);
        let recovered_title = replacement_browser
            .evaluate("document.title")
            .await
            .unwrap();
        assert_eq!(recovered_title.as_str(), Some("managed-recovery"));

        actor
            .release_target_ownership(&replacement_lease.target_id, "worker-a")
            .unwrap();
        replacement_browser.close_owned_target().await.unwrap();
        let _ = std::fs::remove_dir_all(root);
    }
}
