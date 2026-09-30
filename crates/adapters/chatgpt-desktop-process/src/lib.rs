use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use fabushi_chatgpt_application::{ChatProcessHealth, ChatProcessPort};
use std::path::{Path, PathBuf};
use tokio::process::Command;

pub const DEFAULT_CHATGPT_LAUNCHER: &str = "/usr/bin/chatgpt";

#[derive(Debug, Clone)]
pub struct ChatGptDesktopProcess {
    launcher: PathBuf,
}

impl Default for ChatGptDesktopProcess {
    fn default() -> Self {
        Self::new(DEFAULT_CHATGPT_LAUNCHER)
    }
}

impl ChatGptDesktopProcess {
    pub fn new(launcher: impl Into<PathBuf>) -> Self {
        Self {
            launcher: launcher.into(),
        }
    }

    pub fn launcher(&self) -> &Path {
        &self.launcher
    }

    fn running_from_proc(&self) -> Result<bool> {
        let proc = Path::new("/proc");
        let entries = std::fs::read_dir(proc).context("read /proc for ChatGPT process discovery")?;

        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.bytes().all(|byte| byte.is_ascii_digit()) {
                continue;
            }

            let exe = entry.path().join("exe");
            if let Ok(target) = std::fs::read_link(&exe)
                && executable_matches(&target, &self.launcher)
            {
                return Ok(true);
            }

            let cmdline = entry.path().join("cmdline");
            if let Ok(bytes) = std::fs::read(cmdline)
                && command_line_matches(&bytes, &self.launcher)
            {
                return Ok(true);
            }
        }

        Ok(false)
    }
}

#[async_trait]
impl ChatProcessPort for ChatGptDesktopProcess {
    async fn health(&self) -> Result<ChatProcessHealth> {
        Ok(if self.running_from_proc()? {
            ChatProcessHealth::Running
        } else {
            ChatProcessHealth::NotRunning
        })
    }

    async fn ensure_running(&self) -> Result<()> {
        if self.health().await? == ChatProcessHealth::Running {
            return Ok(());
        }

        if !self.launcher.is_file() {
            bail!("ChatGPT desktop launcher does not exist: {:?}", self.launcher);
        }

        Command::new(&self.launcher)
            .spawn()
            .with_context(|| format!("launch ChatGPT desktop via {:?}", self.launcher))?;
        Ok(())
    }
}

fn executable_matches(candidate: &Path, launcher: &Path) -> bool {
    candidate == launcher
        || (candidate.file_name().is_some()
            && candidate.file_name() == launcher.file_name()
            && candidate
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("chatgpt")))
}

fn command_line_matches(command_line: &[u8], launcher: &Path) -> bool {
    let launcher_text = launcher.to_string_lossy();
    let launcher_name = launcher
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("chatgpt");

    command_line
        .split(|byte| *byte == 0)
        .filter_map(|part| std::str::from_utf8(part).ok())
        .any(|part| {
            part == launcher_text
                || Path::new(part)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.eq_ignore_ascii_case(launcher_name))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_match_accepts_launcher_or_chatgpt_binary_name() {
        assert!(executable_matches(
            Path::new("/usr/bin/chatgpt"),
            Path::new("/usr/bin/chatgpt")
        ));
        assert!(executable_matches(
            Path::new("/opt/ChatGPT/chatgpt"),
            Path::new("/usr/bin/chatgpt")
        ));
        assert!(!executable_matches(
            Path::new("/usr/bin/chromium"),
            Path::new("/usr/bin/chatgpt")
        ));
    }

    #[test]
    fn command_line_match_handles_nul_separated_proc_cmdline() {
        assert!(command_line_matches(
            b"/usr/bin/chatgpt\0--flag\0",
            Path::new("/usr/bin/chatgpt")
        ));
        assert!(!command_line_matches(
            b"/usr/bin/other\0chatgpt-helper\0",
            Path::new("/usr/bin/chatgpt")
        ));
    }
}
