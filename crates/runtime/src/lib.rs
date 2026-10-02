use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use fabushi_chatgpt_application::{
    ChatProcessHealth, ChatProcessPort, ChatSurfacePort, Clock, ContinuousTaskLifecycle,
    ContinuousTaskState, DurableReviewSettlementState, ReasoningDecision, ReasoningGateState,
    RecoveryRunContext, ReviewRunIdentity, ReviewSettlementKey, ReviewSettlementPort,
    RunControlPort, RunPrompt, WakeReason, parse_strict_review_report,
};
use fabushi_chatgpt_attachment_store::{AttachmentStore, StoredAttachment};
#[cfg(target_os = "linux")]
use fabushi_chatgpt_desktop_atspi::ChatGptDesktopAtspi;
#[cfg(target_os = "macos")]
use fabushi_chatgpt_desktop_macos::{ChatGptDesktopMacProcess, ChatGptDesktopMacSurface};
#[cfg(target_os = "linux")]
use fabushi_chatgpt_desktop_process::ChatGptDesktopProcess;
use fabushi_chatgpt_sqlite_store::{
    AttachmentRecord, IncompleteContinuousPhase, PreparedApproval, PreparedDispatch,
    ReviewSettlementRecord, SqliteStore, StateTransitionRecord, TransitionRecord, UiSessionLease,
};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{Notify, OnceCell, mpsc, oneshot};
use tokio::time::MissedTickBehavior;

pub use fabushi_chatgpt_application::RunOptions;
pub use fabushi_chatgpt_cdp::ChatGptCdp;
use fabushi_chatgpt_domain::{
    AttachmentId, AuthorizationSettlementState, ConversationFingerprint, DispatchId, GoalRevision,
    OwnershipConfidence, RunId, UserTurnBoundary,
};
pub use fabushi_chatgpt_domain::{
    ChatSurfaceSnapshot, Phase, ReasoningPreset, Round, RunReport, RunState, TaskId,
};
pub use fabushi_chatgpt_linux_browser::{BrowserLaunch, find_chromium_binary, launch_chromium};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

const DESKTOP_UI_LEASE_NAME: &str = "chatgpt-desktop-ui";
const DESKTOP_UI_LEASE_TTL_MS: i64 = 15_000;
const DESKTOP_UI_LEASE_HEARTBEAT: Duration = Duration::from_secs(5);
const STARTUP_PENDING_EFFECT_LIMIT: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupReconcileOutcome {
    Clear,
    DeferredToRunWorker,
    SettledObservedSend,
    SettledObservedReasoning,
    SettledObservedApproval,
    SettledObservedFreshConversation,
}

struct DurableUiLease {
    store: SqliteStore,
    lease: UiSessionLease,
    owner_id: String,
}

impl DurableUiLease {
    fn acquire(path: &Path, owner_id: String) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create state directory {}", parent.display()))?;
        }
        let mut store = SqliteStore::open(path)?;
        let now = unix_time_ms()?;
        let lease = store
            .acquire_ui_session_lease(
                DESKTOP_UI_LEASE_NAME,
                &owner_id,
                now,
                DESKTOP_UI_LEASE_TTL_MS,
            )?
            .ok_or_else(|| {
                anyhow::anyhow!("ChatGPT desktop UI is owned by another Fabushi process")
            })?;
        Ok(Self {
            store,
            lease,
            owner_id,
        })
    }

    fn ensure(&mut self) -> Result<()> {
        let now = unix_time_ms()?;
        if self
            .store
            .renew_ui_session_lease(&self.lease, now, DESKTOP_UI_LEASE_TTL_MS)?
        {
            self.lease.expires_at_unix_ms = now + DESKTOP_UI_LEASE_TTL_MS;
            return Ok(());
        }

        self.lease = self
            .store
            .acquire_ui_session_lease(
                DESKTOP_UI_LEASE_NAME,
                &self.owner_id,
                now,
                DESKTOP_UI_LEASE_TTL_MS,
            )?
            .ok_or_else(|| {
                anyhow::anyhow!("lost ChatGPT desktop UI lease to another Fabushi process")
            })?;
        Ok(())
    }

    fn release(&self) {
        let _ = self.store.release_ui_session_lease(&self.lease);
    }
}

enum DesktopMutation {
    SetReasoning {
        preset: ReasoningPreset,
        reply: oneshot::Sender<Result<bool>>,
    },
    SendPrompt {
        prompt: String,
        reply: oneshot::Sender<Result<()>>,
    },
    AttachFile {
        file_name: String,
        bytes: Vec<u8>,
        reply: oneshot::Sender<Result<bool>>,
    },
    ApproveCurrentConversation {
        reply: oneshot::Sender<Result<bool>>,
    },
    DismissRateLimitNotice {
        reply: oneshot::Sender<Result<bool>>,
    },
    RecoverCurrentSurface {
        reply: oneshot::Sender<Result<()>>,
    },
    StartFreshConversation {
        reply: oneshot::Sender<Result<()>>,
    },
}

#[derive(Clone)]
pub struct DesktopSessionActorHandle {
    surface: Arc<dyn ChatSurfacePort>,
    mutations: mpsc::Sender<DesktopMutation>,
}

impl DesktopSessionActorHandle {
    #[cfg(test)]
    fn spawn(surface: Arc<dyn ChatSurfacePort>) -> Self {
        Self::spawn_inner(surface, None)
    }

    fn spawn_durable(surface: Arc<dyn ChatSurfacePort>, state_db_path: &Path) -> Result<Self> {
        let owner_id = format!("{}-{}", std::process::id(), dispatch_marker()?);
        let lease = DurableUiLease::acquire(state_db_path, owner_id)?;
        Ok(Self::spawn_inner(surface, Some(lease)))
    }

    fn spawn_inner(
        surface: Arc<dyn ChatSurfacePort>,
        mut durable_lease: Option<DurableUiLease>,
    ) -> Self {
        let (mutations, mut inbox) = mpsc::channel::<DesktopMutation>(64);
        let actor_surface = surface.clone();
        tokio::spawn(async move {
            let mut heartbeat = tokio::time::interval(DESKTOP_UI_LEASE_HEARTBEAT);
            heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = heartbeat.tick(), if durable_lease.is_some() => {
                        if let Some(lease) = durable_lease.as_mut() {
                            let _ = lease.ensure();
                        }
                    }
                    mutation = inbox.recv() => {
                        let Some(mutation) = mutation else { break; };
                        if let Some(lease) = durable_lease.as_mut()
                            && let Err(error) = lease.ensure()
                        {
                            reject_mutation(mutation, error);
                            continue;
                        }
                        execute_mutation(&*actor_surface, mutation).await;
                    }
                }
            }
            if let Some(lease) = durable_lease.as_ref() {
                lease.release();
            }
        });
        Self { surface, mutations }
    }

    async fn request<T>(
        &self,
        mutation: impl FnOnce(oneshot::Sender<Result<T>>) -> DesktopMutation,
    ) -> Result<T> {
        let (reply, receive) = oneshot::channel();
        self.mutations
            .send(mutation(reply))
            .await
            .map_err(|_| anyhow::anyhow!("desktop session actor stopped"))?;
        receive
            .await
            .map_err(|_| anyhow::anyhow!("desktop session actor dropped mutation reply"))?
    }
}

async fn execute_mutation(surface: &dyn ChatSurfacePort, mutation: DesktopMutation) {
    match mutation {
        DesktopMutation::SetReasoning { preset, reply } => {
            let _ = reply.send(surface.set_reasoning_preset(preset).await);
        }
        DesktopMutation::SendPrompt { prompt, reply } => {
            let _ = reply.send(surface.send_prompt(&prompt).await);
        }
        DesktopMutation::AttachFile {
            file_name,
            bytes,
            reply,
        } => {
            let _ = reply.send(surface.attach_file(&file_name, &bytes).await);
        }
        DesktopMutation::ApproveCurrentConversation { reply } => {
            let _ = reply.send(surface.approve_current_conversation().await);
        }
        DesktopMutation::DismissRateLimitNotice { reply } => {
            let _ = reply.send(surface.dismiss_rate_limit_notice().await);
        }
        DesktopMutation::RecoverCurrentSurface { reply } => {
            let _ = reply.send(surface.recover_current_surface().await);
        }
        DesktopMutation::StartFreshConversation { reply } => {
            let _ = reply.send(surface.start_fresh_conversation().await);
        }
    }
}

fn reject_mutation(mutation: DesktopMutation, error: anyhow::Error) {
    let message = error.to_string();
    match mutation {
        DesktopMutation::SetReasoning { reply, .. }
        | DesktopMutation::AttachFile { reply, .. }
        | DesktopMutation::ApproveCurrentConversation { reply }
        | DesktopMutation::DismissRateLimitNotice { reply } => {
            let _ = reply.send(Err(anyhow::anyhow!(message)));
        }
        DesktopMutation::SendPrompt { reply, .. }
        | DesktopMutation::RecoverCurrentSurface { reply }
        | DesktopMutation::StartFreshConversation { reply } => {
            let _ = reply.send(Err(anyhow::anyhow!(message)));
        }
    }
}

#[async_trait]
impl ChatSurfacePort for DesktopSessionActorHandle {
    async fn observe(&self) -> Result<ChatSurfaceSnapshot> {
        self.surface.observe().await
    }

    async fn set_reasoning_preset(&self, preset: ReasoningPreset) -> Result<bool> {
        self.request(|reply| DesktopMutation::SetReasoning { preset, reply })
            .await
    }

    async fn send_prompt(&self, prompt: &str) -> Result<()> {
        let prompt = prompt.to_owned();
        self.request(|reply| DesktopMutation::SendPrompt { prompt, reply })
            .await
    }

    async fn attach_file(&self, file_name: &str, bytes: &[u8]) -> Result<bool> {
        let file_name = file_name.to_owned();
        let bytes = bytes.to_vec();
        self.request(|reply| DesktopMutation::AttachFile {
            file_name,
            bytes,
            reply,
        })
        .await
    }

    async fn attachment_ready(&self, file_name: &str) -> Result<bool> {
        self.surface.attachment_ready(file_name).await
    }

    async fn approve_current_conversation(&self) -> Result<bool> {
        self.request(|reply| DesktopMutation::ApproveCurrentConversation { reply })
            .await
    }

    async fn dismiss_rate_limit_notice(&self) -> Result<bool> {
        self.request(|reply| DesktopMutation::DismissRateLimitNotice { reply })
            .await
    }

    async fn recover_current_surface(&self) -> Result<()> {
        self.request(|reply| DesktopMutation::RecoverCurrentSurface { reply })
            .await
    }

    async fn start_fresh_conversation(&self) -> Result<()> {
        self.request(|reply| DesktopMutation::StartFreshConversation { reply })
            .await
    }
}

struct DurableRunJournal {
    store: SqliteStore,
    task_id: TaskId,
    run_id: RunId,
    revision: i64,
}

impl DurableRunJournal {
    fn open(path: &Path, task_id: TaskId, run_id: RunId) -> Result<Self> {
        let store = SqliteStore::open(path)?;
        let revision = store.task_revision(&task_id)?.unwrap_or(0);
        Ok(Self {
            store,
            task_id,
            run_id,
            revision,
        })
    }

    fn begin(
        &mut self,
        effect_kind: &str,
        effect_payload_json: String,
        prepared_dispatch: Option<PreparedDispatch>,
        prepared_approval: Option<PreparedApproval>,
    ) -> Result<i64> {
        let next_revision = self.revision + 1;
        let mut hasher = Sha256::new();
        hasher.update(self.task_id.as_str().as_bytes());
        hasher.update(self.run_id.as_str().as_bytes());
        hasher.update(next_revision.to_le_bytes());
        hasher.update(effect_kind.as_bytes());
        hasher.update(effect_payload_json.as_bytes());
        let idempotency_key = format!("run-effect:{:x}", hasher.finalize());
        let existing_state = self.store.task_state_json(&self.task_id)?;
        let materialized_state_json = merge_task_runtime_state(
            existing_state.as_deref(),
            &self.task_id,
            &self.run_id,
            next_revision,
            effect_kind,
        )?;
        self.store.record_transition(
            &TransitionRecord {
                task_id: self.task_id.clone(),
                run_id: self.run_id.clone(),
                expected_revision: self.revision,
                next_revision,
                event_kind: format!("{effect_kind}_prepared"),
                event_payload_json: effect_payload_json.clone(),
                materialized_state_json,
                effect_kind: effect_kind.to_owned(),
                effect_payload_json,
                idempotency_key: idempotency_key.clone(),
                prepared_dispatch,
                prepared_approval,
            },
            unix_time_ms()?,
        )?;
        self.revision = next_revision;
        let effect = self
            .store
            .pending_effects(128)?
            .into_iter()
            .find(|effect| effect.idempotency_key == idempotency_key)
            .ok_or_else(|| anyhow::anyhow!("durable effect missing after commit"))?;
        self.store.mark_effect_attempted(effect.id)?;
        Ok(effect.id)
    }

    fn settle(&self, effect_id: i64, ok: bool, detail: &str) -> Result<()> {
        self.store.settle_effect(
            effect_id,
            &json!({ "ok": ok, "detail": detail }).to_string(),
            unix_time_ms()?,
        )
    }

    fn mark_attempted(&self, effect_id: i64) -> Result<()> {
        self.store.mark_effect_attempted(effect_id)
    }

    fn arm_approval_settlement(&self, fingerprint: &str, until_unix_ms: i64) -> Result<()> {
        self.store
            .arm_approval_settlement(fingerprint, until_unix_ms)
    }

    fn settle_approval(&self, effect_id: i64, fingerprint: &str, detail: &str) -> Result<()> {
        self.store.settle_effect(
            effect_id,
            &json!({"ok": true, "detail": detail}).to_string(),
            unix_time_ms()?,
        )?;
        self.store.settle_approval_fingerprint(fingerprint)
    }

    fn settle_confirmed_dispatch(
        &mut self,
        effect_id: i64,
        dispatch_id: &DispatchId,
        settlement: serde_json::Value,
    ) -> Result<()> {
        self.store.settle_confirmed_dispatch_effect(
            effect_id,
            &self.task_id,
            &self.run_id,
            dispatch_id,
            &settlement.to_string(),
            unix_time_ms()?,
        )
    }
}

#[derive(Debug, Clone)]
struct PendingReasoningSettlement {
    effect_id: i64,
    target: ReasoningPreset,
}

#[derive(Debug, Clone)]
struct PendingSendSettlement {
    effect_id: i64,
    dispatch_id: DispatchId,
    baseline_user_turn: Option<UserTurnBoundary>,
}

#[derive(Debug, Clone)]
struct PendingApprovalSettlement {
    effect_id: i64,
    fingerprint: String,
    conversation_fingerprint: ConversationFingerprint,
    settlement_until_unix_ms: i64,
}

#[derive(Debug, Clone)]
struct PendingAttachmentSettlement {
    effect_id: i64,
    readiness_deadline_unix_ms: i64,
}

#[derive(Clone)]
struct DurableRunIdentity {
    task_id: TaskId,
    run_id: RunId,
    dispatch_id: DispatchId,
    phase: Phase,
    round: Round,
}

#[derive(Clone)]
struct DurableRunSurface {
    surface: DesktopSessionActorHandle,
    task_id: TaskId,
    foreground_gate: ForegroundGate,
    supervisor: SupervisorHandle,
    attachment_root: PathBuf,
    journal: Arc<Mutex<DurableRunJournal>>,
    dispatch_id: Arc<Mutex<DispatchId>>,
    phase: Phase,
    round: Round,
    pending_reasoning: Arc<Mutex<Option<PendingReasoningSettlement>>>,
    pending_send: Arc<Mutex<Option<PendingSendSettlement>>>,
    pending_approval: Arc<Mutex<Option<PendingApprovalSettlement>>>,
    pending_attachments: Arc<Mutex<HashMap<String, PendingAttachmentSettlement>>>,
}

