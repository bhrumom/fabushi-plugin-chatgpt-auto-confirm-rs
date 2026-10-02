use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use fabushi_chatgpt_application::{
    ChatProcessHealth, ChatProcessPort, ChatSurfacePort, Clock, ContinuousTaskState,
    ReasoningDecision, ReasoningGateState, RecoveryRunContext, ReviewRunIdentity, RunPrompt,
    parse_strict_review_report,
};
use fabushi_chatgpt_desktop_atspi::ChatGptDesktopAtspi;
use fabushi_chatgpt_desktop_process::ChatGptDesktopProcess;
use fabushi_chatgpt_sqlite_store::{
    PreparedApproval, PreparedDispatch, SqliteStore, StateTransitionRecord, TransitionRecord,
    UiSessionLease,
};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{OnceCell, mpsc, oneshot};
use tokio::time::MissedTickBehavior;

pub use fabushi_chatgpt_application::RunOptions;
pub use fabushi_chatgpt_cdp::ChatGptCdp;
use fabushi_chatgpt_domain::{
    AuthorizationSettlementState, ConversationFingerprint, DispatchId, GoalRevision,
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
    SettledObservedSend,
    SettledObservedReasoning,
    SettledObservedApproval,
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
    baseline_user_turn: Option<UserTurnBoundary>,
}

#[derive(Debug, Clone)]
struct PendingApprovalSettlement {
    effect_id: i64,
    fingerprint: String,
    conversation_fingerprint: ConversationFingerprint,
    settlement_until_unix_ms: i64,
}

#[derive(Clone)]
struct DurableRunSurface {
    surface: DesktopSessionActorHandle,
    journal: Arc<Mutex<DurableRunJournal>>,
    dispatch_id: DispatchId,
    phase: Phase,
    round: Round,
    pending_reasoning: Arc<Mutex<Option<PendingReasoningSettlement>>>,
    pending_send: Arc<Mutex<Option<PendingSendSettlement>>>,
    pending_approval: Arc<Mutex<Option<PendingApprovalSettlement>>>,
}

impl DurableRunSurface {
    fn new(
        surface: DesktopSessionActorHandle,
        state_db_path: &Path,
        task_id: TaskId,
        run_id: RunId,
        dispatch_id: DispatchId,
        phase: Phase,
        round: Round,
    ) -> Result<Self> {
        let journal = DurableRunJournal::open(state_db_path, task_id, run_id)?;
        let phase_label = match phase {
            Phase::Work => "work",
            Phase::Review => "review",
        };
        let now = unix_time_ms()?;
        let mut restored_approval = None;
        for effect in journal.store.pending_effects(128)? {
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
            if record.phase != phase_label || record.round != i64::from(round.get()) {
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
            journal: Arc::new(Mutex::new(journal)),
            dispatch_id,
            phase,
            round,
            pending_reasoning: Arc::new(Mutex::new(None)),
            pending_send: Arc::new(Mutex::new(None)),
            pending_approval: Arc::new(Mutex::new(restored_approval)),
        })
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
        let confirmed = snapshot.current_dispatch_id.as_ref() == Some(&self.dispatch_id)
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
                &self.dispatch_id,
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

    async fn send_prompt(&self, prompt: &str) -> Result<()> {
        let baseline = self.surface.observe().await?.user_turn_boundary;
        let prepared_intent = json!({
            "preparedPrompt": prompt,
            "dispatchId": self.dispatch_id.as_str(),
        });
        let effect_payload = json!({
            "preparedPrompt": prompt,
            "dispatchId": self.dispatch_id.as_str(),
            "baselineUserTurnBoundary": baseline.as_ref().map(|value| value.as_str()),
        });
        let effect_id = {
            let mut journal = self.journal.lock().await;
            journal.begin(
                "send_prompt",
                effect_payload.to_string(),
                Some(PreparedDispatch {
                    dispatch_id: self.dispatch_id.clone(),
                    prepared_intent_json: prepared_intent.to_string(),
                }),
                None,
            )?
        };
        *self.pending_send.lock().await = Some(PendingSendSettlement {
            effect_id,
            baseline_user_turn: baseline,
        });

        let result = self.surface.send_prompt(prompt).await;
        if result.is_ok() {
            let snapshot = self.surface.observe().await?;
            self.settle_pending_send_if_observed(&snapshot).await?;
        }
        result
    }

    async fn approve_current_conversation(&self) -> Result<bool> {
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
        self.settle_pending_send_for_recovery().await?;
        self.record_and_settle(
            "start_fresh_conversation",
            json!({}),
            self.surface.start_fresh_conversation(),
        )
        .await
    }
}

pub struct RunWorker {
    surface: DurableRunSurface,
}

impl RunWorker {
    fn new(
        surface: DesktopSessionActorHandle,
        state_db_path: &Path,
        task_id: TaskId,
        run_id: RunId,
        dispatch_id: DispatchId,
        phase: Phase,
        round: Round,
    ) -> Result<Self> {
        Ok(Self {
            surface: DurableRunSurface::new(
                surface,
                state_db_path,
                task_id,
                run_id,
                dispatch_id,
                phase,
                round,
            )?,
        })
    }

