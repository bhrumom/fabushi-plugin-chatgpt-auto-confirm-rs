use anyhow::{Context, Result, bail};
use fabushi_chatgpt_cdp::ChatGptCdp;
use fabushi_chatgpt_domain::{RunReport, RunState};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{info, warn};

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub timeout: Duration,
    pub poll_interval: Duration,
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
            auto_confirm: true,
            stale_reload_after: Duration::from_secs(15 * 60),
            rate_limit_pause: Duration::from_secs(5 * 60),
            max_rate_limit_pauses: 3,
            dispatch_confirm_after: Duration::from_secs(90),
            continuation_after: Duration::from_secs(30 * 60),
        }
    }
}

pub async fn run_prompt(cdp: &ChatGptCdp, prompt: &str, options: RunOptions) -> Result<RunReport> {
    let before = cdp.snapshot().await?;
    let baseline_users = before.user_turns;
    cdp.send_prompt(prompt).await?;

    let started = Instant::now();
    let mut dispatch_started = Instant::now();
    let mut last_progress = Instant::now();
    let mut last_continuation = Instant::now();
    let mut last_fingerprint = String::new();
    let mut stable_terminal_count = 0u8;
    let mut approvals_clicked = 0u32;
    let mut recoveries = 0u32;
    let mut rate_limit_pauses = 0u32;
    let mut dispatch_retries = 0u32;
    let mut continuations = 0u32;

    loop {
        if started.elapsed() > options.timeout {
            return Ok(RunReport {
                state: RunState::TimedOut,
                conversation_url: None,
                assistant_text: String::new(),
                approvals_clicked,
                recoveries,
                message: "run timed out before terminal response evidence".into(),
            });
        }

        let snapshot = cdp.snapshot().await?;

        if snapshot.user_turns < baseline_users + 1 {
            if dispatch_started.elapsed() >= options.dispatch_confirm_after {
                warn!(
                    dispatch_retries,
                    "prompt dispatch not confirmed within window; resending original prompt"
                );
                cdp.send_prompt(prompt).await?;
                dispatch_retries += 1;
                dispatch_started = Instant::now();
            }
            sleep(options.poll_interval).await;
            continue;
        }

        let fingerprint = snapshot.activity_fingerprint();
        if fingerprint != last_fingerprint {
            last_fingerprint = fingerprint;
            last_progress = Instant::now();
            stable_terminal_count = 0;
        }

        if options.auto_confirm && snapshot.waiting_for_approval && cdp.click_allow_once().await? {
            approvals_clicked += 1;
            info!(
                approvals_clicked,
                "approved one current-session authorization card"
            );
            sleep(Duration::from_millis(600)).await;
            continue;
        }

        if cdp.dismiss_rate_limit_notice().await? {
            rate_limit_pauses += 1;
            if rate_limit_pauses > options.max_rate_limit_pauses {
                bail!(
                    "rate-limit dialog repeated more than {} times",
                    options.max_rate_limit_pauses
                );
            }
            warn!(
                rate_limit_pauses,
                "rate limit detected; pausing before retry observation"
            );
            sleep(options.rate_limit_pause).await;
            continue;
        }

        if snapshot.is_terminal() {
            stable_terminal_count += 1;
            if stable_terminal_count >= 2 {
                return Ok(RunReport {
                    state: RunState::Complete,
                    conversation_url: Some(snapshot.url),
                    assistant_text: snapshot.assistant_text,
                    approvals_clicked,
                    recoveries,
                    message:
                        "terminal response action row is stable and bound to the latest user turn"
                            .into(),
                });
            }
        } else {
            stable_terminal_count = 0;
        }

        if last_progress.elapsed() >= options.stale_reload_after {
            warn!("no page progress within stale window; reloading current conversation");
            cdp.reload().await?;
            recoveries += 1;
            last_progress = Instant::now();
            sleep(Duration::from_secs(3)).await;
            continue;
        }

        if last_continuation.elapsed() >= options.continuation_after
            && !snapshot.response_in_flight()
            && !snapshot.is_terminal()
        {
            warn!(
                continuations,
                "no terminal reply within continuation window; asking ChatGPT to continue all work"
            );
            cdp.send_prompt("继续完成所有").await?;
            continuations += 1;
            last_continuation = Instant::now();
            last_progress = Instant::now();
            sleep(Duration::from_secs(1)).await;
            continue;
        }

        sleep(options.poll_interval).await;
    }
}

pub fn find_chromium_binary(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
        bail!("explicit Chromium path does not exist: {}", path.display());
    }

    for candidate in [
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
    ] {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Ok(path);
        }
    }

    bail!("no Chromium/Google Chrome binary found; pass --browser-binary")
}

pub fn launch_chromium(
    browser_binary: &Path,
    profile_dir: &Path,
    port: u16,
    headed: bool,
    initial_url: &str,
) -> Result<u32> {
    std::fs::create_dir_all(profile_dir).with_context(|| {
        format!(
            "failed to create profile directory {}",
            profile_dir.display()
        )
    })?;

    let mut command = Command::new(browser_binary);
    command
        .arg(format!("--remote-debugging-port={port}"))
        .arg(format!("--user-data-dir={}", profile_dir.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-background-networking")
        .arg("--disable-component-update")
        .arg("--disable-sync")
        .arg(initial_url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    if !headed {
        command.arg("--headless=new").arg("--disable-gpu");
    }

    let child = command.spawn().context("failed to start Chromium")?;
    Ok(child.id())
}