impl DurableRunSurface {
    fn new_with_identity(
        surface: DesktopSessionActorHandle,
        state_db_path: &Path,
        identity: DurableRunIdentity,
        supervisor: SupervisorHandle,
    ) -> Result<Self> {
        let journal = DurableRunJournal::open(
            state_db_path,
            identity.task_id.clone(),
            identity.run_id.clone(),
        )?;
        let phase_label = match identity.phase {
            Phase::Work => "work",
            Phase::Review => "review",
        };
        let now = unix_time_ms()?;
        let mut restored_approval = None;
        let mut restored_attachments = HashMap::new();
        for effect in journal.store.pending_effects(128)? {
            if effect.task_id == journal.task_id.as_str()
                && effect.run_id == journal.run_id.as_str()
                && effect.effect_kind == "attach_file"
            {
                let payload: serde_json::Value = serde_json::from_str(&effect.effect_payload_json)
                    .context("parse durable attachment effect while restoring run surface")?;
                let file_name = payload
                    .get("fileName")
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!("pending attachment effect is missing fileName")
                    })?;
                let readiness_deadline_unix_ms = payload
                    .get("readinessDeadlineUnixMs")
                    .and_then(serde_json::Value::as_i64)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "pending attachment effect is missing durable readiness deadline"
                        )
                    })?;
                restored_attachments.insert(
                    file_name.to_owned(),
                    PendingAttachmentSettlement {
                        effect_id: effect.id,
                        readiness_deadline_unix_ms,
                    },
                );
                continue;
            }
            if effect.task_id != journal.task_id.as_str()
                || effect.run_id != journal.run_id.as_str()
                || effect.effect_kind != "approve_current_conversation"
            {
                continue;
            }
            let payload: serde_json::Value = serde_json::from_str(&effect.effect_payload_json)
                .context("parse durable approval effect while restoring run surface")?;
            let Some(fingerprint) = payload
                .get("fingerprint")
                .and_then(serde_json::Value::as_str)
            else {
                continue;
            };
            let Some(record) = journal.store.approval_fingerprint(fingerprint)? else {
                continue;
            };
            if record.phase != phase_label || record.round != i64::from(identity.round.get()) {
                bail!("durable approval identity does not match requested phase/round");
            }
            let Some(until) = record.settlement_until_unix_ms else {
                continue;
            };
            if record.state != "settling" {
                continue;
            }
            restored_approval = Some(PendingApprovalSettlement {
                effect_id: effect.id,
                fingerprint: record.fingerprint,
                conversation_fingerprint: ConversationFingerprint::new(
                    record.conversation_fingerprint,
                ),
                settlement_until_unix_ms: until,
            });
            if until > now {
                break;
            }
        }

        Ok(Self {
            surface,
            task_id: identity.task_id,
            foreground_gate: supervisor.foreground_gate.clone(),
            supervisor,
            attachment_root: attachment_root_for_state_db(state_db_path),
            journal: Arc::new(Mutex::new(journal)),
            dispatch_id: Arc::new(Mutex::new(identity.dispatch_id)),
            phase: identity.phase,
            round: identity.round,
            pending_reasoning: Arc::new(Mutex::new(None)),
            pending_send: Arc::new(Mutex::new(None)),
            pending_approval: Arc::new(Mutex::new(restored_approval)),
            pending_attachments: Arc::new(Mutex::new(restored_attachments)),
        })
    }

    #[cfg(test)]
    fn new(
        surface: DesktopSessionActorHandle,
        state_db_path: &Path,
        task_id: TaskId,
        run_id: RunId,
        dispatch_id: DispatchId,
        phase: Phase,
        round: Round,
    ) -> Result<Self> {
        Self::new_with_identity(
            surface,
            state_db_path,
            DurableRunIdentity {
                task_id,
                run_id,
                dispatch_id,
                phase,
                round,
            },
            SupervisorHandle::spawn(state_db_path.to_path_buf()),
        )
    }

    async fn mutation_permit(&self) -> ForegroundMutationPermit {
        self.foreground_gate.acquire_mutation(&self.task_id).await
    }

    async fn record_and_settle<T>(
        &self,
        effect_kind: &str,
        payload: serde_json::Value,
        effect: impl std::future::Future<Output = Result<T>> + Send,
    ) -> Result<T>
    where
        T: Send,
    {
        let _mutation_permit = self.mutation_permit().await;
        let effect_id = {
            let mut journal = self.journal.lock().await;
            journal.begin(effect_kind, payload.to_string(), None, None)?
        };
        let result = effect.await;
        let settlement = match &result {
            Ok(_) => (true, "effect returned successfully".to_owned()),
            Err(error) => (false, error.to_string()),
        };
        {
            let journal = self.journal.lock().await;
            journal.settle(effect_id, settlement.0, &settlement.1)?;
        }
        result
    }

    async fn settle_pending_attachment_if_ready(&self, file_name: &str) -> Result<bool> {
        if !self.surface.attachment_ready(file_name).await? {
            return Ok(false);
        }
        if let Some(pending) = self.pending_attachments.lock().await.remove(file_name) {
            let journal = self.journal.lock().await;
            journal.settle(
                pending.effect_id,
                true,
                "attachment filename/preview readiness observed",
            )?;
        }
        Ok(true)
    }

    async fn attach_required_file(
        &self,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<PendingAttachmentSettlement> {
        if let Some(existing) = self
            .pending_attachments
            .lock()
            .await
            .get(file_name)
            .cloned()
        {
            return Ok(existing);
        }
        let digest = format!("{:x}", Sha256::digest(bytes));
        let readiness_deadline_unix_ms = unix_time_ms()?.saturating_add(45_000);
        let effect_id = {
            let mut journal = self.journal.lock().await;
            journal.begin(
                "attach_file",
                json!({
                    "fileName": file_name,
                    "sha256": digest,
                    "readinessDeadlineUnixMs": readiness_deadline_unix_ms,
                })
                .to_string(),
                None,
                None,
            )?
        };
        let pending = PendingAttachmentSettlement {
            effect_id,
            readiness_deadline_unix_ms,
        };
        self.pending_attachments
            .lock()
            .await
            .insert(file_name.to_owned(), pending.clone());
        let accepted = {
            let _mutation_permit = self.mutation_permit().await;
            self.surface.attach_file(file_name, bytes).await?
        };
        if !accepted {
            let journal = self.journal.lock().await;
            journal.settle(
                effect_id,
                false,
                "desktop attachment action was not accepted",
            )?;
            self.pending_attachments.lock().await.remove(file_name);
            bail!("desktop attachment action was not accepted for {file_name}");
        }
        Ok(pending)
    }

    async fn ensure_required_attachments(&self) -> Result<()> {
        let records = {
            let journal = self.journal.lock().await;
            journal.store.task_attachments(&self.task_id)?
        };
        if records.is_empty() {
            return Ok(());
        }
        let store = AttachmentStore::open(&self.attachment_root)?;
        for record in records {
            let byte_len = serde_json::from_str::<serde_json::Value>(&record.metadata_json)
                .context("parse persisted attachment metadata")?
                .get("byteLen")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "persisted attachment metadata missing byteLen for {}",
                        record.file_name
                    )
                })?;
            let bytes = store.bytes(&StoredAttachment {
                attachment_id: record.attachment_id.clone(),
                file_name: record.file_name.clone(),
                sha256: record.sha256.clone(),
                byte_len,
                storage_ref: PathBuf::from(&record.storage_ref),
            })?;
            let mut retry = 0_u32;
            loop {
                if self
                    .settle_pending_attachment_if_ready(&record.file_name)
                    .await?
                {
                    break;
                }
                let pending = self.attach_required_file(&record.file_name, &bytes).await?;
                let readiness_deadline = pending.readiness_deadline_unix_ms;
                loop {
                    if self
                        .settle_pending_attachment_if_ready(&record.file_name)
                        .await?
                    {
                        break;
                    }
                    if unix_time_ms()? >= readiness_deadline {
                        break;
                    }
                    let run_id = { self.journal.lock().await.run_id.clone() };
                    self.supervisor
                        .sleep_for(
                            &self.task_id,
                            &run_id,
                            WakeReason::AttachmentWait,
                            Duration::from_secs(1),
                        )
                        .await?;
                }
                if self.surface.attachment_ready(&record.file_name).await? {
                    self.settle_pending_attachment_if_ready(&record.file_name)
                        .await?;
                    break;
                }
                if let Some(pending) = self
                    .pending_attachments
                    .lock()
                    .await
                    .remove(&record.file_name)
                {
                    let journal = self.journal.lock().await;
                    journal.settle(
                        pending.effect_id,
                        false,
                        "attachment readiness was not observed within 45 seconds",
                    )?;
                }
                let shift = retry.min(4);
                let backoff_secs = (5_u64 << shift).min(60);
                retry = retry.saturating_add(1);
                let run_id = { self.journal.lock().await.run_id.clone() };
                self.supervisor
                    .sleep_for(
                        &self.task_id,
                        &run_id,
                        WakeReason::AttachmentWait,
                        Duration::from_secs(backoff_secs),
                    )
                    .await?;
            }
        }
        Ok(())
    }

    async fn settle_pending_reasoning_if_observed(
        &self,
        snapshot: &ChatSurfaceSnapshot,
    ) -> Result<bool> {
        let pending = self.pending_reasoning.lock().await.clone();
        let Some(pending) = pending else {
            return Ok(false);
        };
        if snapshot.selected_reasoning_preset != Some(pending.target) {
            return Ok(false);
        }
        {
            let journal = self.journal.lock().await;
            journal.settle(
                pending.effect_id,
                true,
                "reasoning preset re-observed after UI mutation",
            )?;
        }
        *self.pending_reasoning.lock().await = None;
        Ok(true)
    }

    async fn settle_pending_send_if_observed(
        &self,
        snapshot: &ChatSurfaceSnapshot,
    ) -> Result<bool> {
        let pending = self.pending_send.lock().await.clone();
        let Some(pending) = pending else {
            return Ok(false);
        };
        let confirmed = snapshot.current_dispatch_id.as_ref() == Some(&pending.dispatch_id)
            && snapshot.user_turn_ownership == OwnershipConfidence::Strong
            && snapshot.user_turn_boundary.is_some()
            && snapshot.user_turn_boundary != pending.baseline_user_turn;
        if !confirmed {
            return Ok(false);
        }

        {
            let mut journal = self.journal.lock().await;
            journal.settle_confirmed_dispatch(
                pending.effect_id,
                &pending.dispatch_id,
                json!({
                    "ok": true,
                    "detail": "dispatch marker and new strong user turn observed",
                    "confirmed": true,
                    "userTurnBoundary": snapshot.user_turn_boundary.as_ref().map(|value| value.as_str()),
                    "conversationFingerprint": snapshot.conversation_fingerprint.as_ref().map(|value| value.as_str()),
                }),
            )?;
        }
        *self.pending_send.lock().await = None;
        Ok(true)
    }

    async fn settle_pending_send_for_recovery(&self) -> Result<()> {
        let pending = self.pending_send.lock().await.take();
        if let Some(pending) = pending {
            let journal = self.journal.lock().await;
            journal.settle(
                pending.effect_id,
                false,
                "unconfirmed send superseded by fresh-conversation recovery",
            )?;
        }
        Ok(())
    }
}

impl DurableRunSurface {
    async fn project_and_settle_pending_approval(
        &self,
        snapshot: &mut ChatSurfaceSnapshot,
    ) -> Result<()> {
        let pending = self.pending_approval.lock().await.clone();
        let Some(pending) = pending else {
            return Ok(());
        };
        let now = unix_time_ms()?;
        let same_conversation =
            snapshot.conversation_fingerprint.as_ref() == Some(&pending.conversation_fingerprint);
        if now < pending.settlement_until_unix_ms {
            snapshot.authorization_settlement = AuthorizationSettlementState::Settling;
            return Ok(());
        }
        if !same_conversation {
            snapshot.authorization_settlement = AuthorizationSettlementState::Settling;
            return Ok(());
        }
        if !snapshot.authorization_surface_present
            && snapshot.authorization_settlement == AuthorizationSettlementState::Inactive
        {
            {
                let journal = self.journal.lock().await;
                journal.settle_approval(
                    pending.effect_id,
                    &pending.fingerprint,
                    "approval semantic postcondition observed after settlement window",
                )?;
            }
            *self.pending_approval.lock().await = None;
        }
        Ok(())
    }
}

#[async_trait]
impl ChatSurfacePort for DurableRunSurface {
    async fn observe(&self) -> Result<ChatSurfaceSnapshot> {
        let mut snapshot = self.surface.observe().await?;
        self.settle_pending_reasoning_if_observed(&snapshot).await?;
        self.settle_pending_send_if_observed(&snapshot).await?;
        self.project_and_settle_pending_approval(&mut snapshot)
            .await?;
        Ok(snapshot)
    }

    async fn set_reasoning_preset(&self, preset: ReasoningPreset) -> Result<bool> {
        let _mutation_permit = self.mutation_permit().await;
        let existing = self.pending_reasoning.lock().await.clone();
        if let Some(existing) = existing.as_ref()
            && existing.target != preset
        {
            bail!("unsettled reasoning effect targets a different preset");
        }

        let effect_id = if let Some(existing) = existing {
            let journal = self.journal.lock().await;
            journal.mark_attempted(existing.effect_id)?;
            existing.effect_id
        } else {
            let effect_id = {
                let mut journal = self.journal.lock().await;
                journal.begin(
                    "set_reasoning",
                    json!({"preset": preset.index()}).to_string(),
                    None,
                    None,
                )?
            };
            *self.pending_reasoning.lock().await = Some(PendingReasoningSettlement {
                effect_id,
                target: preset,
            });
            effect_id
        };

        let result = self.surface.set_reasoning_preset(preset).await;
        if result.is_ok() {
            let snapshot = self.surface.observe().await?;
            self.settle_pending_reasoning_if_observed(&snapshot).await?;
        }
        let _ = effect_id;
        result
    }

    async fn attach_file(&self, file_name: &str, bytes: &[u8]) -> Result<bool> {
        self.attach_required_file(file_name, bytes).await?;
        Ok(true)
    }

    async fn attachment_ready(&self, file_name: &str) -> Result<bool> {
        self.settle_pending_attachment_if_ready(file_name).await
    }

    async fn send_prompt(&self, prompt: &str) -> Result<()> {
        self.ensure_required_attachments().await?;
        self.foreground_gate.acquire_exclusive(&self.task_id).await;
        let _mutation_permit = self.mutation_permit().await;
        let baseline = self.surface.observe().await?.user_turn_boundary;
        let dispatch_id = self.dispatch_id.lock().await.clone();
        let prepared_prompt = format!("{prompt}\n\n[Fabushi:{}]", dispatch_id.as_str());
        let prepared_intent = json!({
            "preparedPrompt": prepared_prompt,
            "dispatchId": dispatch_id.as_str(),
        });
        let effect_payload = json!({
            "preparedPrompt": prepared_prompt,
            "dispatchId": dispatch_id.as_str(),
            "baselineUserTurnBoundary": baseline.as_ref().map(|value| value.as_str()),
        });
        let effect_id = {
            let mut journal = self.journal.lock().await;
            journal.begin(
                "send_prompt",
                effect_payload.to_string(),
                Some(PreparedDispatch {
                    dispatch_id: dispatch_id.clone(),
                    prepared_intent_json: prepared_intent.to_string(),
                }),
                None,
            )?
        };
        *self.pending_send.lock().await = Some(PendingSendSettlement {
            effect_id,
            dispatch_id,
            baseline_user_turn: baseline,
        });

        let result = self.surface.send_prompt(&prepared_prompt).await;
        if result.is_ok() {
            let snapshot = self.surface.observe().await?;
            self.settle_pending_send_if_observed(&snapshot).await?;
        }
        result
    }

    async fn expected_dispatch_id(&self) -> Result<Option<DispatchId>> {
        Ok(Some(self.dispatch_id.lock().await.clone()))
    }

    async fn approve_current_conversation(&self) -> Result<bool> {
        let _mutation_permit = self.mutation_permit().await;
        if self.pending_approval.lock().await.is_some() {
            bail!("approval effect is already pending semantic settlement");
        }

        let snapshot = self.surface.observe().await?;
        if !snapshot.authorization_surface_present || !snapshot.authorization_actionable {
            return Ok(false);
        }
        let conversation_fingerprint = snapshot
            .conversation_fingerprint
            .clone()
            .ok_or_else(|| anyhow::anyhow!("authorization requires a conversation fingerprint"))?;

        let (task_id, run_id) = {
            let journal = self.journal.lock().await;
            (journal.task_id.clone(), journal.run_id.clone())
        };
        let phase = match self.phase {
            Phase::Work => "work",
            Phase::Review => "review",
        };
        let round_text = self.round.get().to_string();
        let mut hasher = Sha256::new();
        for part in [
            task_id.as_str(),
            run_id.as_str(),
            phase,
            round_text.as_str(),
            conversation_fingerprint.as_str(),
        ] {
            hasher.update(part.as_bytes());
            hasher.update([0]);
        }
        let fingerprint = format!("approval:{:x}", hasher.finalize());
        let payload = json!({
            "fingerprint": fingerprint,
            "taskId": task_id.as_str(),
            "runId": run_id.as_str(),
            "phase": phase,
            "round": self.round.get(),
            "conversationFingerprint": conversation_fingerprint.as_str(),
        });
        let effect_id = {
            let mut journal = self.journal.lock().await;
            journal.begin(
                "approve_current_conversation",
                payload.to_string(),
                None,
                Some(PreparedApproval {
                    fingerprint: fingerprint.clone(),
                    phase: phase.to_owned(),
                    round: i64::from(self.round.get()),
                    conversation_fingerprint: conversation_fingerprint.as_str().to_owned(),
                }),
            )?
        };

        let accepted = self.surface.approve_current_conversation().await?;
        if !accepted {
            bail!(
                "conversation-scoped approval action was not accepted; durable approval intent remains pending for semantic reconciliation"
            );
        }

        let settlement_until_unix_ms = unix_time_ms()? + 12_000;
        {
            let journal = self.journal.lock().await;
            journal.arm_approval_settlement(&fingerprint, settlement_until_unix_ms)?;
        }
        *self.pending_approval.lock().await = Some(PendingApprovalSettlement {
            effect_id,
            fingerprint,
            conversation_fingerprint,
            settlement_until_unix_ms,
        });
        Ok(true)
    }

    async fn dismiss_rate_limit_notice(&self) -> Result<bool> {
        self.record_and_settle(
            "dismiss_rate_limit_notice",
            json!({}),
            self.surface.dismiss_rate_limit_notice(),
        )
        .await
    }

    async fn recover_current_surface(&self) -> Result<()> {
        self.record_and_settle(
            "recover_current_surface",
            json!({}),
            self.surface.recover_current_surface(),
        )
        .await
    }

    async fn start_fresh_conversation(&self) -> Result<()> {
        self.foreground_gate.acquire_exclusive(&self.task_id).await;
        self.settle_pending_send_for_recovery().await?;
        let _mutation_permit = self.mutation_permit().await;
        let baseline = self.surface.observe().await?;
        let baseline_user_turn_boundary = baseline
            .user_turn_boundary
            .as_ref()
            .map(|value| value.as_str().to_owned());
        let baseline_conversation_fingerprint = baseline
            .conversation_fingerprint
            .as_ref()
            .map(|value| value.as_str().to_owned());
        let payload = json!({
            "baselineUserTurnBoundary": baseline_user_turn_boundary,
            "baselineConversationFingerprint": baseline_conversation_fingerprint,
        });
        let effect_id = {
            let mut journal = self.journal.lock().await;
            journal.begin("start_fresh_conversation", payload.to_string(), None, None)?
        };

        if let Err(error) = self.surface.start_fresh_conversation().await {
            let journal = self.journal.lock().await;
            journal.settle(effect_id, false, &error.to_string())?;
            return Err(error);
        }

        let observed = self.surface.observe().await?;
        if !fresh_conversation_postcondition(
            &observed,
            baseline_user_turn_boundary.as_deref(),
            baseline_conversation_fingerprint.as_deref(),
        ) {
            bail!(
                "fresh conversation action returned successfully but its semantic postcondition was not observed; durable effect remains pending for startup reconciliation"
            );
        }
        {
            let journal = self.journal.lock().await;
            journal.settle(
                effect_id,
                true,
                "fresh conversation semantic postcondition observed",
            )?;
        }
        *self.dispatch_id.lock().await = DispatchId::new(dispatch_marker()?);
        Ok(())
    }
}

struct SqliteReviewSettlement {
    path: PathBuf,
}

