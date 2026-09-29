use anyhow::{Result, bail};
use async_trait::async_trait;
use fabushi_chatgpt_domain::{PageSnapshot, RunReport, RunState};
use std::time::Duration;

#[async_trait]
pub trait BrowserPort: Send + Sync {
    async fn snapshot(&self) -> Result<PageSnapshot>;
    async fn send_prompt(&self, prompt: &str) -> Result<()>;
    async fn approve_once(&self) -> Result<bool>;
    async fn dismiss_rate_limit_notice(&self) -> Result<bool>;
    async fn reload(&self) -> Result<()>;
    async fn navigate(&self, url: &str) -> Result<()>;
}

#[async_trait]
pub trait Clock: Send + Sync {
    fn now(&self) -> Duration;
    async fn sleep(&self, duration: Duration);
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub timeout: Duration,
    pub poll_interval: Duration,
    pub approval_settle_delay: Duration,
    pub reload_settle_delay: Duration,
    pub continuation_settle_delay: Duration,
    pub auto_confirm: bool,
    pub stale_reload_after: Duration,
    pub rate_limit_pause: Duration,
    pub max_rate_limit_pauses: u32,
    pub dispatch_confirm_after: Duration,
    pub continuation_after: Duration,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(60 * 60),
            poll_interval: Duration::from_millis(900),
            approval_settle_delay: Duration::from_millis(600),
            reload_settle_delay: Duration::from_secs(3),
            continuation_settle_delay: Duration::from_secs(1),
            auto_confirm: true,
            stale_reload_after: Duration::from_secs(15 * 60),
            rate_limit_pause: Duration::from_secs(5 * 60),
            max_rate_limit_pauses: 3,
            dispatch_confirm_after: Duration::from_secs(90),
            continuation_after: Duration::from_secs(30 * 60),
        }
    }
}

pub struct RunPrompt<'a> {
    browser: &'a dyn BrowserPort,
    clock: &'a dyn Clock,
}

impl<'a> RunPrompt<'a> {
    pub fn new(browser: &'a dyn BrowserPort, clock: &'a dyn Clock) -> Self {
        Self { browser, clock }
    }

    pub async fn execute(&self, prompt: &str, options: RunOptions) -> Result<RunReport> {
        let before = self.browser.snapshot().await?;
        let baseline_users = before.user_turns;
        self.browser.send_prompt(prompt).await?;

        let started_at = self.clock.now();
        let mut dispatch_started_at = started_at;
        let mut last_progress_at = started_at;
        let mut last_continuation_at = started_at;
        let mut last_fingerprint = String::new();
        let mut stable_terminal_count = 0u8;
        let mut approvals_clicked = 0u32;
        let mut recoveries = 0u32;
        let mut rate_limit_pauses = 0u32;
        let mut dispatch_retries = 0u32;
        let mut continuations = 0u32;

        loop {
            let now = self.clock.now();
            if elapsed(now, started_at) > options.timeout {
                return Ok(RunReport {
                    state: RunState::TimedOut,
                    conversation_url: None,
                    assistant_text: String::new(),
                    approvals_clicked,
                    recoveries,
                    rate_limit_pauses,
                    dispatch_retries,
                    continuations,
                    message: "run timed out before terminal response evidence".into(),
                });
            }

            let snapshot = self.browser.snapshot().await?;

            if snapshot.user_turns < baseline_users + 1 {
                if elapsed(now, dispatch_started_at) >= options.dispatch_confirm_after {
                    self.browser.send_prompt(prompt).await?;
                    dispatch_retries += 1;
                    dispatch_started_at = now;
                }
                self.clock.sleep(options.poll_interval).await;
                continue;
            }

            let fingerprint = snapshot.activity_fingerprint();
            if fingerprint != last_fingerprint {
                last_fingerprint = fingerprint;
                last_progress_at = now;
                stable_terminal_count = 0;
            }

            if options.auto_confirm
                && snapshot.waiting_for_approval
                && self.browser.approve_once().await?
            {
                approvals_clicked += 1;
                self.clock.sleep(options.approval_settle_delay).await;
                continue;
            }

            if snapshot.rate_limit_notice && self.browser.dismiss_rate_limit_notice().await? {
                rate_limit_pauses += 1;
                if rate_limit_pauses > options.max_rate_limit_pauses {
                    bail!(
                        "rate-limit dialog repeated more than {} times",
                        options.max_rate_limit_pauses
                    );
                }
                self.clock.sleep(options.rate_limit_pause).await;
                continue;
            }

            if snapshot.is_terminal() {
                stable_terminal_count += 1;
                if stable_terminal_count >= 2 {
                    return Ok(RunReport {
                        state: RunState::Complete,
                        conversation_url: snapshot.canonical_conversation_url(),
                        assistant_text: snapshot.assistant_text,
                        approvals_clicked,
                        recoveries,
                        rate_limit_pauses,
                        dispatch_retries,
                        continuations,
                        message: "terminal response action row is stable and bound to the latest user turn".into(),
                    });
                }
            } else {
                stable_terminal_count = 0;
            }

            if elapsed(now, last_progress_at) >= options.stale_reload_after {
                self.browser.reload().await?;
                recoveries += 1;
                last_progress_at = now;
                self.clock.sleep(options.reload_settle_delay).await;
                continue;
            }

            if elapsed(now, last_continuation_at) >= options.continuation_after
                && !snapshot.response_in_flight()
                && !snapshot.is_terminal()
            {
                self.browser.send_prompt("继续完成所有").await?;
                continuations += 1;
                last_continuation_at = now;
                last_progress_at = now;
                self.clock.sleep(options.continuation_settle_delay).await;
                continue;
            }

            self.clock.sleep(options.poll_interval).await;
        }
    }
}

