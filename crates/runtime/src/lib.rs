use anyhow::Result;
use async_trait::async_trait;
use fabushi_chatgpt_application::{Clock, RunPrompt};
use std::time::{Duration, Instant};

pub use fabushi_chatgpt_application::RunOptions;
pub use fabushi_chatgpt_cdp::ChatGptCdp;
pub use fabushi_chatgpt_domain::{ChatSurfaceSnapshot, RunReport, RunState};
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