#[async_trait]
impl ReviewSettlementPort for SqliteReviewSettlement {
    async fn load_review_settlement(
        &self,
        key: &ReviewSettlementKey,
    ) -> Result<Option<DurableReviewSettlementState>> {
        let store = SqliteStore::open(&self.path)?;
        let phase = match key.phase {
            Phase::Work => "work",
            Phase::Review => "review",
        };
        Ok(store
            .review_settlement(
                &key.task_id,
                &key.run_id,
                phase,
                i64::from(key.round.get()),
                key.conversation_fingerprint.as_str(),
            )?
            .map(|record| DurableReviewSettlementState {
                progress_signature: record.progress_signature,
                no_final_since_unix_ms: record.no_final_since_unix_ms,
            }))
    }

    async fn store_review_settlement(
        &self,
        key: &ReviewSettlementKey,
        state: &DurableReviewSettlementState,
    ) -> Result<()> {
        let store = SqliteStore::open(&self.path)?;
        let phase = match key.phase {
            Phase::Work => "work",
            Phase::Review => "review",
        };
        store.store_review_settlement(
            &ReviewSettlementRecord {
                task_id: key.task_id.as_str().to_owned(),
                run_id: key.run_id.as_str().to_owned(),
                phase: phase.to_owned(),
                round: i64::from(key.round.get()),
                conversation_fingerprint: key.conversation_fingerprint.as_str().to_owned(),
                progress_signature: state.progress_signature.clone(),
                no_final_since_unix_ms: state.no_final_since_unix_ms,
            },
            unix_time_ms()?,
        )
    }

    async fn clear_review_settlement(&self, key: &ReviewSettlementKey) -> Result<()> {
        let store = SqliteStore::open(&self.path)?;
        let phase = match key.phase {
            Phase::Work => "work",
            Phase::Review => "review",
        };
        store.clear_review_settlement(
            &key.task_id,
            &key.run_id,
            phase,
            i64::from(key.round.get()),
            key.conversation_fingerprint.as_str(),
        )
    }
}

struct SqliteRunControl {
    path: PathBuf,
    task_id: TaskId,
}

#[async_trait]
impl RunControlPort for SqliteRunControl {
    async fn lifecycle(&self) -> Result<ContinuousTaskLifecycle> {
        let store = SqliteStore::open(&self.path)?;
        let Some(raw) = store.task_state_json(&self.task_id)? else {
            return Ok(ContinuousTaskLifecycle::Active);
        };
        let root: serde_json::Value =
            serde_json::from_str(&raw).context("parse durable task state JSON for run control")?;
        let Some(orchestration) = root.get("orchestration") else {
            return Ok(ContinuousTaskLifecycle::Active);
        };
        let state: ContinuousTaskState = serde_json::from_value(orchestration.clone())
            .context("parse durable orchestration state for run control")?;
        Ok(state.lifecycle)
    }
}

#[derive(Default)]
struct ForegroundGateState {
    exclusive_task: Option<String>,
    mutation_active: bool,
}

#[derive(Clone, Default)]
struct ForegroundGate {
    state: Arc<StdMutex<ForegroundGateState>>,
    notify: Arc<Notify>,
}

struct ForegroundMutationPermit {
    gate: ForegroundGate,
}

impl Drop for ForegroundMutationPermit {
    fn drop(&mut self) {
        let mut state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.mutation_active = false;
        drop(state);
        self.gate.notify.notify_waiters();
    }
}

impl ForegroundGate {
    async fn acquire_mutation(&self, task_id: &TaskId) -> ForegroundMutationPermit {
        loop {
            let notified = self.notify.notified();
            {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let allowed = state
                    .exclusive_task
                    .as_deref()
                    .is_none_or(|owner| owner == task_id.as_str());
                if allowed && !state.mutation_active {
                    state.mutation_active = true;
                    return ForegroundMutationPermit { gate: self.clone() };
                }
            }
            notified.await;
        }
    }

    async fn acquire_exclusive(&self, task_id: &TaskId) {
        loop {
            let notified = self.notify.notified();
            {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let compatible = state
                    .exclusive_task
                    .as_deref()
                    .is_none_or(|owner| owner == task_id.as_str());
                if compatible && !state.mutation_active {
                    state.exclusive_task = Some(task_id.as_str().to_owned());
                    return;
                }
            }
            notified.await;
        }
    }

    fn release_exclusive(&self, task_id: &TaskId) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.exclusive_task.as_deref() == Some(task_id.as_str()) {
            state.exclusive_task = None;
            drop(state);
            self.notify.notify_waiters();
        }
    }
}

#[derive(Clone)]
struct SupervisorHandle {
    sender: mpsc::Sender<SupervisorMessage>,
    state_db_path: PathBuf,
    foreground_gate: ForegroundGate,
    next_wake_token: Arc<AtomicU64>,
}

struct SupervisorSleepRequest {
    token: u64,
    task_id: TaskId,
    run_id: RunId,
    reason: String,
    requested_wake_at_unix_ms: i64,
    durable: bool,
    already_persisted: bool,
    reply: oneshot::Sender<Result<()>>,
}

struct SupervisorArmResult {
    token: u64,
    task_id: TaskId,
    result: std::result::Result<i64, String>,
}

enum SupervisorMessage {
    Sleep(SupervisorSleepRequest),
}

struct PendingWake {
    token: u64,
    task_id: TaskId,
    wake_at_unix_ms: i64,
    durable: bool,
    armed: bool,
    reply: oneshot::Sender<Result<()>>,
}

impl SupervisorHandle {
    fn spawn(state_db_path: PathBuf) -> Self {
        let (sender, receiver) = mpsc::channel(256);
        let (arm_sender, arm_receiver) = mpsc::unbounded_channel();
        let persistence_gate = Arc::new(Mutex::new(()));
        tokio::spawn(run_supervisor(
            receiver,
            arm_receiver,
            arm_sender,
            state_db_path.clone(),
            persistence_gate.clone(),
        ));
        Self {
            sender,
            state_db_path,
            foreground_gate: ForegroundGate::default(),
            next_wake_token: Arc::new(AtomicU64::new(1)),
        }
    }

    async fn sleep_for(
        &self,
        task_id: &TaskId,
        run_id: &RunId,
        reason: WakeReason,
        duration: Duration,
    ) -> Result<()> {
        if reason.holds_foreground() {
            self.foreground_gate.acquire_exclusive(task_id).await;
        } else {
            self.foreground_gate.release_exclusive(task_id);
        }
        let requested_wake_at_unix_ms =
            unix_time_ms()?.saturating_add(duration.as_millis().min(i64::MAX as u128) as i64);
        let token = self.next_wake_token.fetch_add(1, Ordering::Relaxed);
        let (reply_tx, reply_rx) = oneshot::channel();
        self.sender
            .send(SupervisorMessage::Sleep(SupervisorSleepRequest {
                token,
                task_id: task_id.clone(),
                run_id: run_id.clone(),
                reason: reason.as_str().to_owned(),
                requested_wake_at_unix_ms,
                durable: reason.is_durable(),
                already_persisted: false,
                reply: reply_tx,
            }))
            .await
            .map_err(|_| anyhow::anyhow!("fair supervisor stopped before wake registration"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("fair supervisor dropped wake waiter"))??;
        Ok(())
    }

    async fn wait_existing(&self, task_id: &TaskId, run_id: &RunId) -> Result<()> {
        let store = SqliteStore::open(&self.state_db_path)?;
        let Some(existing) = store.task_wake(task_id)? else {
            return Ok(());
        };
        if existing.run_id != run_id.as_str() {
            store.clear_task_wake(task_id)?;
            return Ok(());
        }
        let now = unix_time_ms()?;
        if existing.wake_at_unix_ms <= now {
            store.clear_task_wake(task_id)?;
            return Ok(());
        }
        if existing.reason == WakeReason::DispatchConfirmation.as_str() {
            self.foreground_gate.acquire_exclusive(task_id).await;
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        self.sender
            .send(SupervisorMessage::Sleep(SupervisorSleepRequest {
                token: self.next_wake_token.fetch_add(1, Ordering::Relaxed),
                task_id: task_id.clone(),
                run_id: RunId::new(existing.run_id),
                reason: existing.reason,
                requested_wake_at_unix_ms: existing.wake_at_unix_ms,
                durable: true,
                already_persisted: true,
                reply: reply_tx,
            }))
            .await
            .map_err(|_| anyhow::anyhow!("fair supervisor stopped while restoring durable wake"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("fair supervisor dropped durable wake waiter"))??;
        Ok(())
    }
    fn release_foreground(&self, task_id: &TaskId) {
        self.foreground_gate.release_exclusive(task_id);
    }
}

async fn run_supervisor(
    mut receiver: mpsc::Receiver<SupervisorMessage>,
    mut arm_receiver: mpsc::UnboundedReceiver<SupervisorArmResult>,
    arm_sender: mpsc::UnboundedSender<SupervisorArmResult>,
    state_db_path: PathBuf,
    persistence_gate: Arc<Mutex<()>>,
) {
    let mut pending: HashMap<String, PendingWake> = HashMap::new();
    let mut last_granted: Option<String> = None;

    loop {
        let now = unix_time_ms().unwrap_or(0);
        let blocked_on_arming = pending
            .values()
            .any(|wake| !wake.armed && wake.wake_at_unix_ms <= now);
        if !blocked_on_arming
            && let Some(selected) = select_due_task(&pending, now, last_granted.as_deref())
            && let Some(wake) = pending.remove(&selected)
        {
            if wake.durable
                && let Ok(store) = SqliteStore::open(&state_db_path)
            {
                let _ = store.clear_task_wake(&wake.task_id);
            }
            last_granted = Some(selected);
            let _ = wake.reply.send(Ok(()));
            tokio::task::yield_now().await;
            continue;
        }

        let next_deadline = pending.values().map(|wake| wake.wake_at_unix_ms).min();
        let sleep_duration = next_deadline
            .map(|deadline| {
                let now = unix_time_ms().unwrap_or(0);
                Duration::from_millis(deadline.saturating_sub(now).max(1) as u64)
            })
            .unwrap_or(Duration::from_secs(24 * 60 * 60));

        tokio::select! {
            message = receiver.recv() => {
                let Some(SupervisorMessage::Sleep(request)) = message else {
                    break;
                };
                register_supervisor_sleep(
                    &mut pending,
                    request,
                    &arm_sender,
                    &state_db_path,
                    &persistence_gate,
                );
            }
            result = arm_receiver.recv() => {
                if let Some(result) = result {
                    apply_supervisor_arm_result(&mut pending, result);
                }
            }
            _ = tokio::time::sleep(sleep_duration), if next_deadline.is_some() && !blocked_on_arming => {}
        }
    }

    for (_, wake) in pending {
        let _ = wake
            .reply
            .send(Err(anyhow::anyhow!("fair supervisor stopped")));
    }
}

fn select_due_task(
    pending: &HashMap<String, PendingWake>,
    now_unix_ms: i64,
    last_granted: Option<&str>,
) -> Option<String> {
    let mut due = pending
        .iter()
        .filter_map(|(task_id, wake)| {
            (wake.armed && wake.wake_at_unix_ms <= now_unix_ms).then_some(task_id.clone())
        })
        .collect::<Vec<_>>();
    due.sort();
    if due.is_empty() {
        return None;
    }
    Some(match last_granted {
        Some(last) => due
            .iter()
            .find(|task_id| task_id.as_str() > last)
            .cloned()
            .unwrap_or_else(|| due[0].clone()),
        None => due[0].clone(),
    })
}

fn register_supervisor_sleep(
    pending: &mut HashMap<String, PendingWake>,
    request: SupervisorSleepRequest,
    arm_sender: &mpsc::UnboundedSender<SupervisorArmResult>,
    state_db_path: &Path,
    persistence_gate: &Arc<Mutex<()>>,
) {
    let key = request.task_id.as_str().to_owned();
    let token = request.token;
    let task_id = request.task_id.clone();
    let durable = request.durable;
    let already_persisted = request.already_persisted;
    let run_id = request.run_id.clone();
    let reason = request.reason.clone();
    let requested_wake_at_unix_ms = request.requested_wake_at_unix_ms;
    if let Some(previous) = pending.insert(
        key,
        PendingWake {
            token,
            task_id: request.task_id,
            wake_at_unix_ms: requested_wake_at_unix_ms,
            durable,
            armed: !durable || already_persisted,
            reply: request.reply,
        },
    ) {
        let _ = previous.reply.send(Err(anyhow::anyhow!(
            "task registered a second wake before the previous wake completed"
        )));
    }
    if durable && !already_persisted {
        let arm_sender = arm_sender.clone();
        let path = state_db_path.to_path_buf();
        let persistence_gate = persistence_gate.clone();
        tokio::spawn(async move {
            let _persistence_permit = persistence_gate.lock().await;
            let task_id_for_store = task_id.clone();
            let result = tokio::task::spawn_blocking(move || {
                let now = unix_time_ms().map_err(|error| error.to_string())?;
                SqliteStore::open(&path)
                    .and_then(|mut store| {
                        store.arm_task_wake(
                            &task_id_for_store,
                            &run_id,
                            &reason,
                            requested_wake_at_unix_ms,
                            now,
                        )
                    })
                    .map(|record| record.wake_at_unix_ms)
                    .map_err(|error| error.to_string())
            })
            .await
            .unwrap_or_else(|error| Err(format!("durable wake persistence task failed: {error}")));
            let _ = arm_sender.send(SupervisorArmResult {
                token,
                task_id,
                result,
            });
        });
    }
}

fn apply_supervisor_arm_result(
    pending: &mut HashMap<String, PendingWake>,
    result: SupervisorArmResult,
) {
    let key = result.task_id.as_str().to_owned();
    let Some(current) = pending.get_mut(&key) else {
        return;
    };
    if current.token != result.token {
        return;
    }
    match result.result {
        Ok(wake_at_unix_ms) => {
            current.wake_at_unix_ms = wake_at_unix_ms;
            current.armed = true;
        }
        Err(message) => {
            if let Some(failed) = pending.remove(&key) {
                let _ = failed.reply.send(Err(anyhow::anyhow!(message)));
            }
        }
    }
}

struct SupervisorClock {
    origin: Instant,
    supervisor: SupervisorHandle,
    task_id: TaskId,
    run_id: RunId,
}

impl SupervisorClock {
    fn new(supervisor: SupervisorHandle, task_id: TaskId, run_id: RunId) -> Self {
        Self {
            origin: Instant::now(),
            supervisor,
            task_id,
            run_id,
        }
    }
}

#[async_trait]
impl Clock for SupervisorClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }

    async fn sleep(&self, duration: Duration) -> Result<()> {
        self.sleep_for(WakeReason::Poll, duration).await
    }

    async fn sleep_for(&self, reason: WakeReason, duration: Duration) -> Result<()> {
        self.supervisor
            .sleep_for(&self.task_id, &self.run_id, reason, duration)
            .await
    }
}

#[derive(Clone)]
struct RunWorkerIdentity {
    task_id: TaskId,
    run_id: RunId,
    dispatch_id: DispatchId,
    phase: Phase,
    round: Round,
}

pub struct RunWorker {
    surface: DurableRunSurface,
    review_settlement: SqliteReviewSettlement,
    run_control: SqliteRunControl,
    clock: SupervisorClock,
}

impl Drop for RunWorker {
    fn drop(&mut self) {
        self.clock
            .supervisor
            .release_foreground(&self.clock.task_id);
    }
}

impl RunWorker {
    fn new(
        surface: DesktopSessionActorHandle,
        state_db_path: &Path,
        identity: RunWorkerIdentity,
        supervisor: SupervisorHandle,
    ) -> Result<Self> {
        Ok(Self {
            surface: DurableRunSurface::new_with_identity(
                surface,
                state_db_path,
                DurableRunIdentity {
                    task_id: identity.task_id.clone(),
                    run_id: identity.run_id.clone(),
                    dispatch_id: identity.dispatch_id.clone(),
                    phase: identity.phase,
                    round: identity.round,
                },
                supervisor.clone(),
            )?,
            review_settlement: SqliteReviewSettlement {
                path: state_db_path.to_path_buf(),
            },
            run_control: SqliteRunControl {
                path: state_db_path.to_path_buf(),
                task_id: identity.task_id.clone(),
            },
            clock: SupervisorClock::new(supervisor, identity.task_id, identity.run_id),
        })
    }

    async fn lifecycle_report(&self) -> Result<Option<RunReport>> {
        let (state, message) = match self.run_control.lifecycle().await? {
            ContinuousTaskLifecycle::Active => return Ok(None),
            ContinuousTaskLifecycle::Paused => {
                (RunState::Paused, "continuous task paused in durable state")
            }
            ContinuousTaskLifecycle::Cancelled => (
                RunState::Cancelled,
                "continuous task cancelled in durable state",
            ),
        };
        Ok(Some(RunReport {
            state,
            conversation_ref: None,
            assistant_text: String::new(),
            approvals_clicked: 0,
            recoveries: 0,
            rate_limit_pauses: 0,
            dispatch_retries: 0,
            message: message.into(),
        }))
    }

    async fn wait_until_runnable(&self) -> Result<()> {
        self.clock
            .supervisor
            .wait_existing(&self.clock.task_id, &self.clock.run_id)
            .await
    }

    async fn execute(&self, prompt: &str, options: RunOptions) -> Result<RunReport> {
        RunPrompt::with_durable_ports(
            &self.surface,
            &self.clock,
            &self.review_settlement,
            &self.run_control,
        )
        .execute(prompt, options)
        .await
    }

    async fn resume_existing(&self, prompt: &str, options: RunOptions) -> Result<RunReport> {
        RunPrompt::with_durable_ports(
            &self.surface,
            &self.clock,
            &self.review_settlement,
            &self.run_control,
        )
        .resume_confirmed_dispatch(prompt, options)
        .await
    }
}

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

    async fn sleep(&self, duration: Duration) -> Result<()> {
        tokio::time::sleep(duration).await;
        Ok(())
    }
}

pub struct DesktopRuntime {
    process: Arc<dyn ChatProcessPort>,
    surface: Arc<dyn ChatSurfacePort>,
    desktop_session: OnceCell<DesktopSessionActorHandle>,
    supervisor: OnceCell<SupervisorHandle>,
    state_db_path: PathBuf,
}

impl Default for DesktopRuntime {
    fn default() -> Self {
        #[cfg(target_os = "linux")]
        {
            Self::new(
                ChatGptDesktopProcess::default(),
                ChatGptDesktopAtspi::default(),
            )
        }
        #[cfg(target_os = "macos")]
        {
            Self::new(ChatGptDesktopMacProcess, ChatGptDesktopMacSurface)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            compile_error!("ChatGPT desktop runtime currently supports Linux and macOS")
        }
    }
}