fn elapsed(now: Duration, earlier: Duration) -> Duration {
    now.saturating_sub(earlier)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct FakeClock {
        now: Mutex<Duration>,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                now: Mutex::new(Duration::ZERO),
            }
        }
    }

    #[async_trait]
    impl Clock for FakeClock {
        fn now(&self) -> Duration {
            *self.now.lock().expect("clock poisoned")
        }

        async fn sleep(&self, duration: Duration) {
            let mut now = self.now.lock().expect("clock poisoned");
            *now += duration;
        }
    }

    struct FakeBrowser {
        snapshots: Mutex<VecDeque<PageSnapshot>>,
        last_snapshot: Mutex<PageSnapshot>,
        sent: Mutex<Vec<String>>,
        approvals: Mutex<u32>,
        dismissals: Mutex<u32>,
        reloads: Mutex<u32>,
    }

    impl FakeBrowser {
        fn new(snapshots: impl IntoIterator<Item = PageSnapshot>) -> Self {
            let snapshots: VecDeque<_> = snapshots.into_iter().collect();
            let first = snapshots.front().cloned().unwrap_or_default();
            Self {
                snapshots: Mutex::new(snapshots),
                last_snapshot: Mutex::new(first),
                sent: Mutex::new(Vec::new()),
                approvals: Mutex::new(0),
                dismissals: Mutex::new(0),
                reloads: Mutex::new(0),
            }
        }

        fn sent(&self) -> Vec<String> {
            self.sent.lock().expect("sent poisoned").clone()
        }
    }

    #[async_trait]
    impl BrowserPort for FakeBrowser {
        async fn snapshot(&self) -> Result<PageSnapshot> {
            if let Some(next) = self
                .snapshots
                .lock()
                .expect("snapshots poisoned")
                .pop_front()
            {
                *self.last_snapshot.lock().expect("last snapshot poisoned") = next.clone();
                Ok(next)
            } else {
                Ok(self
                    .last_snapshot
                    .lock()
                    .expect("last snapshot poisoned")
                    .clone())
            }
        }

        async fn send_prompt(&self, prompt: &str) -> Result<()> {
            self.sent
                .lock()
                .expect("sent poisoned")
                .push(prompt.to_owned());
            Ok(())
        }

        async fn approve_once(&self) -> Result<bool> {
            *self.approvals.lock().expect("approvals poisoned") += 1;
            Ok(true)
        }

        async fn dismiss_rate_limit_notice(&self) -> Result<bool> {
            *self.dismissals.lock().expect("dismissals poisoned") += 1;
            Ok(true)
        }

        async fn reload(&self) -> Result<()> {
            *self.reloads.lock().expect("reloads poisoned") += 1;
            Ok(())
        }

        async fn navigate(&self, _url: &str) -> Result<()> {
            Ok(())
        }
    }

    fn terminal_snapshot() -> PageSnapshot {
        PageSnapshot {
            url: "https://chatgpt.com/c/abc123".into(),
            user_turns: 1,
            assistant_turns: 1,
            copy_available_on_last_assistant: true,
            response_actions_complete: true,
            response_action_turn_bound_to_last: true,
            assistant_text: "done".into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn completes_only_after_two_stable_terminal_observations() {
        let browser = FakeBrowser::new([
            PageSnapshot::default(),
            terminal_snapshot(),
            terminal_snapshot(),
        ]);
        let clock = FakeClock::new();
        let report = RunPrompt::new(&browser, &clock)
            .execute("hello", RunOptions::default())
            .await
            .expect("run should complete");

        assert_eq!(report.state, RunState::Complete);
        assert_eq!(
            report.conversation_url.as_deref(),
            Some("https://chatgpt.com/c/abc123")
        );
        assert_eq!(browser.sent(), vec!["hello"]);
    }

    #[tokio::test]
    async fn resends_when_dispatch_is_not_confirmed_for_90_seconds() {
        let mut delivered = terminal_snapshot();
        delivered.user_turns = 1;
        let browser = FakeBrowser::new([
            PageSnapshot::default(),
            PageSnapshot::default(),
            PageSnapshot::default(),
            PageSnapshot::default(),
            PageSnapshot::default(),
            delivered.clone(),
            delivered,
        ]);
        let clock = FakeClock::new();
        let options = RunOptions {
            poll_interval: Duration::from_secs(30),
            ..RunOptions::default()
        };

        let report = RunPrompt::new(&browser, &clock)
            .execute("original", options)
            .await
            .expect("run should complete");

        assert_eq!(report.dispatch_retries, 1);
        assert_eq!(browser.sent(), vec!["original", "original"]);
    }

    #[tokio::test]
    async fn exact_approval_path_is_application_controlled() {
        let approval = PageSnapshot {
            user_turns: 1,
            waiting_for_approval: true,
            ..Default::default()
        };
        let browser = FakeBrowser::new([
            PageSnapshot::default(),
            approval,
            terminal_snapshot(),
            terminal_snapshot(),
        ]);
        let clock = FakeClock::new();

        let report = RunPrompt::new(&browser, &clock)
            .execute("needs tool", RunOptions::default())
            .await
            .expect("run should complete");

        assert_eq!(report.approvals_clicked, 1);
        assert_eq!(*browser.approvals.lock().expect("approvals poisoned"), 1);
    }

    #[tokio::test]
    async fn rate_limit_pause_is_counted_and_resumed() {
        let limited = PageSnapshot {
            user_turns: 1,
            rate_limit_notice: true,
            ..Default::default()
        };
        let browser = FakeBrowser::new([
            PageSnapshot::default(),
            limited,
            terminal_snapshot(),
            terminal_snapshot(),
        ]);
        let clock = FakeClock::new();

        let report = RunPrompt::new(&browser, &clock)
            .execute("hello", RunOptions::default())
            .await
            .expect("run should complete");

        assert_eq!(report.rate_limit_pauses, 1);
        assert_eq!(*browser.dismissals.lock().expect("dismissals poisoned"), 1);
    }

    #[tokio::test]
    async fn reloads_after_15_minutes_without_progress() {
        let idle = PageSnapshot {
            user_turns: 1,
            ..Default::default()
        };
        let browser = FakeBrowser::new([
            PageSnapshot::default(),
            idle.clone(),
            idle.clone(),
            idle.clone(),
            idle,
            terminal_snapshot(),
            terminal_snapshot(),
        ]);
        let clock = FakeClock::new();
        let options = RunOptions {
            poll_interval: Duration::from_secs(5 * 60),
            continuation_after: Duration::from_secs(60 * 60),
            ..RunOptions::default()
        };

        let report = RunPrompt::new(&browser, &clock)
            .execute("original", options)
            .await
            .expect("run should complete");

        assert_eq!(report.recoveries, 1);
        assert_eq!(*browser.reloads.lock().expect("reloads poisoned"), 1);
    }

    #[tokio::test]
    async fn continues_after_30_minutes_without_terminal_completion() {
        let idle = PageSnapshot {
            user_turns: 1,
            ..Default::default()
        };
        let mut terminal = terminal_snapshot();
        terminal.user_turns = 2;
        terminal.assistant_turns = 2;
        let browser = FakeBrowser::new([
            PageSnapshot::default(),
            idle.clone(),
            idle.clone(),
            idle.clone(),
            idle,
            terminal.clone(),
            terminal,
        ]);
        let clock = FakeClock::new();
        let options = RunOptions {
            poll_interval: Duration::from_secs(10 * 60),
            stale_reload_after: Duration::from_secs(2 * 60 * 60),
            ..RunOptions::default()
        };

        let report = RunPrompt::new(&browser, &clock)
            .execute("original", options)
            .await
            .expect("run should complete");

        assert_eq!(report.continuations, 1);
        assert_eq!(browser.sent(), vec!["original", "继续完成所有"]);
    }
}
