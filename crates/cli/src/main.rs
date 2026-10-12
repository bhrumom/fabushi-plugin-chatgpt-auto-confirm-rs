use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use fabushi_chatgpt_runtime::{
    ChatGptCdp, ExecutionProfile, Queue, RunOptions, TaskMode, find_chromium_binary,
    launch_chromium, launch_desktop, run_prompt, run_tui, supervise, tui_snapshot,
};
use std::path::PathBuf;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

#[derive(Clone, Copy, ValueEnum)]
enum Backend {
    Desktop,
    Browser,
}

#[derive(Parser)]
#[command(name = "fabushi-chatgpt-auto-confirm")]
#[command(about = "ChatGPT desktop task dispatch and allow-once confirmer")]
struct Cli {
    #[arg(long, global = true)]
    cdp: Option<String>,
    #[arg(long, global = true, value_enum, default_value = "desktop")]
    backend: Backend,
    #[arg(long, global = true, env = "FABUSHI_WORKBENCH_DB")]
    database: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    ClearSavedDraft {
        expected: String,
    },
    Switch {
        id: String,
    },
    OpenTask {
        id: String,
    },
    Tui,
    TuiSnapshot {
        #[arg(long)]
        output: PathBuf,
    },
    Supervise {
        #[arg(long)]
        once: bool,
    },
    Add {
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        goal: Option<String>,
        #[arg(long, conflicts_with = "goal")]
        goal_file: Option<PathBuf>,
        #[arg(long, default_value = "goal")]
        mode: String,
        #[arg(long, default_value = "GPT-5.6 Sol")]
        model: String,
        #[arg(long, default_value_t = 3)]
        effort: u8,
        #[arg(long)]
        manual_approval: bool,
        #[arg(long)]
        attach: Vec<PathBuf>,
    },
    Tasks,
    Logs {
        #[arg(long, default_value = "")]
        task: String,
        #[arg(long, default_value_t = 40)]
        limit: usize,
    },
    Task {
        id: String,
        action: String,
        value: Option<String>,
    },
    Export {
        path: PathBuf,
    },
    Import {
        path: PathBuf,
    },
    Conversations,
    Profile {
        #[arg(long, default_value = "GPT-5.6 Sol")]
        model: String,
        #[arg(long, default_value_t = 3)]
        effort: u8,
    },
    Observe {
        #[arg(long, default_value = "")]
        token: String,
    },

    Desktop {
        #[arg(long, default_value = "/Applications/ChatGPT.app")]
        app: PathBuf,
        #[arg(long)]
        profile: Option<PathBuf>,
        #[arg(long, default_value_t = 9223)]
        port: u16,
    },
    Inspect,
    NewChat,
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
    ApproveSession,
    RetryLoad,
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

    let endpoint = cli.cdp.unwrap_or_else(|| match cli.backend {
        Backend::Desktop => "http://127.0.0.1:9223".into(),
        Backend::Browser => "http://127.0.0.1:9222".into(),
    });