impl DesktopRuntime {
    pub fn new<P, S>(process: P, surface: S) -> Self
    where
        P: ChatProcessPort + 'static,
        S: ChatSurfacePort + 'static,
    {
        Self {
            process: Arc::new(process),
            surface: Arc::new(surface),
            desktop_session: OnceCell::new(),
            supervisor: OnceCell::new(),
            state_db_path: default_state_db_path(),
        }
    }

    pub fn with_state_db_path(mut self, path: PathBuf) -> Self {
        self.state_db_path = path;
        self
    }

    fn attachment_store_root(&self) -> PathBuf {
        if let Some(root) = std::env::var_os("FABUSHI_CHATGPT_ATTACHMENT_DIR") {
            return PathBuf::from(root);
        }
        self.state_db_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("attachments")
    }

    fn stage_task_attachments(
        &self,
        task_id: &TaskId,
        paths: &[PathBuf],
    ) -> Result<Vec<AttachmentId>> {
        if paths.is_empty() {
            return Ok(SqliteStore::open(&self.state_db_path)?
                .task_attachments(task_id)?
                .into_iter()
                .map(|record| record.attachment_id)
                .collect());
        }
        let attachment_store = AttachmentStore::open(self.attachment_store_root())?;
        let sqlite = SqliteStore::open(&self.state_db_path)?;
        let mut ids = Vec::with_capacity(paths.len());
        for path in paths {
            let bytes =
                std::fs::read(path).with_context(|| format!("read task attachment {path:?}"))?;
            let file_name = path
                .file_name()
                .and_then(|value| value.to_str())
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!("attachment path has no UTF-8 file name: {path:?}")
                })?;
            let content_sha = format!("{:x}", Sha256::digest(&bytes));
            let mut identity = Sha256::new();
            identity.update(task_id.as_str().as_bytes());
            identity.update([0]);
            identity.update(file_name.as_bytes());
            identity.update([0]);
            identity.update(content_sha.as_bytes());
            let attachment_id = AttachmentId::new(format!("attachment-{:x}", identity.finalize()));
            let stored =
                attachment_store.persist_bytes(attachment_id.clone(), file_name, &bytes)?;
            sqlite.upsert_attachment(&AttachmentRecord {
                attachment_id: attachment_id.clone(),
                task_id: task_id.clone(),
                file_name: stored.file_name,
                sha256: stored.sha256,
                storage_ref: stored.storage_ref.to_string_lossy().into_owned(),
                metadata_json: json!({ "byteLen": stored.byte_len }).to_string(),
            })?;
            ids.push(attachment_id);
        }
        Ok(ids)
    }

    #[cfg(test)]
    fn fail_closed_if_native_attachments_pending(&self, task_id: &TaskId) -> Result<()> {
        let attachments = SqliteStore::open(&self.state_db_path)?.task_attachments(task_id)?;
        if attachments.is_empty() {
            return Ok(());
        }
        bail!(
            "task has {} persisted attachment(s), but the desktop-native attachment effect/readiness adapter is not available yet; refusing text-only Send",
            attachments.len()
        )
    }

    async fn desktop_session(&self) -> Result<&DesktopSessionActorHandle> {
        self.desktop_session
            .get_or_try_init(|| async {
                DesktopSessionActorHandle::spawn_durable(self.surface.clone(), &self.state_db_path)
            })
            .await
    }

    async fn supervisor(&self) -> &SupervisorHandle {
        self.supervisor
            .get_or_init(|| async { SupervisorHandle::spawn(self.state_db_path.clone()) })
            .await
    }

    pub async fn ensure_ready(&self) -> Result<()> {
        self.process.ensure_running().await?;
        for _ in 0..40 {
            if self.process.health().await? == ChatProcessHealth::Running
                && let Ok(snapshot) = self.surface.observe().await
                && snapshot.app_healthy
            {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        bail!(
            "ChatGPT desktop process started but its semantic accessibility surface did not become ready"
        )
    }

    pub async fn snapshot(&self) -> Result<ChatSurfaceSnapshot> {
        self.ensure_ready().await?;
        self.surface
            .observe()
            .await
            .context("observe ChatGPT desktop semantic surface")
    }

    async fn ensure_reasoning_preset(
        &self,
        worker: &RunWorker,
        target: ReasoningPreset,
    ) -> Result<Option<RunReport>> {
        let surface = &worker.surface;
        let clock = &worker.clock;
        let mut gate = ReasoningGateState::default();
        loop {
            if let Some(report) = worker.lifecycle_report().await? {
                return Ok(Some(report));
            }
            let snapshot = surface.observe().await?;
            match gate.observe(&snapshot, target, clock.now()) {
                ReasoningDecision::Ready => return Ok(None),
                ReasoningDecision::Select(preset) => {
                    let changed = surface.set_reasoning_preset(preset).await?;
                    if changed && surface.observe().await?.selected_reasoning_preset == Some(target)
                    {
                        gate.selection_succeeded();
                        return Ok(None);
                    }
                    if gate.selection_failed(clock.now())
                        == ReasoningDecision::RecoverCurrentSurface
                    {
                        surface.recover_current_surface().await?;
                    }
                    clock
                        .sleep_for(WakeReason::ReasoningPicker, Duration::from_millis(500))
                        .await?;
                }
                ReasoningDecision::Wait => {
                    clock
                        .sleep_for(WakeReason::ReasoningPicker, Duration::from_millis(500))
                        .await?;
                }
                ReasoningDecision::RecoverCurrentSurface => {
                    surface.recover_current_surface().await?;
                    clock
                        .sleep_for(WakeReason::ReasoningPicker, Duration::from_millis(500))
                        .await?;
                }
            }
        }
    }

    async fn reconcile_unsettled_effects_before_run(&self) -> Result<()> {
        let surface = self.desktop_session().await?;
        let snapshot = surface.observe().await?;
        let mut store = SqliteStore::open(&self.state_db_path)?;
        let pending = store.pending_effects(STARTUP_PENDING_EFFECT_LIMIT)?;
        if pending.is_empty() {
            return Ok(());
        }

        let mut settled_any = false;
        for effect in pending {
            match reconcile_pending_effect(&mut store, &snapshot, &effect)? {
                StartupReconcileOutcome::Clear | StartupReconcileOutcome::DeferredToRunWorker => {}
                StartupReconcileOutcome::SettledObservedSend
                | StartupReconcileOutcome::SettledObservedReasoning
                | StartupReconcileOutcome::SettledObservedApproval
                | StartupReconcileOutcome::SettledObservedFreshConversation => {
                    settled_any = true;
                }
            }
        }

        let remaining = store.pending_effects(STARTUP_PENDING_EFFECT_LIMIT)?;
        let blocking = remaining
            .iter()
            .filter(|effect| effect.effect_kind != "attach_file")
            .collect::<Vec<_>>();
        if !blocking.is_empty() {
            let sample = blocking
                .iter()
                .take(4)
                .map(|effect| {
                    format!(
                        "{}:{}:{}",
                        effect.id, effect.effect_kind, effect.idempotency_key
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "unsettled destructive effects remain after startup reconciliation; refusing a new desktop mutation run to avoid duplicate effects: {sample}"
            );
        }

        let _ = settled_any;
        Ok(())
    }

    async fn resume_incomplete_continuous_phase(
        &self,
        state: &ContinuousTaskState,
        options: &RunOptions,
    ) -> Result<Option<(RunId, RunReport)>> {
        self.ensure_ready().await?;
        self.reconcile_unsettled_effects_before_run().await?;

        let candidate = {
            let store = SqliteStore::open(&self.state_db_path)?;
            store.latest_incomplete_continuous_phase(&state.task_id)?
        };
        let Some(candidate) = candidate else {
            return Ok(None);
        };
        if !incomplete_phase_matches_state(&candidate, state)? {
            return Ok(None);
        }

        let session = self.desktop_session().await?.clone();
        let snapshot = session.observe().await?;
        if !snapshot_matches_dispatch(&snapshot, &candidate.dispatch_id) {
            return Ok(None);
        }

        let phase = state.phase;
        let round = state.round;
        let prompt = match phase {
            Phase::Work => state.work_instruction(),
            Phase::Review => state.review_instruction()?,
        };
        let mut phase_options = options.clone();
        phase_options.run_phase = Some(phase);
        phase_options.run_round = Some(round);
        phase_options.review_identity = (phase == Phase::Review).then(|| ReviewRunIdentity {
            task_id: state.task_id.clone(),
            run_id: candidate.run_id.clone(),
            phase,
            round,
        });
        phase_options.recovery_context = Some(RecoveryRunContext {
            task_id: state.task_id.clone(),
            run_id: candidate.run_id.clone(),
            phase,
            round,
            goal_revision: state.goal_revision,
            authoritative_instruction: prompt.clone(),
            previous_work_result: state.previous_work_result.clone(),
            current_next: state.current_next.clone(),
            original_goal: state.goal.clone(),
            completed: state
                .previous_work_result
                .iter()
                .map(|value| format!("previous Work result: {value}"))
                .collect(),
            remaining: vec![
                state
                    .current_next
                    .clone()
                    .unwrap_or_else(|| state.goal.clone()),
            ],
            blockers: Vec::new(),
        });
        phase_options.expected_dispatch_id = Some(candidate.dispatch_id.clone());

        let worker = RunWorker::new(
            session,
            &self.state_db_path,
            RunWorkerIdentity {
                task_id: state.task_id.clone(),
                run_id: candidate.run_id.clone(),
                dispatch_id: candidate.dispatch_id,
                phase,
                round,
            },
            self.supervisor().await.clone(),
        )?;
        if let Some(report) = worker.lifecycle_report().await? {
            return Ok(Some((candidate.run_id, report)));
        }
        worker.wait_until_runnable().await?;
        if let Some(report) = worker.lifecycle_report().await? {
            return Ok(Some((candidate.run_id, report)));
        }
        let report = worker.resume_existing(&prompt, phase_options).await?;
        Ok(Some((candidate.run_id, report)))
    }

    fn load_continuous_task_state(&self, task_id: &TaskId) -> Result<Option<ContinuousTaskState>> {
        let store = SqliteStore::open(&self.state_db_path)?;
        let Some(raw) = store.task_state_json(task_id)? else {
            return Ok(None);
        };
        let root: serde_json::Value =
            serde_json::from_str(&raw).context("parse durable task state JSON")?;
        let Some(orchestration) = root.get("orchestration") else {
            return Ok(None);
        };
        serde_json::from_value(orchestration.clone())
            .map(Some)
            .context("parse durable continuous orchestration state")
    }

    fn persist_continuous_task_state(
        &self,
        state: &ContinuousTaskState,
        run_id: &RunId,
        event_kind: &str,
        event_payload: serde_json::Value,
    ) -> Result<()> {
        let mut store = SqliteStore::open(&self.state_db_path)?;
        let expected_revision = store.task_revision(&state.task_id)?.unwrap_or(0);
        let existing_state = store.task_state_json(&state.task_id)?;
        let materialized_state_json =
            merge_task_orchestration_state(existing_state.as_deref(), state)?;
        store.record_state_transition(
            &StateTransitionRecord {
                task_id: state.task_id.clone(),
                run_id: run_id.clone(),
                expected_revision,
                next_revision: expected_revision + 1,
                event_kind: event_kind.to_owned(),
                event_payload_json: event_payload.to_string(),
                materialized_state_json,
            },
            unix_time_ms()?,
        )
    }

    fn mutate_continuous_task_lifecycle(
        &self,
        task_id: &TaskId,
        lifecycle: ContinuousTaskLifecycle,
        event_kind: &str,
    ) -> Result<ContinuousTaskState> {
        let mut state = self
            .load_continuous_task_state(task_id)?
            .ok_or_else(|| anyhow::anyhow!("continuous task not found"))?;
        if state.completed {
            bail!("completed continuous task lifecycle cannot be changed");
        }
        state = match lifecycle {
            ContinuousTaskLifecycle::Active => state.resume(),
            ContinuousTaskLifecycle::Paused => state.pause(),
            ContinuousTaskLifecycle::Cancelled => state.cancel(),
        };
        let control_run = RunId::new(format!("control-{}", dispatch_marker()?));
        self.persist_continuous_task_state(
            &state,
            &control_run,
            event_kind,
            json!({"lifecycle": lifecycle}),
        )?;
        let store = SqliteStore::open(&self.state_db_path)?;
        store.clear_task_wake(task_id)?;
        if matches!(
            lifecycle,
            ContinuousTaskLifecycle::Active | ContinuousTaskLifecycle::Cancelled
        ) {
            store.clear_review_settlements_for_task(task_id)?;
        }
        Ok(state)
    }

    pub fn pause_continuous_task(&self, task_id: &TaskId) -> Result<ContinuousTaskState> {
        self.mutate_continuous_task_lifecycle(
            task_id,
            ContinuousTaskLifecycle::Paused,
            "continuous_task_paused",
        )
    }

    pub fn resume_continuous_task(&self, task_id: &TaskId) -> Result<ContinuousTaskState> {
        self.mutate_continuous_task_lifecycle(
            task_id,
            ContinuousTaskLifecycle::Active,
            "continuous_task_resumed",
        )
    }

    pub fn cancel_continuous_task(&self, task_id: &TaskId) -> Result<ContinuousTaskState> {
        self.mutate_continuous_task_lifecycle(
            task_id,
            ContinuousTaskLifecycle::Cancelled,
            "continuous_task_cancelled",
        )
    }

    pub async fn run_continuous(
        &self,
        task_id: TaskId,
        goal: &str,
        requested_reasoning: ReasoningPreset,
        options: RunOptions,
    ) -> Result<RunReport> {
        self.run_continuous_with_attachments(task_id, goal, requested_reasoning, options, &[])
            .await
    }

    pub async fn run_continuous_with_attachments(
        &self,
        task_id: TaskId,
        goal: &str,
        requested_reasoning: ReasoningPreset,
        options: RunOptions,
        attachment_paths: &[PathBuf],
    ) -> Result<RunReport> {
        self.stage_task_attachments(&task_id, attachment_paths)?;
        let loaded_state = self.load_continuous_task_state(&task_id)?;
        let resumed_existing = loaded_state.is_some();
        let mut state = match loaded_state {
            Some(existing) => {
                if existing.goal != goal {
                    bail!(
                        "durable task goal differs from the requested goal; edit-goal persistence must be used instead of silently replacing it"
                    );
                }
                if existing.reasoning_preset != requested_reasoning {
                    bail!(
                        "durable task reasoning preset differs from the requested preset; resume must keep the persisted preset"
                    );
                }
                existing
            }
            None => ContinuousTaskState::new(task_id.clone(), goal.to_owned(), requested_reasoning),
        };

        if state.lifecycle == ContinuousTaskLifecycle::Paused {
            return Ok(RunReport {
                state: RunState::Paused,
                conversation_ref: None,
                assistant_text: state.previous_work_result.clone().unwrap_or_default(),
                approvals_clicked: 0,
                recoveries: 0,
                rate_limit_pauses: 0,
                dispatch_retries: 0,
                message: "continuous task is paused in durable state".into(),
            });
        }
        if state.lifecycle == ContinuousTaskLifecycle::Cancelled {
            return Ok(RunReport {
                state: RunState::Cancelled,
                conversation_ref: None,
                assistant_text: state.previous_work_result.clone().unwrap_or_default(),
                approvals_clicked: 0,
                recoveries: 0,
                rate_limit_pauses: 0,
                dispatch_retries: 0,
                message: "continuous task is cancelled in durable state".into(),
            });
        }
        if state.completed {
            return Ok(RunReport {
                state: RunState::Complete,
                conversation_ref: None,
                assistant_text: state.previous_work_result.clone().unwrap_or_default(),
                approvals_clicked: 0,
                recoveries: 0,
                rate_limit_pauses: 0,
                dispatch_retries: 0,
                message: "continuous task is already complete in durable state".into(),
            });
        }

        if resumed_existing
            && let Some((run_id, report)) = self
                .resume_incomplete_continuous_phase(&state, &options)
                .await?
        {
            if report.state != RunState::Complete {
                return Ok(report);
            }

            let phase = state.phase;
            let round = state.round;
            match phase {
                Phase::Work => {
                    state = state.after_work_result(report.assistant_text.clone());
                    self.persist_continuous_task_state(
                        &state,
                        &run_id,
                        "continuous_work_result_saved",
                        json!({
                            "round": round,
                            "resultChars": report.assistant_text.chars().count(),
                            "recoveredAfterRestart": true,
                        }),
                    )?;
                }
                Phase::Review => {
                    let review =
                        parse_strict_review_report(&report.assistant_text, &state.task_id, round)?;
                    let status = review.status;
                    state = state.apply_review(review)?;
                    self.persist_continuous_task_state(
                        &state,
                        &run_id,
                        "continuous_review_applied",
                        json!({
                            "round": round,
                            "status": status,
                            "nextRound": state.round,
                            "completed": state.completed,
                            "recoveredAfterRestart": true,
                        }),
                    )?;
                    if state.completed {
                        return Ok(report);
                    }
                }
            }
        }

        loop {
            let phase = state.phase;
            let round = state.round;
            let run_marker = dispatch_marker()?;
            let run_id = RunId::new(format!("run-{run_marker}"));
            self.persist_continuous_task_state(
                &state,
                &run_id,
                "continuous_phase_started",
                json!({
                    "phase": phase,
                    "round": round,
                    "goalRevision": state.goal_revision,
                }),
            )?;

            let prompt = match phase {
                Phase::Work => state.work_instruction(),
                Phase::Review => state.review_instruction()?,
            };
            let mut phase_options = options.clone();
            phase_options.run_phase = Some(phase);
            phase_options.run_round = Some(round);
            phase_options.review_identity = (phase == Phase::Review).then(|| ReviewRunIdentity {
                task_id: task_id.clone(),
                run_id: run_id.clone(),
                phase,
                round,
            });
            phase_options.recovery_context = Some(RecoveryRunContext {
                task_id: task_id.clone(),
                run_id: run_id.clone(),
                phase,
                round,
                goal_revision: state.goal_revision,
                authoritative_instruction: prompt.clone(),
                previous_work_result: state.previous_work_result.clone(),
                current_next: state.current_next.clone(),
                original_goal: state.goal.clone(),
                completed: state
                    .previous_work_result
                    .iter()
                    .map(|value| format!("previous Work result: {value}"))
                    .collect(),
                remaining: vec![
                    state
                        .current_next
                        .clone()
                        .unwrap_or_else(|| state.goal.clone()),
                ],
                blockers: Vec::new(),
            });

            let report = self
                .run_prompt_with_identity(
                    &prompt,
                    state.reasoning_preset,
                    phase_options,
                    task_id.clone(),
                    run_id.clone(),
                    true,
                )
                .await?;
            if report.state != RunState::Complete {
                return Ok(report);
            }

            match phase {
                Phase::Work => {
                    state = state.after_work_result(report.assistant_text.clone());
                    self.persist_continuous_task_state(
                        &state,
                        &run_id,
                        "continuous_work_result_saved",
                        json!({
                            "round": round,
                            "resultChars": report.assistant_text.chars().count(),
                        }),
                    )?;
                }
                Phase::Review => {
                    let review =
                        parse_strict_review_report(&report.assistant_text, &task_id, round)?;
                    let status = review.status;
                    state = state.apply_review(review)?;
                    self.persist_continuous_task_state(
                        &state,
                        &run_id,
                        "continuous_review_applied",
                        json!({
                            "round": round,
                            "status": status,
                            "nextRound": state.round,
                            "completed": state.completed,
                        }),
                    )?;
                    if state.completed {
                        return Ok(report);
                    }
                }
            }
        }
    }

    pub async fn run_prompt(
        &self,
        prompt: &str,
        requested_reasoning: ReasoningPreset,
        options: RunOptions,
    ) -> Result<RunReport> {
        self.run_prompt_with_attachments(prompt, requested_reasoning, options, &[])
            .await
    }

    pub async fn run_prompt_with_attachments(
        &self,
        prompt: &str,
        requested_reasoning: ReasoningPreset,
        options: RunOptions,
        attachment_paths: &[PathBuf],
    ) -> Result<RunReport> {
        let identity_marker = dispatch_marker()?;
        let task_id = TaskId::new(format!("task-{identity_marker}"));
        self.stage_task_attachments(&task_id, attachment_paths)?;
        self.run_prompt_with_identity(
            prompt,
            requested_reasoning,
            options,
            task_id,
            RunId::new(format!("run-{identity_marker}")),
            false,
        )
        .await
    }

    async fn run_prompt_with_identity(
        &self,
        prompt: &str,
        requested_reasoning: ReasoningPreset,
        options: RunOptions,
        task_id: TaskId,
        run_id: RunId,
        start_fresh: bool,
    ) -> Result<RunReport> {
        self.ensure_ready().await?;
        self.reconcile_unsettled_effects_before_run().await?;

        let phase = options
            .run_phase
            .ok_or_else(|| anyhow::anyhow!("desktop run requires explicit run_phase"))?;
        let round = options
            .run_round
            .ok_or_else(|| anyhow::anyhow!("desktop run requires explicit run_round"))?;

        let marker = dispatch_marker()?;
        let dispatch_id = DispatchId::new(marker.clone());
        let mut options = options;
        if options.recovery_context.is_none() {
            options.recovery_context = Some(RecoveryRunContext {
                task_id: task_id.clone(),
                run_id: run_id.clone(),
                phase,
                round,
                goal_revision: GoalRevision::new(0),
                authoritative_instruction: prompt.to_owned(),
                previous_work_result: None,
                current_next: None,
                original_goal: prompt.to_owned(),
                completed: Vec::new(),
                remaining: vec![prompt.to_owned()],
                blockers: Vec::new(),
            });
        }
        let worker = RunWorker::new(
            self.desktop_session().await?.clone(),
            &self.state_db_path,
            RunWorkerIdentity {
                task_id,
                run_id,
                dispatch_id: dispatch_id.clone(),
                phase,
                round,
            },
            self.supervisor().await.clone(),
        )?;
        if let Some(report) = worker.lifecycle_report().await? {
            return Ok(report);
        }
        worker.wait_until_runnable().await?;
        if let Some(report) = worker.lifecycle_report().await? {
            return Ok(report);
        }
        if start_fresh {
            worker.surface.start_fresh_conversation().await?;
        }
        if let Some(report) = self
            .ensure_reasoning_preset(&worker, requested_reasoning)
            .await?
        {
            return Ok(report);
        }

        options.expected_dispatch_id = Some(dispatch_id);
        worker.execute(prompt, options).await
    }
}

fn incomplete_phase_matches_state(
    candidate: &IncompleteContinuousPhase,
    state: &ContinuousTaskState,
) -> Result<bool> {
    let payload: serde_json::Value = serde_json::from_str(&candidate.event_payload_json)
        .context("parse continuous phase-start event payload")?;
    let expected_phase = match state.phase {
        Phase::Work => "work",
        Phase::Review => "review",
    };
    Ok(
        payload.get("phase").and_then(serde_json::Value::as_str) == Some(expected_phase)
            && payload.get("round").and_then(serde_json::Value::as_u64)
                == Some(u64::from(state.round.get())),
    )
}

fn snapshot_matches_dispatch(snapshot: &ChatSurfaceSnapshot, dispatch_id: &DispatchId) -> bool {
    snapshot.current_dispatch_id.as_ref() == Some(dispatch_id)
        && snapshot.user_turn_ownership == OwnershipConfidence::Strong
}

fn merge_task_root(existing: Option<&str>) -> Result<serde_json::Map<String, serde_json::Value>> {
    let value = match existing {
        Some(raw) => serde_json::from_str::<serde_json::Value>(raw)
            .context("parse existing durable task state")?,
        None => json!({}),
    };
    value
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("durable task state must be a JSON object"))
}

fn merge_task_runtime_state(
    existing: Option<&str>,
    task_id: &TaskId,
    run_id: &RunId,
    revision: i64,
    effect_kind: &str,
) -> Result<String> {
    let mut root = merge_task_root(existing)?;
    root.insert(
        "runtime".into(),
        json!({
            "taskId": task_id.as_str(),
            "runId": run_id.as_str(),
            "revision": revision,
            "lastEffect": effect_kind,
        }),
    );
    Ok(serde_json::Value::Object(root).to_string())
}

fn merge_task_orchestration_state(
    existing: Option<&str>,
    state: &ContinuousTaskState,
) -> Result<String> {
    let mut root = merge_task_root(existing)?;
    root.insert(
        "orchestration".into(),
        serde_json::to_value(state).context("serialize continuous task state")?,
    );
    Ok(serde_json::Value::Object(root).to_string())
}

fn fresh_conversation_postcondition(
    snapshot: &ChatSurfaceSnapshot,
    baseline_user_turn_boundary: Option<&str>,
    baseline_conversation_fingerprint: Option<&str>,
) -> bool {
    if !snapshot.app_healthy
        || !snapshot.composer_ready
        || snapshot.current_dispatch_id.is_some()
        || snapshot.user_turn_boundary.is_some()
        || snapshot.assistant_response_boundary.is_some()
        || snapshot.streaming_or_busy
        || snapshot.stop_available
        || snapshot.authorization_surface_present
    {
        return false;
    }

    let current_conversation = snapshot
        .conversation_fingerprint
        .as_ref()
        .map(|value| value.as_str());
    if baseline_conversation_fingerprint.is_some()
        && current_conversation == baseline_conversation_fingerprint
    {
        return false;
    }
    let current_user_turn = snapshot
        .user_turn_boundary
        .as_ref()
        .map(|value| value.as_str());
    if baseline_user_turn_boundary.is_some() && current_user_turn == baseline_user_turn_boundary {
        return false;
    }
    true
}

fn reconcile_pending_effect(
    store: &mut SqliteStore,
    snapshot: &ChatSurfaceSnapshot,
    effect: &fabushi_chatgpt_sqlite_store::PendingEffect,
) -> Result<StartupReconcileOutcome> {
    if effect.effect_kind == "approve_current_conversation" {
        let payload: serde_json::Value = serde_json::from_str(&effect.effect_payload_json)
            .context("parse pending approval effect payload during startup reconciliation")?;
        let fingerprint = payload
            .get("fingerprint")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("pending approval effect is missing fingerprint"))?;
        let conversation_fingerprint = payload
            .get("conversationFingerprint")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("pending approval effect is missing conversationFingerprint")
            })?;
        let record = store.approval_fingerprint(fingerprint)?.ok_or_else(|| {
            anyhow::anyhow!("pending approval effect has no durable approval fingerprint")
        })?;

        let same_conversation = snapshot
            .conversation_fingerprint
            .as_ref()
            .map(|value| value.as_str())
            == Some(conversation_fingerprint);
        let now = unix_time_ms()?;
        let settlement_expired = record
            .settlement_until_unix_ms
            .is_some_and(|until| until <= now);
        if settlement_expired
            && same_conversation
            && !snapshot.authorization_surface_present
            && snapshot.authorization_settlement == AuthorizationSettlementState::Inactive
        {
            store.settle_effect(
                effect.id,
                &json!({
                    "ok": true,
                    "detail": "startup reconciliation observed approval postcondition after durable settlement window",
                    "conversationFingerprint": conversation_fingerprint,
                })
                .to_string(),
                now,
            )?;
            store.settle_approval_fingerprint(fingerprint)?;
            return Ok(StartupReconcileOutcome::SettledObservedApproval);
        }

        return Ok(StartupReconcileOutcome::Clear);
    }

    if effect.effect_kind == "attach_file" {
        let payload: serde_json::Value = serde_json::from_str(&effect.effect_payload_json)
            .context("parse pending attachment effect payload during startup reconciliation")?;
        payload
            .get("fileName")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("pending attachment effect is missing fileName"))?;
        payload
            .get("readinessDeadlineUnixMs")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| {
                anyhow::anyhow!("pending attachment effect is missing durable readiness deadline")
            })?;
        return Ok(StartupReconcileOutcome::DeferredToRunWorker);
    }

    if effect.effect_kind == "start_fresh_conversation" {
        let payload: serde_json::Value = serde_json::from_str(&effect.effect_payload_json)
            .context(
                "parse pending fresh-conversation effect payload during startup reconciliation",
            )?;
        let baseline_user_turn_boundary = payload
            .get("baselineUserTurnBoundary")
            .and_then(serde_json::Value::as_str);
        let baseline_conversation_fingerprint = payload
            .get("baselineConversationFingerprint")
            .and_then(serde_json::Value::as_str);
        if !fresh_conversation_postcondition(
            snapshot,
            baseline_user_turn_boundary,
            baseline_conversation_fingerprint,
        ) {
            return Ok(StartupReconcileOutcome::Clear);
        }
        store.settle_effect(
            effect.id,
            &json!({
                "ok": true,
                "detail": "startup reconciliation observed fresh-conversation postcondition",
                "baselineUserTurnBoundary": baseline_user_turn_boundary,
                "baselineConversationFingerprint": baseline_conversation_fingerprint,
            })
            .to_string(),
            unix_time_ms()?,
        )?;
        return Ok(StartupReconcileOutcome::SettledObservedFreshConversation);
    }

    if effect.effect_kind == "set_reasoning" {
        let payload: serde_json::Value = serde_json::from_str(&effect.effect_payload_json)
            .context("parse pending reasoning effect payload during startup reconciliation")?;
        let preset_index = payload
            .get("preset")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u8::try_from(value).ok())
            .ok_or_else(|| anyhow::anyhow!("pending reasoning effect is missing preset"))?;
        let target = ReasoningPreset::from_index(preset_index)
            .ok_or_else(|| anyhow::anyhow!("pending reasoning effect has invalid preset"))?;
        if snapshot.selected_reasoning_preset != Some(target) {
            return Ok(StartupReconcileOutcome::Clear);
        }
        store.settle_effect(
            effect.id,
            &json!({
                "ok": true,
                "detail": "startup reconciliation re-observed requested reasoning preset",
                "preset": preset_index,
            })
            .to_string(),
            unix_time_ms()?,
        )?;
        return Ok(StartupReconcileOutcome::SettledObservedReasoning);
    }

    if effect.effect_kind != "send_prompt" {
        return Ok(StartupReconcileOutcome::Clear);
    }

    let payload: serde_json::Value = serde_json::from_str(&effect.effect_payload_json)
        .context("parse pending Send effect payload during startup reconciliation")?;
    let dispatch_id = payload
        .get("dispatchId")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("pending Send effect is missing dispatchId"))?;
    let baseline = payload
        .get("baselineUserTurnBoundary")
        .and_then(serde_json::Value::as_str);

    let observed_boundary = snapshot
        .user_turn_boundary
        .as_ref()
        .map(|value| value.as_str());
    let confirmed = snapshot
        .current_dispatch_id
        .as_ref()
        .map(|value| value.as_str())
        == Some(dispatch_id)
        && snapshot.user_turn_ownership == OwnershipConfidence::Strong
        && observed_boundary.is_some()
        && observed_boundary != baseline;

    if !confirmed {
        return Ok(StartupReconcileOutcome::Clear);
    }

    let settlement = json!({
        "ok": true,
        "detail": "startup reconciliation observed dispatch marker and new strong user turn",
        "confirmed": true,
        "userTurnBoundary": observed_boundary,
        "conversationFingerprint": snapshot.conversation_fingerprint.as_ref().map(|value| value.as_str()),
    })
    .to_string();
    store.settle_confirmed_dispatch_effect(
        effect.id,
        &TaskId::new(effect.task_id.clone()),
        &RunId::new(effect.run_id.clone()),
        &DispatchId::new(dispatch_id),
        &settlement,
        unix_time_ms()?,
    )?;
    Ok(StartupReconcileOutcome::SettledObservedSend)
}

