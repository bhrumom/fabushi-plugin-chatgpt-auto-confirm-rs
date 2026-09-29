use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use fabushi_chatgpt_runtime::{
    ChatGptCdp, ExecutionProfile, QueueTask, RunOptions, SqliteStore, Supervisor,
    find_chromium_binary, launch_chromium, run_prompt,
};
use std::path::PathBuf;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "fabushi-chatgpt-auto-confirm")]
#[command(about = "Rust/Linux ChatGPT browser automation and durable queue runtime")]
struct Cli {
    #[arg(long, global = true, default_value = "http://127.0.0.1:9222")]
    cdp: String,
    #[arg(long, global = true)]
    db: Option<PathBuf>,
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
        #[arg(long, default_value = "GPT-5.6")]
        model: String,
        #[arg(long, default_value = "Extra High")]
        thinking: String,
    },
    Open {
        #[arg(long, default_value = "https://chatgpt.com/")]
        url: String,
    },
    QueueEnqueue {
        #[arg(long)]
        task_id: String,
        #[arg(long, default_value = "default")]
        account_id: String,
        #[arg(long)]
        prompt: String,
        #[arg(long, default_value_t = 1)]
        revision: u64,
        #[arg(long, default_value_t = 0)]
        priority: i32,
        #[arg(long, default_value = "GPT-5.6")]
        model: String,
        #[arg(long, default_value = "Extra High")]
        thinking: String,
        #[arg(long)]
        acceptance_prompt: Option<String>,
    },
    QueueRunOnce {
        #[arg(long, default_value = "default")]
        account_id: String,
    },
    QueueRecover,
    QueueStatus,
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
            let launch = launch_chromium(&binary, &profile, port, headed, &url)?;
            println!(
                "{}",
                serde_json::json!({
                    "pid": launch.pid,
                    "browser": launch.browser_binary,
                    "profile": launch.profile_dir,
                    "cdp": launch.endpoint,
                    "headed": launch.headed,
                    "url": url
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
            model,
            thinking,
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
                execution_profile: Some(ExecutionProfile {
                    model,
                    thinking_effort: thinking,
                    connector_requirements: Vec::new(),
                    tool_mode: None,
                }),
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
        Commands::QueueEnqueue {
            task_id,
            account_id,
            prompt,
            revision,
            priority,
            model,
            thinking,
            acceptance_prompt,
        } => {
            let store = SqliteStore::open(queue_db(cli.db))?;
            let mut task = QueueTask::new(task_id, account_id, prompt);
            task.current_revision = revision;
            task.priority = priority;
            task.acceptance_prompt = acceptance_prompt;
            task.execution_profile = ExecutionProfile {
                model,
                thinking_effort: thinking,
                connector_requirements: Vec::new(),
                tool_mode: None,
            };
            fabushi_chatgpt_runtime::QueueStore::enqueue_task(&store, &task)?;
            println!("{}", serde_json::to_string_pretty(&task)?);
        }
        Commands::QueueRunOnce { account_id } => {
            let supervisor = Supervisor::open(queue_db(cli.db), vec![(account_id, cli.cdp)], 1)?;
            supervisor.recover_startup()?;
            let report = supervisor.run_one().await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Commands::QueueRecover => {
            let store = SqliteStore::open(queue_db(cli.db))?;
            let recovered = fabushi_chatgpt_runtime::QueueStore::recover_expired_leases(
                &store,
                current_time_ms(),
            )?;
            println!("{}", serde_json::json!({"recovered": recovered}));
        }
        Commands::QueueStatus => {
            let store = SqliteStore::open(queue_db(cli.db))?;
            let snapshot = fabushi_chatgpt_runtime::QueueStore::snapshot(&store)?;
            println!(
                "{}",
                serde_json::json!({"tasks": snapshot.tasks, "runs": snapshot.runs})
            );
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

fn queue_db(explicit: Option<PathBuf>) -> PathBuf {
    explicit.unwrap_or_else(|| {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("fabushi")
            .join("chatgpt-auto-confirm")
            .join("queue.sqlite3")
    })
}

fn current_time_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
