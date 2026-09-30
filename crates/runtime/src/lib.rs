use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use fabushi_chatgpt_application::{
    ChatProcessHealth, ChatProcessPort, ChatSurfacePort, Clock, ReasoningDecision,
    ReasoningGateState, RunPrompt,
};
use fabushi_chatgpt_desktop_atspi::ChatGptDesktopAtspi;
use fabushi_chatgpt_desktop_process::ChatGptDesktopProcess;
use fabushi_chatgpt_sqlite_store::{SqliteStore, UiSessionLease};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{OnceCell, mpsc, oneshot};
use tokio::time::MissedTickBehavior;

pub use fabushi_chatgpt_application::RunOptions;
pub use fabushi_chatgpt_cdp::ChatGptCdp;
pub use fabushi_chatgpt_domain::{
    ChatSurfaceSnapshot, DispatchId, ReasoningPreset, RunReport, RunState,
};
pub use fabushi_chatgpt_linux_browser::{BrowserLaunch, find_chromium_binary, launch_chromium};

const DESKTOP_UI_LEASE_NAME: &str = "chatgpt-desktop-ui";
const DESKTOP_UI_LEASE_TTL_MS: i64 = 15_000;
const DESKTOP_UI_LEASE_HEARTBEAT: Duration = Duration::from_secs(5);

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

pub struct RunWorker {
    surface: DesktopSessionActorHandle,
}

impl RunWorker {
    fn new(surface: DesktopSessionActorHandle) -> Self {
        Self { surface }
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

    async fn ensure_reasoning_preset(&self, target: ReasoningPreset) -> Result<()> {
        let clock = TokioClock::default();
        let mut gate = ReasoningGateState::default();
        let surface = self.desktop_session().await?;
        loop {
            let snapshot = surface.observe().await?;
            match gate.observe(&snapshot, target, clock.now()) {
                ReasoningDecision::Ready => return Ok(()),
                ReasoningDecision::Select(preset) => {
                    let changed = surface.set_reasoning_preset(preset).await?;
                    if changed && self.surface.observed_reasoning_preset().await? == Some(target) {
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

    pub async fn run_prompt(
        &self,
        prompt: &str,
        requested_reasoning: ReasoningPreset,
        options: RunOptions,
    ) -> Result<RunReport> {
        self.ensure_ready().await?;
        self.ensure_reasoning_preset(requested_reasoning).await?;

        let marker = dispatch_marker()?;
        let prepared = format!("{prompt}\n\n[Fabushi:{marker}]");
        let mut options = options;
        options.expected_dispatch_id = Some(DispatchId::new(marker));
        let worker = RunWorker::new(self.desktop_session().await?.clone());
        worker.execute(&prepared, options).await
    }
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
    use std::sync::atomic::{AtomicUsize, Ordering};

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
