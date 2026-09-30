use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use fabushi_chatgpt_runtime::{
    ChatGptCdp, DesktopRuntime, ReasoningPreset, RunOptions, find_chromium_binary, launch_chromium,
    run_prompt,
};
use std::path::PathBuf;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "fabushi-chatgpt-auto-confirm")]
#[command(about = "Fabushi automation for the ChatGPT desktop application")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Doctor,
    Status,
    Send {
        #[arg(long)]
        prompt: String,
        #[arg(long, default_value_t = true)]
        auto_confirm: bool,
        #[arg(long, default_value_t = 3600)]
        timeout_seconds: u64,
        #[arg(long, default_value_t = 900)]
        poll_ms: u64,
        #[arg(long, default_value_t = 3)]
        reasoning: u8,
    },
    LegacyBrowser {
        #[arg(long)]
        browser_binary: Option<PathBuf>,
        #[arg(long)]
        profile: Option<PathBuf>,
        #[arg(long, default_value_t = 9222)]
        port: u16,
        #[arg(long, default_value_t = true)]
        headed: bool,
        #[arg(long, default_value = "https://chatgpt.com/")]
        url: String,
    },
    LegacyCdpStatus {
        #[arg(long, default_value = "http://127.0.0.1:9222")]
        cdp: String,
    },
    LegacyCdpSend {
        #[arg(long, default_value = "http://127.0.0.1:9222")]
        cdp: String,
        #[arg(long)]
        prompt: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Commands::Doctor => {
            let runtime = DesktopRuntime::default();
            let snapshot = runtime.snapshot().await?;
            println!(
                "{}",
                serde_json::json!({
                    "runtime": "chatgpt-desktop-atspi",
                    "appHealthy": snapshot.app_healthy,
                    "composerReady": snapshot.composer_ready,
                    "reasoningPickerAvailable": snapshot.reasoning_picker_available,
                    "selectedReasoningPreset": snapshot.selected_reasoning_preset,
                })
            );
        }
        Commands::Status => {
            let runtime = DesktopRuntime::default();
            println!(
                "{}",
                serde_json::to_string_pretty(&runtime.snapshot().await?)?
            );
        }
        Commands::Send {
            prompt,
            auto_confirm,
            timeout_seconds,
            poll_ms,
            reasoning,
        } => {
            let runtime = DesktopRuntime::default();
            let options = RunOptions {
                timeout: Duration::from_secs(timeout_seconds),
                poll_interval: Duration::from_millis(poll_ms),
                auto_confirm,
                ..RunOptions::default()
            };
            let requested_reasoning = ReasoningPreset::from_index(reasoning)
                .ok_or_else(|| anyhow::anyhow!("reasoning must be one of 0,1,2,3,4"))?;
            let report = runtime
                .run_prompt(&prompt, requested_reasoning, options)
                .await
                .context("ChatGPT desktop automation run failed")?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Commands::LegacyBrowser {
            browser_binary,
            profile,
            port,
            headed,
            url,
        } => {
            let binary = find_chromium_binary(browser_binary.as_deref())?;
            let profile = profile.unwrap_or_else(default_profile_dir);
            let launch = launch_chromium(&binary, &profile, port, headed, &url)?;
            println!(
                "{}",
                serde_json::json!({
                    "pid": launch.pid,
                    "browser": launch.browser_binary,
                    "profile": launch.profile_dir,
                    "cdp": launch.endpoint,
                    "headed": launch.headed,
                    "url": url,
                    "legacy": true,
                })
            );
        }
        Commands::LegacyCdpStatus { cdp } => {
            let cdp = ChatGptCdp::connect(&cdp).await?;
            println!("{}", serde_json::to_string_pretty(&cdp.snapshot().await?)?);
        }
        Commands::LegacyCdpSend { cdp, prompt } => {
            let cdp = ChatGptCdp::connect(&cdp).await?;
            let report = run_prompt(&cdp, &prompt, RunOptions::default()).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }

    Ok(())
}

fn default_profile_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("fabushi")
        .join("chatgpt-auto-confirm")
        .join("chromium-profile")
}