    async fn execute(&self, prompt: &str, options: RunOptions) -> Result<RunReport> {
        let clock = TokioClock::default();
        RunPrompt::new(&self.surface, &clock)
            .execute(prompt, options)
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

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

pub struct DesktopRuntime {
    process: ChatGptDesktopProcess,
    surface: Arc<ChatGptDesktopAtspi>,
    desktop_session: OnceCell<DesktopSessionActorHandle>,
    state_db_path: PathBuf,
}

impl Default for DesktopRuntime {
    fn default() -> Self {
        Self::new(
            ChatGptDesktopProcess::default(),
            ChatGptDesktopAtspi::default(),
        )
    }
}

impl DesktopRuntime {
    pub fn new(process: ChatGptDesktopProcess, surface: ChatGptDesktopAtspi) -> Self {
        Self {
            process,
            surface: Arc::new(surface),
            desktop_session: OnceCell::new(),
            state_db_path: default_state_db_path(),
        }
    }

    pub fn with_state_db_path(mut self, path: PathBuf) -> Self {
        self.state_db_path = path;
        self
    }

    async fn desktop_session(&self) -> Result<&DesktopSessionActorHandle> {
        self.desktop_session
            .get_or_try_init(|| async {
                let surface: Arc<dyn ChatSurfacePort> = self.surface.clone();
                DesktopSessionActorHandle::spawn_durable(surface, &self.state_db_path)
            })
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
        bail!("ChatGPT desktop process started but semantic AT-SPI surface did not become ready")
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
        surface: &dyn ChatSurfacePort,
        target: ReasoningPreset,
    ) -> Result<()> {
        let clock = TokioClock::default();
        let mut gate = ReasoningGateState::default();
        loop {
            let snapshot = surface.observe().await?;
            match gate.observe(&snapshot, target, clock.now()) {
                ReasoningDecision::Ready => return Ok(()),
                ReasoningDecision::Select(preset) => {
                    let changed = surface.set_reasoning_preset(preset).await?;
                    if changed && surface.observe().await?.selected_reasoning_preset == Some(target)
                    {
                        gate.selection_succeeded();
                        return Ok(());
                    }
                    if gate.selection_failed(clock.now())
                        == ReasoningDecision::RecoverCurrentSurface
                    {
                        surface.recover_current_surface().await?;
                    }
                    clock.sleep(Duration::from_millis(500)).await;
                }
                ReasoningDecision::Wait => {
                    clock.sleep(Duration::from_millis(500)).await;
                }
                ReasoningDecision::RecoverCurrentSurface => {
                    surface.recover_current_surface().await?;
                    clock.sleep(Duration::from_millis(500)).await;
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
                StartupReconcileOutcome::Clear => {}
                StartupReconcileOutcome::SettledObservedSend
                | StartupReconcileOutcome::SettledObservedReasoning
                | StartupReconcileOutcome::SettledObservedApproval => {
                    settled_any = true;
                }
            }
        }

        let remaining = store.pending_effects(STARTUP_PENDING_EFFECT_LIMIT)?;
        if !remaining.is_empty() {
            let sample = remaining
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

    pub async fn run_continuous(
        &self,
        task_id: TaskId,
        goal: &str,
        requested_reasoning: ReasoningPreset,
        options: RunOptions,
    ) -> Result<RunReport> {
        let mut state = match self.load_continuous_task_state(&task_id)? {
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
        let identity_marker = dispatch_marker()?;
        self.run_prompt_with_identity(
            prompt,
            requested_reasoning,
            options,
            TaskId::new(format!("task-{identity_marker}")),
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
            task_id,
            run_id,
            dispatch_id.clone(),
            phase,
            round,
        )?;
        if start_fresh {
            worker.surface.start_fresh_conversation().await?;
        }
        self.ensure_reasoning_preset(&worker.surface, requested_reasoning)
            .await?;

        let prepared = format!("{prompt}\n\n[Fabushi:{marker}]");
        options.expected_dispatch_id = Some(dispatch_id);
        worker.execute(&prepared, options).await
    }
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
