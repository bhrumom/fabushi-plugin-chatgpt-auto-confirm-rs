use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use fabushi_chatgpt_cdp::ChatGptCdp;
use fabushi_chatgpt_runtime::{RunOptions, find_chromium_binary, launch_chromium, run_prompt};
use std::path::PathBuf;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "fabushi-chatgpt-auto-confirm")]
#[command(about = "Rust/Linux ChatGPT browser automation and allow-once confirmer")]
struct Cli {
    #[arg(long, global = true, default_value = "http://127.0.0.1:9222")]
    cdp: String,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Browser {
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
    Status,
    ApproveOnce,
    Send {
        #[arg(long)]
        prompt: String,
        #[arg(long, default_value_t = true)]
        auto_confirm: bool,
        #[arg(long, default_value_t = 3600)]
        timeout_seconds: u64,
        #[arg(long, default_value_t = 900)]
        poll_ms: u64,
        #[arg(long, default_value_t = 900)]
        stale_reload_seconds: u64,
        #[arg(long, default_value_t = 300)]
        rate_limit_pause_seconds: u64,
        #[arg(long, default_value_t = 90)]
        dispatch_confirm_seconds: u64,
        #[arg(long, default_value_t = 1800)]
        continuation_seconds: u64,
    },
    Open {
        #[arg(long, default_value = "https://chatgpt.com/")]
        url: String,
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
        Commands::Browser {
            browser_binary,
            profile,
            port,
            headed,
            url,
        } => {
            let binary = find_chromium_binary(browser_binary.as_deref())?;
            let profile = profile.unwrap_or_else(default_profile_dir);
            let pid = launch_chromium(&binary, &profile, port, headed, &url)?;
            println!(
                "{}",
                serde_json::json!({
                    "pid": pid,
                    "browser": binary,
                    "profile": profile,
                    "cdp": format!("http://127.0.0.1:{port}"),
                    "headed": headed,
                    "url": url,
                })
            );
        }
        Commands::Status => {
            let cdp = ChatGptCdp::connect(&cli.cdp).await?;
            println!("{}", serde_json::to_string_pretty(&cdp.snapshot().await?)?);
        }
        Commands::ApproveOnce => {
            let cdp = ChatGptCdp::connect(&cli.cdp).await?;
            println!(
                "{}",
                serde_json::json!({"clicked": cdp.click_allow_once().await?})
            );
        }
        Commands::Send {
            prompt,
            auto_confirm,
            timeout_seconds,
            poll_ms,
            stale_reload_seconds,
            rate_limit_pause_seconds,
            dispatch_confirm_seconds,
            continuation_seconds,
        } => {
            let cdp = ChatGptCdp::connect(&cli.cdp).await?;
            let options = RunOptions {
                timeout: Duration::from_secs(timeout_seconds),
                poll_interval: Duration::from_millis(poll_ms),
                auto_confirm,
                stale_reload_after: Duration::from_secs(stale_reload_seconds),
                rate_limit_pause: Duration::from_secs(rate_limit_pause_seconds),
                dispatch_confirm_after: Duration::from_secs(dispatch_confirm_seconds),
                continuation_after: Duration::from_secs(continuation_seconds),
                ..RunOptions::default()
            };
            let report = run_prompt(&cdp, &prompt, options)
                .await
                .context("ChatGPT automation run failed")?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Commands::Open { url } => {
            let cdp = ChatGptCdp::connect(&cli.cdp).await?;
            cdp.navigate(&url).await?;
            println!("{}", serde_json::json!({"navigated": url}));
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