fn attachment_root_for_state_db(state_db_path: &Path) -> PathBuf {
    if let Some(root) = std::env::var_os("FABUSHI_CHATGPT_ATTACHMENT_DIR") {
        return PathBuf::from(root);
    }
    state_db_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("attachments")
}

fn default_state_db_path() -> PathBuf {
    if let Some(path) = std::env::var_os("FABUSHI_CHATGPT_STATE_DB") {
        return PathBuf::from(path);
    }
    if let Some(root) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(root)
            .join("fabushi")
            .join("chatgpt-auto-confirm")
            .join("state.sqlite3");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("fabushi")
            .join("chatgpt-auto-confirm")
            .join("state.sqlite3");
    }
    PathBuf::from(".fabushi-chatgpt-auto-confirm-state.sqlite3")
}

fn unix_time_ms() -> Result<i64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?;
    i64::try_from(elapsed.as_millis()).context("Unix timestamp does not fit i64")
}

fn dispatch_marker() -> Result<String> {
    let mut bytes = [0_u8; 16];
    std::fs::File::open("/dev/urandom")
        .context("open /dev/urandom for Fabushi dispatch marker")?
        .read_exact(&mut bytes)
        .context("read Fabushi dispatch marker entropy")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
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

#[cfg(test)]
mod actor_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::Notify;

    #[derive(Default)]
    struct FakeSurface {
        active_mutations: AtomicUsize,
        max_active_mutations: AtomicUsize,
        send_mutations: AtomicUsize,
    }

    impl FakeSurface {
        async fn mutation(&self) {
            let active = self.active_mutations.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active_mutations
                .fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(25)).await;
            self.active_mutations.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl ChatSurfacePort for FakeSurface {
        async fn observe(&self) -> Result<ChatSurfaceSnapshot> {
            Ok(ChatSurfaceSnapshot::default())
        }

        async fn set_reasoning_preset(&self, _preset: ReasoningPreset) -> Result<bool> {
            self.mutation().await;
            Ok(true)
        }

        async fn send_prompt(&self, _prompt: &str) -> Result<()> {
            self.send_mutations.fetch_add(1, Ordering::SeqCst);
            self.mutation().await;
            Ok(())
        }

        async fn approve_current_conversation(&self) -> Result<bool> {
            self.mutation().await;
            Ok(true)
        }

        async fn dismiss_rate_limit_notice(&self) -> Result<bool> {
            self.mutation().await;
            Ok(true)
        }

        async fn recover_current_surface(&self) -> Result<()> {
            self.mutation().await;
            Ok(())
        }

        async fn start_fresh_conversation(&self) -> Result<()> {
            self.mutation().await;
            Ok(())
        }
    }

    #[tokio::test]
    async fn durable_desktop_session_actor_fences_competing_owner() {
        let path = std::env::temp_dir().join(format!(
            "fabushi-chatgpt-ui-lease-{}-{}.sqlite3",
            std::process::id(),
            dispatch_marker().unwrap()
        ));
        let first_surface: Arc<dyn ChatSurfacePort> = Arc::new(FakeSurface::default());
        let first = DesktopSessionActorHandle::spawn_durable(first_surface, &path).unwrap();

        let second_surface: Arc<dyn ChatSurfacePort> = Arc::new(FakeSurface::default());
        let error = DesktopSessionActorHandle::spawn_durable(second_surface, &path)
            .err()
            .expect("competing actor must be fenced");
        assert!(
            error
                .to_string()
                .contains("owned by another Fabushi process")
        );

        drop(first);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let third_surface: Arc<dyn ChatSurfacePort> = Arc::new(FakeSurface::default());
        let third = DesktopSessionActorHandle::spawn_durable(third_surface, &path).unwrap();
        drop(third);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[derive(Default)]
    struct BlockingReasoningSurface {
        entered: Notify,
        release: Notify,
        selected: AtomicBool,
    }

    #[async_trait]
    impl ChatSurfacePort for BlockingReasoningSurface {
        async fn observe(&self) -> Result<ChatSurfaceSnapshot> {
            Ok(ChatSurfaceSnapshot {
                reasoning_picker_available: true,
                selected_reasoning_preset: self
                    .selected
                    .load(Ordering::SeqCst)
                    .then_some(ReasoningPreset::ExtraHigh),
                ..Default::default()
            })
        }

        async fn set_reasoning_preset(&self, preset: ReasoningPreset) -> Result<bool> {
            assert_eq!(preset, ReasoningPreset::ExtraHigh);
            self.entered.notify_one();
            self.release.notified().await;
            self.selected.store(true, Ordering::SeqCst);
            Ok(true)
        }

        async fn send_prompt(&self, _prompt: &str) -> Result<()> {
            Ok(())
        }
        async fn approve_current_conversation(&self) -> Result<bool> {
            Ok(false)
        }
        async fn dismiss_rate_limit_notice(&self) -> Result<bool> {
            Ok(false)
        }
        async fn recover_current_surface(&self) -> Result<()> {
            Ok(())
        }
        async fn start_fresh_conversation(&self) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn durable_reasoning_selection_is_journaled_before_ui_mutation() {
        let fake = Arc::new(BlockingReasoningSurface::default());
        let actor_surface: Arc<dyn ChatSurfacePort> = fake.clone();
        let actor = DesktopSessionActorHandle::spawn(actor_surface);
        let path = std::env::temp_dir().join(format!(
            "fabushi-chatgpt-reasoning-journal-{}-{}.sqlite3",
            std::process::id(),
            dispatch_marker().unwrap()
        ));
        let surface = DurableRunSurface::new(
            actor,
            &path,
            TaskId::new("task-reasoning-journal"),
            RunId::new("run-reasoning-journal"),
            DispatchId::new("dispatch-reasoning-journal"),
            Phase::Work,
            Round::new(1),
        )
        .unwrap();
        let selecting = {
            let surface = surface.clone();
            tokio::spawn(async move {
                surface
                    .set_reasoning_preset(ReasoningPreset::ExtraHigh)
                    .await
            })
        };
        fake.entered.notified().await;
        let store = SqliteStore::open(&path).unwrap();
        let pending = store.pending_effects(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].effect_kind, "set_reasoning");
        assert_eq!(pending[0].attempt_count, 1);
        fake.release.notify_one();
        assert!(selecting.await.unwrap().unwrap());
        assert!(store.pending_effects(10).unwrap().is_empty());
        drop(surface);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[derive(Default)]
    struct ApprovalAcceptingSurface;

    #[async_trait]
    impl ChatSurfacePort for ApprovalAcceptingSurface {
        async fn observe(&self) -> Result<ChatSurfaceSnapshot> {
            Ok(ChatSurfaceSnapshot {
                conversation_fingerprint: Some(ConversationFingerprint::new(
                    "conversation-approval",
                )),
                authorization_surface_present: true,
                authorization_actionable: true,
                ..Default::default()
            })
        }

        async fn send_prompt(&self, _prompt: &str) -> Result<()> {
            Ok(())
        }

        async fn approve_current_conversation(&self) -> Result<bool> {
            Ok(true)
        }

        async fn dismiss_rate_limit_notice(&self) -> Result<bool> {
            Ok(false)
        }

        async fn recover_current_surface(&self) -> Result<()> {
            Ok(())
        }

        async fn start_fresh_conversation(&self) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn adapter_true_does_not_settle_approval_effect_and_persists_identity_latch() {
        let actor_surface: Arc<dyn ChatSurfacePort> = Arc::new(ApprovalAcceptingSurface);
        let actor = DesktopSessionActorHandle::spawn(actor_surface);
        let path = std::env::temp_dir().join(format!(
            "fabushi-chatgpt-approval-journal-{}-{}.sqlite3",
            std::process::id(),
            dispatch_marker().unwrap()
        ));
        let surface = DurableRunSurface::new(
            actor,
            &path,
            TaskId::new("task-approval"),
            RunId::new("run-approval"),
            DispatchId::new("dispatch-approval"),
            Phase::Review,
            Round::new(9),
        )
        .unwrap();

        assert!(surface.approve_current_conversation().await.unwrap());

        let store = SqliteStore::open(&path).unwrap();
        let pending = store.pending_effects(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].effect_kind, "approve_current_conversation");
        let payload: serde_json::Value =
            serde_json::from_str(&pending[0].effect_payload_json).unwrap();
        assert_eq!(payload["taskId"], "task-approval");
        assert_eq!(payload["runId"], "run-approval");
        assert_eq!(payload["phase"], "review");
        assert_eq!(payload["round"], 9);
        assert_eq!(payload["conversationFingerprint"], "conversation-approval");
        let fingerprint = payload["fingerprint"].as_str().unwrap();
        let approval = store.approval_fingerprint(fingerprint).unwrap().unwrap();
        assert_eq!(approval.state, "settling");
        assert!(approval.settlement_until_unix_ms.unwrap() > unix_time_ms().unwrap());

        let projected = surface.observe().await.unwrap();
        assert_eq!(
            projected.authorization_settlement,
            AuthorizationSettlementState::Settling
        );
        assert_eq!(store.pending_effects(10).unwrap().len(), 1);

        drop(surface);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[tokio::test]
    async fn durable_approval_latch_restores_settling_after_run_surface_restart() {
        let actor_surface: Arc<dyn ChatSurfacePort> = Arc::new(ApprovalAcceptingSurface);
        let actor = DesktopSessionActorHandle::spawn(actor_surface);
        let path = std::env::temp_dir().join(format!(
            "fabushi-chatgpt-approval-restart-{}-{}.sqlite3",
            std::process::id(),
            dispatch_marker().unwrap()
        ));
        let task_id = TaskId::new("task-approval-restart");
        let run_id = RunId::new("run-approval-restart");

        let first = DurableRunSurface::new(
            actor.clone(),
            &path,
            task_id.clone(),
            run_id.clone(),
            DispatchId::new("dispatch-approval-restart"),
            Phase::Work,
            Round::new(4),
        )
        .unwrap();
        assert!(first.approve_current_conversation().await.unwrap());
        drop(first);

        let restored = DurableRunSurface::new(
            actor,
            &path,
            task_id,
            run_id,
            DispatchId::new("dispatch-approval-restart"),
            Phase::Work,
            Round::new(4),
        )
        .unwrap();
        let snapshot = restored.observe().await.unwrap();
        assert_eq!(
            snapshot.authorization_settlement,
            AuthorizationSettlementState::Settling
        );
        let store = SqliteStore::open(&path).unwrap();
        assert_eq!(store.pending_effects(10).unwrap().len(), 1);

        drop(restored);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[derive(Default)]
    struct BlockingSendSurface {
        entered: Notify,
        release: Notify,
        sent: AtomicBool,
    }

    #[async_trait]
    impl ChatSurfacePort for BlockingSendSurface {
        async fn observe(&self) -> Result<ChatSurfaceSnapshot> {
            if self.sent.load(Ordering::SeqCst) {
                Ok(ChatSurfaceSnapshot {
                    user_turn_boundary: Some(UserTurnBoundary::new("u1")),
                    current_dispatch_id: Some(DispatchId::new("dispatch-journal")),
                    user_turn_ownership: OwnershipConfidence::Strong,
                    ..Default::default()
                })
            } else {
                Ok(ChatSurfaceSnapshot {
                    user_turn_boundary: Some(UserTurnBoundary::new("u0")),
                    ..Default::default()
                })
            }
        }

        async fn send_prompt(&self, _prompt: &str) -> Result<()> {
            self.entered.notify_one();
            self.release.notified().await;
            self.sent.store(true, Ordering::SeqCst);
            Ok(())
        }

        async fn approve_current_conversation(&self) -> Result<bool> {
            Ok(false)
        }

        async fn dismiss_rate_limit_notice(&self) -> Result<bool> {
            Ok(false)
        }

        async fn recover_current_surface(&self) -> Result<()> {
            Ok(())
        }

        async fn start_fresh_conversation(&self) -> Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct DispatchRotationSurface {
        prompts: std::sync::Mutex<Vec<String>>,
        fresh: AtomicBool,
    }

    #[async_trait]
    impl ChatSurfacePort for DispatchRotationSurface {
        async fn observe(&self) -> Result<ChatSurfaceSnapshot> {
            if self.fresh.load(Ordering::SeqCst) {
                Ok(ChatSurfaceSnapshot {
                    app_healthy: true,
                    composer_ready: true,
                    ..Default::default()
                })
            } else {
                Ok(ChatSurfaceSnapshot {
                    app_healthy: true,
                    composer_ready: true,
                    user_turn_boundary: Some(UserTurnBoundary::new("baseline")),
                    ..Default::default()
                })
            }
        }

        async fn send_prompt(&self, prompt: &str) -> Result<()> {
            self.prompts.lock().unwrap().push(prompt.to_owned());
            Ok(())
        }

        async fn approve_current_conversation(&self) -> Result<bool> {
            Ok(false)
        }

        async fn dismiss_rate_limit_notice(&self) -> Result<bool> {
            Ok(false)
        }

        async fn recover_current_surface(&self) -> Result<()> {
            Ok(())
        }

        async fn start_fresh_conversation(&self) -> Result<()> {
            self.fresh.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn fresh_handoff_rotates_durable_dispatch_identity_and_visible_marker() {
        let path = std::env::temp_dir().join(format!(
            "fabushi-chatgpt-dispatch-rotation-{}-{}.sqlite3",
            std::process::id(),
            dispatch_marker().unwrap()
        ));
        let fake = Arc::new(DispatchRotationSurface::default());
        let actor_surface: Arc<dyn ChatSurfacePort> = fake.clone();
        let actor = DesktopSessionActorHandle::spawn(actor_surface);
        let surface = DurableRunSurface::new(
            actor,
            &path,
            TaskId::new("task-dispatch-rotation"),
            RunId::new("run-dispatch-rotation"),
            DispatchId::new("dispatch-initial"),
            Phase::Work,
            Round::new(1),
        )
        .unwrap();

        surface.send_prompt("hello").await.unwrap();
        let first_effect = SqliteStore::open(&path)
            .unwrap()
            .pending_effects(10)
            .unwrap()
            .into_iter()
            .find(|effect| effect.effect_kind == "send_prompt")
            .unwrap();
        let first_payload: serde_json::Value =
            serde_json::from_str(&first_effect.effect_payload_json).unwrap();
        let first_dispatch = first_payload["dispatchId"].as_str().unwrap().to_owned();

        surface.start_fresh_conversation().await.unwrap();
        surface.send_prompt("hello").await.unwrap();

        let second_effect = SqliteStore::open(&path)
            .unwrap()
            .pending_effects(10)
            .unwrap()
            .into_iter()
            .find(|effect| effect.effect_kind == "send_prompt")
            .unwrap();
        let second_payload: serde_json::Value =
            serde_json::from_str(&second_effect.effect_payload_json).unwrap();
        let second_dispatch = second_payload["dispatchId"].as_str().unwrap().to_owned();

        assert_ne!(first_dispatch, second_dispatch);
        let prompts = fake.prompts.lock().unwrap();
        assert_eq!(prompts.len(), 2);
        assert!(prompts[0].contains(&format!("[Fabushi:{first_dispatch}]")));
        assert!(prompts[1].contains(&format!("[Fabushi:{second_dispatch}]")));

        drop(surface);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[tokio::test]
    async fn run_worker_journals_destructive_effect_before_settlement() {
        let path = std::env::temp_dir().join(format!(
            "fabushi-chatgpt-effect-journal-{}-{}.sqlite3",
            std::process::id(),
            dispatch_marker().unwrap()
        ));
        let fake = Arc::new(BlockingSendSurface::default());
        let actor_surface: Arc<dyn ChatSurfacePort> = fake.clone();
        let actor = DesktopSessionActorHandle::spawn(actor_surface);
        let task_id = TaskId::new("task-journal");
        let run_id = RunId::new("run-journal");
        let surface = DurableRunSurface::new(
            actor,
            &path,
            task_id.clone(),
            run_id.clone(),
            DispatchId::new("dispatch-journal"),
            Phase::Work,
            Round::new(1),
        )
        .unwrap();
        let sending = {
            let surface = surface.clone();
            tokio::spawn(async move { surface.send_prompt("hello").await })
        };

        fake.entered.notified().await;
        let store = SqliteStore::open(&path).unwrap();
        assert_eq!(store.task_revision(&task_id).unwrap(), Some(1));
        let pending = store.pending_effects(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].effect_kind, "send_prompt");
        assert_eq!(pending[0].attempt_count, 1);

        fake.release.notify_one();
        sending.await.unwrap().unwrap();
        assert!(store.pending_effects(10).unwrap().is_empty());
        assert!(
            store
                .dispatch_attempt_settlement(
                    &task_id,
                    &run_id,
                    &DispatchId::new("dispatch-journal"),
                )
                .unwrap()
                .is_some()
        );

        drop(surface);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    fn create_pending_send(
        store: &mut SqliteStore,
        task_id: &TaskId,
        run_id: &RunId,
        dispatch_id: &str,
        baseline: &str,
    ) {
        store
            .record_transition(
                &TransitionRecord {
                    task_id: task_id.clone(),
                    run_id: run_id.clone(),
                    expected_revision: 0,
                    next_revision: 1,
                    event_kind: "send_prompt_prepared".into(),
                    event_payload_json: json!({
                        "preparedPrompt": "hello",
                        "dispatchId": dispatch_id,
                        "baselineUserTurnBoundary": baseline,
                    })
                    .to_string(),
                    materialized_state_json: json!({"revision": 1}).to_string(),
                    effect_kind: "send_prompt".into(),
                    effect_payload_json: json!({
                        "preparedPrompt": "hello",
                        "dispatchId": dispatch_id,
                        "baselineUserTurnBoundary": baseline,
                    })
                    .to_string(),
                    idempotency_key: format!("send-{dispatch_id}"),
                    prepared_dispatch: Some(PreparedDispatch {
                        dispatch_id: DispatchId::new(dispatch_id),
                        prepared_intent_json: json!({
                            "preparedPrompt": "hello",
                            "dispatchId": dispatch_id,
                        })
                        .to_string(),
                    }),
                    prepared_approval: None,
                },
                100,
            )
            .unwrap();
        let pending = store.pending_effects(10).unwrap();
        store.mark_effect_attempted(pending[0].id).unwrap();
    }

    fn create_pending_fresh_conversation(
        store: &mut SqliteStore,
        task_id: &TaskId,
        run_id: &RunId,
        baseline_user_turn: Option<&str>,
        baseline_conversation: Option<&str>,
    ) {
        let payload = json!({
            "baselineUserTurnBoundary": baseline_user_turn,
            "baselineConversationFingerprint": baseline_conversation,
        })
        .to_string();
        store
            .record_transition(
                &TransitionRecord {
                    task_id: task_id.clone(),
                    run_id: run_id.clone(),
                    expected_revision: 0,
                    next_revision: 1,
                    event_kind: "start_fresh_conversation_prepared".into(),
                    event_payload_json: payload.clone(),
                    materialized_state_json: json!({"revision": 1}).to_string(),
                    effect_kind: "start_fresh_conversation".into(),
                    effect_payload_json: payload,
                    idempotency_key: "fresh-conversation-test".into(),
                    prepared_dispatch: None,
                    prepared_approval: None,
                },
                100,
            )
            .unwrap();
        let effect = store.pending_effects(10).unwrap().remove(0);
        store.mark_effect_attempted(effect.id).unwrap();
    }

    fn create_pending_reasoning(
        store: &mut SqliteStore,
        task_id: &TaskId,
        run_id: &RunId,
        preset: ReasoningPreset,
    ) {
        store
            .record_transition(
                &TransitionRecord {
                    task_id: task_id.clone(),
                    run_id: run_id.clone(),
                    expected_revision: 0,
                    next_revision: 1,
                    event_kind: "set_reasoning_prepared".into(),
                    event_payload_json: json!({"preset": preset.index()}).to_string(),
                    materialized_state_json: json!({"revision": 1}).to_string(),
                    effect_kind: "set_reasoning".into(),
                    effect_payload_json: json!({"preset": preset.index()}).to_string(),
                    idempotency_key: format!("reasoning-{}", preset.index()),
                    prepared_dispatch: None,
                    prepared_approval: None,
                },
                100,
            )
            .unwrap();
        let pending = store.pending_effects(10).unwrap();
        store.mark_effect_attempted(pending[0].id).unwrap();
    }

    fn create_pending_approval(
        store: &mut SqliteStore,
        task_id: &TaskId,
        run_id: &RunId,
        fingerprint: &str,
        conversation_fingerprint: &str,
        settlement_until_unix_ms: i64,
    ) {
        store
            .record_transition(
                &TransitionRecord {
                    task_id: task_id.clone(),
                    run_id: run_id.clone(),
                    expected_revision: 0,
                    next_revision: 1,
                    event_kind: "approve_current_conversation_prepared".into(),
                    event_payload_json: json!({
                        "fingerprint": fingerprint,
                        "taskId": task_id.as_str(),
                        "runId": run_id.as_str(),
                        "phase": "work",
                        "round": 2,
                        "conversationFingerprint": conversation_fingerprint,
                    })
                    .to_string(),
                    materialized_state_json: json!({"revision": 1}).to_string(),
                    effect_kind: "approve_current_conversation".into(),
                    effect_payload_json: json!({
                        "fingerprint": fingerprint,
                        "taskId": task_id.as_str(),
                        "runId": run_id.as_str(),
                        "phase": "work",
                        "round": 2,
                        "conversationFingerprint": conversation_fingerprint,
                    })
                    .to_string(),
                    idempotency_key: format!("approval-{fingerprint}"),
                    prepared_dispatch: None,
                    prepared_approval: Some(PreparedApproval {
                        fingerprint: fingerprint.to_owned(),
                        phase: "work".into(),
                        round: 2,
                        conversation_fingerprint: conversation_fingerprint.to_owned(),
                    }),
                },
                100,
            )
            .unwrap();
        store
            .arm_approval_settlement(fingerprint, settlement_until_unix_ms)
            .unwrap();
        let effect = store.pending_effects(10).unwrap().remove(0);
        store.mark_effect_attempted(effect.id).unwrap();
    }

    #[test]
    fn startup_reconciliation_settles_fresh_conversation_only_from_clean_postcondition() {
        let mut store = SqliteStore::in_memory().unwrap();
        let task_id = TaskId::new("task-fresh-reconcile");
        let run_id = RunId::new("run-fresh-reconcile");
        create_pending_fresh_conversation(
            &mut store,
            &task_id,
            &run_id,
            Some("u-before"),
            Some("conversation-before"),
        );
        let snapshot = ChatSurfaceSnapshot {
            app_healthy: true,
            composer_ready: true,
            ..Default::default()
        };
        let effect = store.pending_effects(10).unwrap().remove(0);
        assert_eq!(
            reconcile_pending_effect(&mut store, &snapshot, &effect).unwrap(),
            StartupReconcileOutcome::SettledObservedFreshConversation
        );
        assert!(store.pending_effects(10).unwrap().is_empty());
    }

    #[test]
    fn startup_reconciliation_keeps_fresh_conversation_pending_when_old_owned_turn_remains() {
        let mut store = SqliteStore::in_memory().unwrap();
        let task_id = TaskId::new("task-fresh-ambiguous");
        let run_id = RunId::new("run-fresh-ambiguous");
        create_pending_fresh_conversation(
            &mut store,
            &task_id,
            &run_id,
            Some("u-before"),
            Some("conversation-before"),
        );
        let snapshot = ChatSurfaceSnapshot {
            app_healthy: true,
            composer_ready: true,
            user_turn_boundary: Some(UserTurnBoundary::new("u-before")),
            conversation_fingerprint: Some(ConversationFingerprint::new("conversation-before")),
            ..Default::default()
        };
        let effect = store.pending_effects(10).unwrap().remove(0);
        assert_eq!(
            reconcile_pending_effect(&mut store, &snapshot, &effect).unwrap(),
            StartupReconcileOutcome::Clear
        );
        assert_eq!(store.pending_effects(10).unwrap().len(), 1);
    }

    #[test]
    fn startup_reconciliation_never_replays_or_settles_ambiguous_active_approval() {
        let mut store = SqliteStore::in_memory().unwrap();
        let task_id = TaskId::new("task-approval-ambiguous");
        let run_id = RunId::new("run-approval-ambiguous");
        let now = unix_time_ms().unwrap();
        create_pending_approval(
            &mut store,
            &task_id,
            &run_id,
            "fp-active",
            "conversation-active",
            now + 12_000,
        );
        let snapshot = ChatSurfaceSnapshot {
            conversation_fingerprint: Some(ConversationFingerprint::new("conversation-active")),
            ..Default::default()
        };
        let effect = store.pending_effects(10).unwrap().remove(0);

        assert_eq!(
            reconcile_pending_effect(&mut store, &snapshot, &effect).unwrap(),
            StartupReconcileOutcome::Clear
        );
        assert_eq!(store.pending_effects(10).unwrap().len(), 1);
        let record = store.approval_fingerprint("fp-active").unwrap().unwrap();
        assert_eq!(record.state, "settling");
    }

    #[test]
    fn startup_reconciliation_settles_expired_approval_only_from_semantic_postcondition() {
        let mut store = SqliteStore::in_memory().unwrap();
        let task_id = TaskId::new("task-approval-settle");
        let run_id = RunId::new("run-approval-settle");
        let now = unix_time_ms().unwrap();
        create_pending_approval(
            &mut store,
            &task_id,
            &run_id,
            "fp-expired",
            "conversation-expired",
            now - 1,
        );
        let snapshot = ChatSurfaceSnapshot {
            conversation_fingerprint: Some(ConversationFingerprint::new("conversation-expired")),
            authorization_surface_present: false,
            authorization_settlement: AuthorizationSettlementState::Inactive,
            ..Default::default()
        };
        let effect = store.pending_effects(10).unwrap().remove(0);

        assert_eq!(
            reconcile_pending_effect(&mut store, &snapshot, &effect).unwrap(),
            StartupReconcileOutcome::SettledObservedApproval
        );
        assert!(store.pending_effects(10).unwrap().is_empty());
        let record = store.approval_fingerprint("fp-expired").unwrap().unwrap();
        assert_eq!(record.state, "settled");
    }

    #[test]
    fn startup_reconciliation_settles_reasoning_only_when_target_is_observed() {
        let mut store = SqliteStore::in_memory().unwrap();
        let task_id = TaskId::new("task-reasoning-reconcile");
        let run_id = RunId::new("run-reasoning-reconcile");
        create_pending_reasoning(&mut store, &task_id, &run_id, ReasoningPreset::ExtraHigh);
        let snapshot = ChatSurfaceSnapshot {
            reasoning_picker_available: true,
            selected_reasoning_preset: Some(ReasoningPreset::ExtraHigh),
            ..Default::default()
        };
        let effect = store.pending_effects(10).unwrap().remove(0);
        assert_eq!(
            reconcile_pending_effect(&mut store, &snapshot, &effect).unwrap(),
            StartupReconcileOutcome::SettledObservedReasoning
        );
        assert!(store.pending_effects(10).unwrap().is_empty());
    }

    #[test]
    fn startup_reconciliation_keeps_ambiguous_reasoning_pending() {
        let mut store = SqliteStore::in_memory().unwrap();
        let task_id = TaskId::new("task-reasoning-ambiguous");
        let run_id = RunId::new("run-reasoning-ambiguous");
        create_pending_reasoning(&mut store, &task_id, &run_id, ReasoningPreset::ExtraHigh);
        let snapshot = ChatSurfaceSnapshot {
            reasoning_picker_available: true,
            selected_reasoning_preset: Some(ReasoningPreset::High),
            ..Default::default()
        };
        let effect = store.pending_effects(10).unwrap().remove(0);
        assert_eq!(
            reconcile_pending_effect(&mut store, &snapshot, &effect).unwrap(),
            StartupReconcileOutcome::Clear
        );
        assert_eq!(store.pending_effects(10).unwrap().len(), 1);
    }

    #[test]
    fn startup_reconciliation_settles_send_only_from_semantic_postcondition() {
        let mut store = SqliteStore::in_memory().unwrap();
        let task_id = TaskId::new("task-reconcile");
        let run_id = RunId::new("run-reconcile");
        create_pending_send(&mut store, &task_id, &run_id, "dispatch-reconcile", "u0");

        let snapshot = ChatSurfaceSnapshot {
            user_turn_boundary: Some(UserTurnBoundary::new("u1")),
            current_dispatch_id: Some(DispatchId::new("dispatch-reconcile")),
            user_turn_ownership: OwnershipConfidence::Strong,
            ..Default::default()
        };
        let effect = store.pending_effects(10).unwrap().remove(0);
        assert_eq!(
            reconcile_pending_effect(&mut store, &snapshot, &effect).unwrap(),
            StartupReconcileOutcome::SettledObservedSend
        );
        assert!(store.pending_effects(10).unwrap().is_empty());
        assert!(
            store
                .dispatch_attempt_settlement(
                    &task_id,
                    &run_id,
                    &DispatchId::new("dispatch-reconcile"),
                )
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn startup_reconciliation_never_settles_ambiguous_send() {
        let mut store = SqliteStore::in_memory().unwrap();
        let task_id = TaskId::new("task-ambiguous");
        let run_id = RunId::new("run-ambiguous");
        create_pending_send(&mut store, &task_id, &run_id, "dispatch-ambiguous", "u0");

        let snapshot = ChatSurfaceSnapshot {
            user_turn_boundary: Some(UserTurnBoundary::new("u1")),
            current_dispatch_id: Some(DispatchId::new("different-dispatch")),
            user_turn_ownership: OwnershipConfidence::Strong,
            ..Default::default()
        };
        let effect = store.pending_effects(10).unwrap().remove(0);
        assert_eq!(
            reconcile_pending_effect(&mut store, &snapshot, &effect).unwrap(),
            StartupReconcileOutcome::Clear
        );
        assert_eq!(store.pending_effects(10).unwrap().len(), 1);
        assert_eq!(
            store
                .dispatch_attempt_settlement(
                    &task_id,
                    &run_id,
                    &DispatchId::new("dispatch-ambiguous"),
                )
                .unwrap(),
            None
        );
    }

    #[test]
    fn runtime_effect_state_preserves_continuous_orchestration() {
        let state = ContinuousTaskState::new(
            TaskId::new("task-preserve"),
            "goal".into(),
            ReasoningPreset::ExtraHigh,
        );
        let durable = merge_task_orchestration_state(None, &state).unwrap();
        let after_effect = merge_task_runtime_state(
            Some(&durable),
            &state.task_id,
            &RunId::new("run-preserve"),
            2,
            "send_prompt",
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&after_effect).unwrap();
        assert_eq!(
            value["orchestration"]["task_id"].as_str(),
            Some("task-preserve")
        );
        assert_eq!(value["orchestration"]["phase"].as_str(), Some("work"));
        assert_eq!(value["runtime"]["lastEffect"].as_str(), Some("send_prompt"));
    }

    struct LifecycleTestProcess;

    #[async_trait]
    impl ChatProcessPort for LifecycleTestProcess {
        async fn health(&self) -> Result<ChatProcessHealth> {
            Ok(ChatProcessHealth::Running)
        }

        async fn ensure_running(&self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn task_attachment_ingestion_persists_bytes_metadata_and_fails_closed_before_send() {
        let root = std::env::temp_dir().join(format!(
            "fabushi-chatgpt-attachment-ingestion-{}-{}",
            std::process::id(),
            dispatch_marker().unwrap()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let state_db = root.join("state.sqlite3");
        let input = root.join("notes.txt");
        std::fs::write(&input, b"attachment payload").unwrap();
        let runtime = DesktopRuntime::new(LifecycleTestProcess, FakeSurface::default())
            .with_state_db_path(state_db.clone());
        let task_id = TaskId::new("task-attachment-ingestion");

        let ids = runtime
            .stage_task_attachments(&task_id, std::slice::from_ref(&input))
            .unwrap();
        assert_eq!(ids.len(), 1);
        let records = SqliteStore::open(&state_db)
            .unwrap()
            .task_attachments(&task_id)
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].attachment_id, ids[0]);
        assert_eq!(records[0].file_name, "notes.txt");
        assert_eq!(
            std::fs::read(&records[0].storage_ref).unwrap(),
            b"attachment payload"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&records[0].metadata_json).unwrap()["byteLen"],
            18
        );

        let error = runtime
            .fail_closed_if_native_attachments_pending(&task_id)
            .unwrap_err();
        assert!(error.to_string().contains("refusing text-only Send"));
        assert_eq!(
            runtime.stage_task_attachments(&task_id, &[]).unwrap(),
            ids,
            "restart/resume must recover persisted task attachment identity"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    struct AttachmentReadySurface {
        ready: AtomicBool,
        attach_count: AtomicUsize,
        send_count: AtomicUsize,
    }

    #[async_trait]
    impl ChatSurfacePort for AttachmentReadySurface {
        async fn observe(&self) -> Result<ChatSurfaceSnapshot> {
            Ok(ChatSurfaceSnapshot {
                app_healthy: true,
                composer_ready: true,
                ..Default::default()
            })
        }

        async fn set_reasoning_preset(&self, _preset: ReasoningPreset) -> Result<bool> {
            Ok(true)
        }

        async fn attach_file(&self, file_name: &str, bytes: &[u8]) -> Result<bool> {
            assert_eq!(file_name, "notes.txt");
            assert_eq!(bytes, b"attachment payload");
            self.attach_count.fetch_add(1, Ordering::SeqCst);
            self.ready.store(true, Ordering::SeqCst);
            Ok(true)
        }

        async fn attachment_ready(&self, file_name: &str) -> Result<bool> {
            assert_eq!(file_name, "notes.txt");
            Ok(self.ready.load(Ordering::SeqCst))
        }

        async fn send_prompt(&self, _prompt: &str) -> Result<()> {
            assert!(
                self.ready.load(Ordering::SeqCst),
                "Send must not run before required attachment readiness"
            );
            self.send_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn approve_current_conversation(&self) -> Result<bool> {
            Ok(false)
        }

        async fn dismiss_rate_limit_notice(&self) -> Result<bool> {
            Ok(false)
        }

        async fn recover_current_surface(&self) -> Result<()> {
            Ok(())
        }

        async fn start_fresh_conversation(&self) -> Result<()> {
            self.ready.store(false, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn required_attachment_precedes_send_and_fresh_chat_reattaches() {
        let root = std::env::temp_dir().join(format!(
            "fabushi-chatgpt-attachment-send-{}-{}",
            std::process::id(),
            dispatch_marker().unwrap()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let state_db = root.join("state.sqlite3");
        let input = root.join("notes.txt");
        std::fs::write(&input, b"attachment payload").unwrap();
        let task_id = TaskId::new("task-attachment-send");
        let runtime = DesktopRuntime::new(LifecycleTestProcess, FakeSurface::default())
            .with_state_db_path(state_db.clone());
        runtime
            .stage_task_attachments(&task_id, std::slice::from_ref(&input))
            .unwrap();

        let raw = Arc::new(AttachmentReadySurface {
            ready: AtomicBool::new(false),
            attach_count: AtomicUsize::new(0),
            send_count: AtomicUsize::new(0),
        });
        let actor = DesktopSessionActorHandle::spawn(raw.clone());
        let durable = DurableRunSurface::new(
            actor,
            &state_db,
            task_id,
            RunId::new("run-attachment-send"),
            DispatchId::new("dispatch-attachment-send"),
            Phase::Work,
            Round::new(1),
        )
        .unwrap();

        durable.send_prompt("first").await.unwrap();
        assert_eq!(raw.attach_count.load(Ordering::SeqCst), 1);
        assert_eq!(raw.send_count.load(Ordering::SeqCst), 1);

        durable.start_fresh_conversation().await.unwrap();
        durable.send_prompt("second").await.unwrap();
        assert_eq!(
            raw.attach_count.load(Ordering::SeqCst),
            2,
            "fresh conversation must reattach task-persisted bytes before resend"
        );
        assert_eq!(raw.send_count.load(Ordering::SeqCst), 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn restart_reconciles_ready_attachment_without_duplicate_native_attach() {
        let root = std::env::temp_dir().join(format!(
            "fabushi-chatgpt-attachment-restart-{}-{}",
            std::process::id(),
            dispatch_marker().unwrap()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let state_db = root.join("state.sqlite3");
        let input = root.join("notes.txt");
        std::fs::write(&input, b"attachment payload").unwrap();
        let task_id = TaskId::new("task-attachment-restart");
        let run_id = RunId::new("run-attachment-restart");
        let runtime = DesktopRuntime::new(LifecycleTestProcess, FakeSurface::default())
            .with_state_db_path(state_db.clone());
        runtime
            .stage_task_attachments(&task_id, std::slice::from_ref(&input))
            .unwrap();

        let deadline = unix_time_ms().unwrap() + 45_000;
        let mut store = SqliteStore::open(&state_db).unwrap();
        store
            .record_transition(
                &TransitionRecord {
                    task_id: task_id.clone(),
                    run_id: run_id.clone(),
                    expected_revision: 0,
                    next_revision: 1,
                    event_kind: "attach_file_prepared".into(),
                    event_payload_json: json!({
                        "fileName":"notes.txt",
                        "sha256":"restored",
                        "readinessDeadlineUnixMs":deadline
                    })
                    .to_string(),
                    materialized_state_json: "{}".into(),
                    effect_kind: "attach_file".into(),
                    effect_payload_json: json!({
                        "fileName":"notes.txt",
                        "sha256":"restored",
                        "readinessDeadlineUnixMs":deadline
                    })
                    .to_string(),
                    idempotency_key: "attach-restart".into(),
                    prepared_dispatch: None,
                    prepared_approval: None,
                },
                unix_time_ms().unwrap(),
            )
            .unwrap();
        let attach_effect = store.pending_effects(10).unwrap().remove(0);
        store.mark_effect_attempted(attach_effect.id).unwrap();
        drop(store);

        let raw = Arc::new(AttachmentReadySurface {
            ready: AtomicBool::new(true),
            attach_count: AtomicUsize::new(0),
            send_count: AtomicUsize::new(0),
        });
        let durable = DurableRunSurface::new(
            DesktopSessionActorHandle::spawn(raw.clone()),
            &state_db,
            task_id,
            run_id,
            DispatchId::new("dispatch-attachment-restart"),
            Phase::Work,
            Round::new(1),
        )
        .unwrap();
        let restored = durable
            .pending_attachments
            .lock()
            .await
            .get("notes.txt")
            .cloned()
            .unwrap();
        assert_eq!(restored.readiness_deadline_unix_ms, deadline);

        durable.send_prompt("resume").await.unwrap();
        assert_eq!(
            raw.attach_count.load(Ordering::SeqCst),
            0,
            "semantic readiness after restart must settle the old effect before any reattach"
        );
        assert_eq!(raw.send_count.load(Ordering::SeqCst), 1);
        assert!(
            SqliteStore::open(&state_db)
                .unwrap()
                .pending_effects(20)
                .unwrap()
                .iter()
                .all(|effect| effect.effect_kind != "attach_file")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn startup_reconciliation_defers_attachment_to_task_scoped_worker() {
        let mut store = SqliteStore::in_memory().unwrap();
        let task_id = TaskId::new("task-attachment-reconcile");
        let run_id = RunId::new("run-attachment-reconcile");
        store
            .record_transition(
                &TransitionRecord {
                    task_id: task_id.clone(),
                    run_id: run_id.clone(),
                    expected_revision: 0,
                    next_revision: 1,
                    event_kind: "attach_file_prepared".into(),
                    event_payload_json: json!({"fileName":"notes.txt","sha256":"abc","readinessDeadlineUnixMs":12345}).to_string(),
                    materialized_state_json: "{}".into(),
                    effect_kind: "attach_file".into(),
                    effect_payload_json: json!({"fileName":"notes.txt","sha256":"abc","readinessDeadlineUnixMs":12345}).to_string(),
                    idempotency_key: "attach-reconcile".into(),
                    prepared_dispatch: None,
                    prepared_approval: None,
                },
                100,
            )
            .unwrap();
        let effect = store.pending_effects(10).unwrap().remove(0);
        assert_eq!(
            reconcile_pending_effect(&mut store, &ChatSurfaceSnapshot::default(), &effect).unwrap(),
            StartupReconcileOutcome::DeferredToRunWorker
        );
        assert_eq!(store.pending_effects(10).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn durable_pause_resume_cancel_controls_production_run_control() {
        let path = std::env::temp_dir().join(format!(
            "fabushi-chatgpt-lifecycle-{}-{}.sqlite3",
            std::process::id(),
            dispatch_marker().unwrap()
        ));
        let runtime = DesktopRuntime::new(LifecycleTestProcess, FakeSurface::default())
            .with_state_db_path(path.clone());
        let task_id = TaskId::new("task-lifecycle");
        let initial =
            ContinuousTaskState::new(task_id.clone(), "goal".into(), ReasoningPreset::ExtraHigh);
        runtime
            .persist_continuous_task_state(
                &initial,
                &RunId::new("run-lifecycle"),
                "continuous_phase_started",
                json!({"phase":"work","round":1}),
            )
            .unwrap();
        let now = unix_time_ms().unwrap();
        SqliteStore::open(&path)
            .unwrap()
            .arm_task_wake(
                &task_id,
                &RunId::new("run-lifecycle"),
                WakeReason::RateLimitCooldown.as_str(),
                now + 300_000,
                now,
            )
            .unwrap();

        let paused = runtime.pause_continuous_task(&task_id).unwrap();
        assert_eq!(paused.lifecycle, ContinuousTaskLifecycle::Paused);
        let control = SqliteRunControl {
            path: path.clone(),
            task_id: task_id.clone(),
        };
        assert_eq!(
            control.lifecycle().await.unwrap(),
            ContinuousTaskLifecycle::Paused
        );
        assert!(
            SqliteStore::open(&path)
                .unwrap()
                .task_wake(&task_id)
                .unwrap()
                .is_none()
        );

        let store = SqliteStore::open(&path).unwrap();
        store
            .store_review_settlement(
                &ReviewSettlementRecord {
                    task_id: task_id.as_str().to_owned(),
                    run_id: "run-review".into(),
                    phase: "review".into(),
                    round: 1,
                    conversation_fingerprint: "conversation-review".into(),
                    progress_signature: "old-progress".into(),
                    no_final_since_unix_ms: 1000,
                },
                1100,
            )
            .unwrap();
        drop(store);

        let resumed = runtime.resume_continuous_task(&task_id).unwrap();
        assert_eq!(resumed.lifecycle, ContinuousTaskLifecycle::Active);
        let store = SqliteStore::open(&path).unwrap();
        assert!(
            store
                .review_settlement(
                    &task_id,
                    &RunId::new("run-review"),
                    "review",
                    1,
                    "conversation-review",
                )
                .unwrap()
                .is_none()
        );
        drop(store);

        let now = unix_time_ms().unwrap();
        SqliteStore::open(&path)
            .unwrap()
            .arm_task_wake(
                &task_id,
                &RunId::new("run-lifecycle"),
                WakeReason::AttachmentWait.as_str(),
                now + 60_000,
                now,
            )
            .unwrap();
        let cancelled = runtime.cancel_continuous_task(&task_id).unwrap();
        assert_eq!(cancelled.lifecycle, ContinuousTaskLifecycle::Cancelled);
        assert!(
            SqliteStore::open(&path)
                .unwrap()
                .task_wake(&task_id)
                .unwrap()
                .is_none()
        );
        let loaded = runtime
            .load_continuous_task_state(&task_id)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.lifecycle, ContinuousTaskLifecycle::Cancelled);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    fn scheduler_test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "fabushi-supervisor-{name}-{}-{}.sqlite3",
            std::process::id(),
            unix_time_ms().unwrap_or(0)
        ))
    }

    fn cleanup_scheduler_path(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    async fn wait_for_persisted_wake_reason(
        store: &SqliteStore,
        task_id: &TaskId,
        expected_reason: &str,
    ) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            match store.task_wake(task_id) {
                Ok(Some(wake)) => {
                    assert_eq!(wake.reason, expected_reason);
                    return;
                }
                Ok(None) => {}
                Err(error) => panic!("failed reading persisted task wake: {error}"),
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "durable wake for {} was not persisted within bounded observation window",
                task_id.as_str()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn supervisor_exits_after_last_external_handle_drops() {
        let path = scheduler_test_path("shutdown");
        let (sender, receiver) = mpsc::channel(1);
        let (arm_sender, arm_receiver) = mpsc::unbounded_channel();
        let join = tokio::spawn(run_supervisor(
            receiver,
            arm_receiver,
            arm_sender,
            path.clone(),
            Arc::new(Mutex::new(())),
        ));
        drop(sender);
        tokio::time::timeout(Duration::from_secs(1), join)
            .await
            .expect("supervisor must exit after the final external command sender drops")
            .unwrap();
        cleanup_scheduler_path(&path);
    }

    #[tokio::test]
    async fn durable_wait_fails_closed_when_supervisor_is_unavailable() {
        let path = scheduler_test_path("closed-supervisor");
        let (sender, receiver) = mpsc::channel(1);
        drop(receiver);
        let supervisor = SupervisorHandle {
            sender,
            state_db_path: path.clone(),
            foreground_gate: ForegroundGate::default(),
            next_wake_token: Arc::new(AtomicU64::new(1)),
        };
        let error = supervisor
            .sleep_for(
                &TaskId::new("task-a"),
                &RunId::new("run-a"),
                WakeReason::RateLimitCooldown,
                Duration::from_secs(300),
            )
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("stopped before wake registration")
        );
        assert!(
            SqliteStore::open(&path)
                .unwrap()
                .task_wake(&TaskId::new("task-a"))
                .unwrap()
                .is_none()
        );
        cleanup_scheduler_path(&path);
    }

    #[tokio::test]
    async fn dispatch_confirmation_holds_foreground_until_owner_yields() {
        let path = scheduler_test_path("foreground-dispatch");
        let supervisor = SupervisorHandle::spawn(path.clone());
        let task_a = TaskId::new("task-a");
        let run_a = RunId::new("run-a");
        let gate = supervisor.foreground_gate.clone();
        let supervisor_a = supervisor.clone();
        let task_a_wait = task_a.clone();
        let run_a_wait = run_a.clone();
        let waiting = tokio::spawn(async move {
            supervisor_a
                .sleep_for(
                    &task_a_wait,
                    &run_a_wait,
                    WakeReason::DispatchConfirmation,
                    Duration::from_millis(50),
                )
                .await
                .unwrap();
        });
        tokio::time::sleep(Duration::from_millis(5)).await;
        let mut other = tokio::spawn(async move {
            let _permit = gate.acquire_mutation(&TaskId::new("task-b")).await;
            "task-b"
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut other)
                .await
                .is_err()
        );
        waiting.await.unwrap();
        assert!(!other.is_finished());
        supervisor
            .sleep_for(&task_a, &run_a, WakeReason::Poll, Duration::from_millis(5))
            .await
            .unwrap();
        assert_eq!(other.await.unwrap(), "task-b");
        cleanup_scheduler_path(&path);
    }

    #[tokio::test]
    async fn authorization_settlement_releases_foreground_for_other_tasks() {
        let path = scheduler_test_path("foreground-authorization");
        let supervisor = SupervisorHandle::spawn(path.clone());
        supervisor
            .foreground_gate
            .acquire_exclusive(&TaskId::new("task-a"))
            .await;
        let supervisor_a = supervisor.clone();
        let waiting = tokio::spawn(async move {
            supervisor_a
                .sleep_for(
                    &TaskId::new("task-a"),
                    &RunId::new("run-a"),
                    WakeReason::AuthorizationSettlement,
                    Duration::from_millis(60),
                )
                .await
                .unwrap();
        });
        tokio::time::sleep(Duration::from_millis(5)).await;
        let permit = tokio::time::timeout(
            Duration::from_millis(20),
            supervisor
                .foreground_gate
                .acquire_mutation(&TaskId::new("task-b")),
        )
        .await
        .expect("authorization settlement must not globally hold foreground");
        drop(permit);
        waiting.await.unwrap();
        cleanup_scheduler_path(&path);
    }

    #[tokio::test]
    async fn rate_limit_deferred_task_does_not_block_runnable_task() {
        let path = scheduler_test_path("rate-limit");
        let observer = SqliteStore::open(&path).unwrap();
        let supervisor = SupervisorHandle::spawn(path.clone());
        let a = SupervisorClock::new(
            supervisor.clone(),
            TaskId::new("task-a"),
            RunId::new("run-a"),
        );
        let b = SupervisorClock::new(
            supervisor.clone(),
            TaskId::new("task-b"),
            RunId::new("run-b"),
        );

        let a_wait = tokio::spawn(async move {
            a.sleep_for(WakeReason::RateLimitCooldown, Duration::from_secs(30))
                .await
                .unwrap();
            "a"
        });
        tokio::time::sleep(Duration::from_millis(5)).await;
        let b_wait = tokio::spawn(async move {
            b.sleep_for(WakeReason::Poll, Duration::from_millis(5))
                .await
                .unwrap();
            "b"
        });

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), b_wait)
                .await
                .expect("runnable task must not wait for another task's rate-limit cooldown")
                .unwrap(),
            "b"
        );
        assert!(!a_wait.is_finished());
        wait_for_persisted_wake_reason(
            &observer,
            &TaskId::new("task-a"),
            WakeReason::RateLimitCooldown.as_str(),
        )
        .await;
        a_wait.abort();
        let _ = a_wait.await;
        cleanup_scheduler_path(&path);
    }

    #[tokio::test]
    async fn attachment_backoff_leaves_other_tasks_runnable() {
        let path = scheduler_test_path("attachment");
        let observer = SqliteStore::open(&path).unwrap();
        let supervisor = SupervisorHandle::spawn(path.clone());
        let a = SupervisorClock::new(
            supervisor.clone(),
            TaskId::new("task-a"),
            RunId::new("run-a"),
        );
        let b = SupervisorClock::new(
            supervisor.clone(),
            TaskId::new("task-b"),
            RunId::new("run-b"),
        );
        let c = SupervisorClock::new(supervisor, TaskId::new("task-c"), RunId::new("run-c"));

        let a_wait = tokio::spawn(async move {
            a.sleep_for(WakeReason::AttachmentWait, Duration::from_secs(30))
                .await
                .unwrap();
        });
        let b_wait = tokio::spawn(async move {
            b.sleep_for(WakeReason::Poll, Duration::from_millis(5))
                .await
                .unwrap();
            "b"
        });
        let c_wait = tokio::spawn(async move {
            c.sleep_for(WakeReason::Poll, Duration::from_millis(8))
                .await
                .unwrap();
            "c"
        });

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), b_wait)
                .await
                .expect("task-b must remain runnable while task-a waits for attachment")
                .unwrap(),
            "b"
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), c_wait)
                .await
                .expect("task-c must remain runnable while task-a waits for attachment")
                .unwrap(),
            "c"
        );
        assert!(!a_wait.is_finished());
        wait_for_persisted_wake_reason(
            &observer,
            &TaskId::new("task-a"),
            WakeReason::AttachmentWait.as_str(),
        )
        .await;
        a_wait.abort();
        let _ = a_wait.await;
        cleanup_scheduler_path(&path);
    }

    #[tokio::test]
    async fn authorization_settlement_is_task_scoped_not_global() {
        let path = scheduler_test_path("authorization");
        let observer = SqliteStore::open(&path).unwrap();
        let supervisor = SupervisorHandle::spawn(path.clone());
        let a = SupervisorClock::new(
            supervisor.clone(),
            TaskId::new("task-a"),
            RunId::new("run-a"),
        );
        let b = SupervisorClock::new(supervisor, TaskId::new("task-b"), RunId::new("run-b"));

        let a_wait = tokio::spawn(async move {
            a.sleep_for(WakeReason::AuthorizationSettlement, Duration::from_secs(30))
                .await
                .unwrap();
        });
        let b_wait = tokio::spawn(async move {
            b.sleep_for(WakeReason::Poll, Duration::from_millis(4))
                .await
                .unwrap();
            "b"
        });
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), b_wait)
                .await
                .expect("authorization settlement must only defer its own task")
                .unwrap(),
            "b"
        );
        assert!(!a_wait.is_finished());
        wait_for_persisted_wake_reason(
            &observer,
            &TaskId::new("task-a"),
            WakeReason::AuthorizationSettlement.as_str(),
        )
        .await;
        a_wait.abort();
        let _ = a_wait.await;
        cleanup_scheduler_path(&path);
    }

    #[tokio::test]
    async fn deferred_tasks_wake_in_deadline_order() {
        let path = scheduler_test_path("earliest");
        let supervisor = SupervisorHandle::spawn(path.clone());
        let task_a = TaskId::new("task-a");
        let task_b = TaskId::new("task-b");
        let task_c = TaskId::new("task-c");
        let run_a = RunId::new("run-a");
        let run_b = RunId::new("run-b");
        let run_c = RunId::new("run-c");
        let now = unix_time_ms().unwrap();
        let base = now + 500;
        let mut store = SqliteStore::open(&path).unwrap();
        store
            .arm_task_wake(
                &task_a,
                &run_a,
                WakeReason::ReviewNoFinal.as_str(),
                base + 400,
                now,
            )
            .unwrap();
        store
            .arm_task_wake(
                &task_b,
                &run_b,
                WakeReason::RecoveryNavigation.as_str(),
                base + 100,
                now,
            )
            .unwrap();
        store
            .arm_task_wake(
                &task_c,
                &run_c,
                WakeReason::AttachmentWait.as_str(),
                base + 250,
                now,
            )
            .unwrap();
        drop(store);

        let order = Arc::new(Mutex::new(Vec::new()));
        let order_a = order.clone();
        let order_b = order.clone();
        let order_c = order.clone();
        let supervisor_a = supervisor.clone();
        let supervisor_b = supervisor.clone();
        let supervisor_c = supervisor;

        let a_wait = tokio::spawn(async move {
            supervisor_a.wait_existing(&task_a, &run_a).await.unwrap();
            order_a.lock().await.push("a");
        });
        let b_wait = tokio::spawn(async move {
            supervisor_b.wait_existing(&task_b, &run_b).await.unwrap();
            order_b.lock().await.push("b");
        });
        let c_wait = tokio::spawn(async move {
            supervisor_c.wait_existing(&task_c, &run_c).await.unwrap();
            order_c.lock().await.push("c");
        });

        a_wait.await.unwrap();
        b_wait.await.unwrap();
        c_wait.await.unwrap();
        assert_eq!(*order.lock().await, vec!["b", "c", "a"]);
        cleanup_scheduler_path(&path);
    }

    #[tokio::test]
    async fn restart_wait_uses_original_durable_deadline() {
        let path = scheduler_test_path("restart");
        let task = TaskId::new("task-restart");
        let run = RunId::new("run-restart");
        let now = unix_time_ms().unwrap();
        let original_deadline = now + 30_000;
        SqliteStore::open(&path)
            .unwrap()
            .arm_task_wake(
                &task,
                &run,
                WakeReason::RateLimitCooldown.as_str(),
                original_deadline,
                now,
            )
            .unwrap();

        let (sender, mut receiver) = mpsc::channel(1);
        let supervisor = SupervisorHandle {
            sender,
            state_db_path: path.clone(),
            foreground_gate: ForegroundGate::default(),
            next_wake_token: Arc::new(AtomicU64::new(1)),
        };
        let task_for_wait = task.clone();
        let run_for_wait = run.clone();
        let waiter = tokio::spawn(async move {
            supervisor
                .wait_existing(&task_for_wait, &run_for_wait)
                .await
        });

        let message = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .expect("restart reconciliation must register the durable wake")
            .expect("supervisor request channel must remain open");
        let SupervisorMessage::Sleep(request) = message;
        assert_eq!(request.task_id, task);
        assert_eq!(request.run_id, run);
        assert_eq!(
            request.reason,
            WakeReason::RateLimitCooldown.as_str().to_owned()
        );
        assert_eq!(request.requested_wake_at_unix_ms, original_deadline);
        assert!(request.durable);
        assert!(request.already_persisted);
        assert_eq!(
            SqliteStore::open(&path)
                .unwrap()
                .task_wake(&task)
                .unwrap()
                .unwrap()
                .wake_at_unix_ms,
            original_deadline
        );
        request.reply.send(Ok(())).unwrap();
        waiter.await.unwrap().unwrap();
        cleanup_scheduler_path(&path);
    }

    #[test]
    fn due_task_selection_round_robins_only_due_tasks() {
        fn pending(task: &str, wake_at_unix_ms: i64) -> (String, PendingWake) {
            let (reply, _receiver) = oneshot::channel();
            (
                task.to_owned(),
                PendingWake {
                    token: 1,
                    task_id: TaskId::new(task),
                    wake_at_unix_ms,
                    durable: true,
                    armed: true,
                    reply,
                },
            )
        }
        let pending = HashMap::from([
            pending("task-a", 100),
            pending("task-b", 100),
            pending("task-c", 200),
        ]);
        assert_eq!(
            select_due_task(&pending, 150, None).as_deref(),
            Some("task-a")
        );
        assert_eq!(
            select_due_task(&pending, 150, Some("task-a")).as_deref(),
            Some("task-b")
        );
        assert_eq!(
            select_due_task(&pending, 150, Some("task-b")).as_deref(),
            Some("task-a")
        );
        assert_eq!(
            select_due_task(&pending, 250, Some("task-b")).as_deref(),
            Some("task-c")
        );
    }

    #[tokio::test]
    async fn scheduler_rotation_keeps_ui_single_writer_without_duplicate_send() {
        let path = scheduler_test_path("single-writer");
        let supervisor = SupervisorHandle::spawn(path.clone());
        let fake = Arc::new(FakeSurface::default());
        let actor_surface: Arc<dyn ChatSurfacePort> = fake.clone();
        let actor = DesktopSessionActorHandle::spawn(actor_surface);
        let a = SupervisorClock::new(
            supervisor.clone(),
            TaskId::new("task-a"),
            RunId::new("run-a"),
        );
        let b = SupervisorClock::new(supervisor, TaskId::new("task-b"), RunId::new("run-b"));
        let actor_a = actor.clone();
        let actor_b = actor.clone();
        let left = tokio::spawn(async move {
            a.sleep_for(WakeReason::RecoveryNavigation, Duration::from_millis(20))
                .await
                .unwrap();
            actor_a.send_prompt("[Fabushi:task-a]").await.unwrap();
        });
        let right = tokio::spawn(async move {
            b.sleep_for(WakeReason::RecoveryNavigation, Duration::from_millis(20))
                .await
                .unwrap();
            actor_b.send_prompt("[Fabushi:task-b]").await.unwrap();
        });
        left.await.unwrap();
        right.await.unwrap();
        assert_eq!(fake.send_mutations.load(Ordering::SeqCst), 2);
        assert_eq!(fake.max_active_mutations.load(Ordering::SeqCst), 1);
        cleanup_scheduler_path(&path);
    }

    #[tokio::test]
    async fn desktop_session_actor_serializes_all_mutations() {
        let fake = Arc::new(FakeSurface::default());
        let actor_surface: Arc<dyn ChatSurfacePort> = fake.clone();
        let actor = DesktopSessionActorHandle::spawn(actor_surface);
        let left = actor.clone();
        let right = actor.clone();
        let (send, fresh) =
            tokio::join!(left.send_prompt("hello"), right.start_fresh_conversation(),);
        send.unwrap();
        fresh.unwrap();
        assert_eq!(fake.max_active_mutations.load(Ordering::SeqCst), 1);
    }
}
