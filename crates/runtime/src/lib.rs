use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use fabushi_chatgpt_application::{
    ChatProcessHealth, ChatProcessPort, ChatSurfacePort, Clock, RunPrompt,
};
use fabushi_chatgpt_desktop_atspi::ChatGptDesktopAtspi;
use fabushi_chatgpt_desktop_process::ChatGptDesktopProcess;
use std::io::Read;
use std::time::{Duration, Instant};

pub use fabushi_chatgpt_application::RunOptions;
pub use fabushi_chatgpt_cdp::ChatGptCdp;
pub use fabushi_chatgpt_domain::{ChatSurfaceSnapshot, ReasoningPreset, RunReport, RunState};
pub use fabushi_chatgpt_linux_browser::{BrowserLaunch, find_chromium_binary, launch_chromium};

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

#[derive(Debug, Clone, Default)]
pub struct DesktopRuntime {
    process: ChatGptDesktopProcess,
    surface: ChatGptDesktopAtspi,
}

impl DesktopRuntime {
    pub fn new(process: ChatGptDesktopProcess, surface: ChatGptDesktopAtspi) -> Self {
        Self { process, surface }
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

    pub async fn run_prompt(
        &self,
        prompt: &str,
        requested_reasoning: ReasoningPreset,
        options: RunOptions,
    ) -> Result<RunReport> {
        self.ensure_ready().await?;
        let observed = self.surface.observed_reasoning_preset().await?;
        if observed != Some(requested_reasoning) {
            bail!(
                "ChatGPT desktop reasoning preset is not verified: requested={requested_reasoning:?} observed={observed:?}; refusing to Send"
            );
        }

        let marker = dispatch_marker()?;
        let prepared = format!("{prompt}\n\n[Fabushi:{marker}]");
        let clock = TokioClock::default();
        RunPrompt::new(&self.surface, &clock)
            .execute(&prepared, options)
            .await
    }
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