    let database = cli.database.unwrap_or_else(|| {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("fabushi/chatgpt-auto-confirm/workbench.sqlite")
    });
    match cli.command {
        Commands::ClearSavedDraft { expected } => {
            connect(cli.backend, &endpoint)
                .await?
                .clear_saved_draft(&expected)
                .await?
        }
        Commands::Switch { id } => {
            connect(cli.backend, &endpoint)
                .await?
                .switch_conversation(&fabushi_chatgpt_runtime::SessionBinding {
                    identity: id.clone(),
                    sidebar_id: id,
                    title: String::new(),
                })
                .await?
        }
        Commands::OpenTask { id } => {
            fabushi_chatgpt_runtime::activate_task(&Queue::open(&database)?, &endpoint, &id).await?
        }
        Commands::Tui => run_tui(Queue::open(&database)?, endpoint)?,
        Commands::TuiSnapshot { output } => {
            tui_snapshot(&Queue::open(&database)?, &endpoint, &output)?
        }
        Commands::Supervise { once } => supervise(Queue::open(&database)?, endpoint, once).await?,
        Commands::Add {
            title,
            goal,
            goal_file,
            mode,
            model,
            effort,
            manual_approval,
            attach,
        } => {
            let goal = match (goal, goal_file) {
                (Some(g), _) => g,
                (_, Some(path)) => std::fs::read_to_string(path)?,
                _ => anyhow::bail!("--goal or --goal-file is required"),
            };
            if effort > 4 {
                anyhow::bail!("effort must be 0..4");
            }
            let title = title.unwrap_or_else(|| {
                goal.lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or("任务")
                    .chars()
                    .take(60)
                    .collect()
            });
            let mode = match mode.as_str() {
                "goal" => TaskMode::Goal,
                "once" => TaskMode::Once,
                _ => anyhow::bail!("mode must be goal or once"),
            };
            let task = Queue::open(&database)?.add(
                title,
                goal,
                mode,
                ExecutionProfile { model, effort },
                !manual_approval,
                &attach,
            )?;
            println!(
                "{}",
                serde_json::json!({"id":task.id,"title":task.title,"state":task.state,"goal_bytes":task.goal.len(),"mode":task.mode})
            );
        }
        Commands::Logs { task, limit } => {
            let queue = Queue::open(&database)?;
            for a in queue.activity(&task)?.iter().rev().take(limit) {
                println!("[{}] task={} {} {}", a.at, a.task_id, a.role, a.text);
            }
        }
        Commands::Tasks => println!(
            "{}",
            serde_json::to_string_pretty(&Queue::open(&database)?.tasks()?)?
        ),
        Commands::Task { id, action, value } => {
            Queue::open(&database)?.change(&id, &action, value)?
        }
        Commands::Export { path } => Queue::open(&database)?.export(&path)?,
        Commands::Import { path } => println!(
            "{}",
            serde_json::json!({"imported":Queue::open(&database)?.import(&path)?})
        ),
        Commands::Conversations => println!(
            "{}",
            serde_json::to_string_pretty(
                &connect(cli.backend, &endpoint)
                    .await?
                    .conversations()
                    .await?
            )?
        ),
        Commands::Profile { model, effort } => {
            connect(cli.backend, &endpoint)
                .await?
                .select_profile(&ExecutionProfile { model, effort })
                .await?
        }
        Commands::Observe { token } => println!(
            "{}",
            serde_json::to_string_pretty(
                &connect(cli.backend, &endpoint)
                    .await?
                    .task_observation(&token)
                    .await?
            )?
        ),

        Commands::Desktop { app, profile, port } => {
            let profile =
                profile.unwrap_or_else(|| default_profile_dir().with_file_name("desktop-profile"));
            launch_desktop(&app, &profile, port)?;
            println!(
                "{}",
                serde_json::json!({"backend":"desktop", "app":app, "profile":profile, "cdp":format!("http://127.0.0.1:{port}")})
            );
        }
        Commands::Inspect => {
            let cdp = connect(cli.backend, &endpoint).await?;
            println!("{}", serde_json::to_string_pretty(&cdp.surface().await?)?);
        }
        Commands::NewChat => {
            let cdp = connect(cli.backend, &endpoint).await?;
            cdp.new_desktop_chat().await?;
            println!("{}", serde_json::to_string_pretty(&cdp.surface().await?)?);
        }
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
                    "url": url,
                })
            );
        }
        Commands::Status => {
            let cdp = connect(cli.backend, &endpoint).await?;
            println!("{}", serde_json::to_string_pretty(&cdp.snapshot().await?)?);
        }
        Commands::RetryLoad => {
            let cdp = connect(cli.backend, &endpoint).await?;
            println!(
                "{}",
                serde_json::json!({"clicked":cdp.retry_loading_conversation().await?})
            );
        }
        Commands::ApproveSession => {
            let cdp = connect(cli.backend, &endpoint).await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&cdp.approve_current_session().await?)?
            );
        }
        Commands::ApproveOnce => {
            let cdp = connect(cli.backend, &endpoint).await?;
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
            let cdp = connect(cli.backend, &endpoint).await?;
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
            let cdp = connect(cli.backend, &endpoint).await?;
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

async fn connect(backend: Backend, endpoint: &str) -> Result<ChatGptCdp> {
    match backend {
        Backend::Desktop => ChatGptCdp::connect_desktop(endpoint).await,
        Backend::Browser => ChatGptCdp::connect(endpoint).await,
    }
}
