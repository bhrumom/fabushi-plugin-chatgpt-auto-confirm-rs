use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use fabushi_chatgpt_application::{ChatProcessHealth, ChatProcessPort};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
        let entries =
            std::fs::read_dir(proc).context("read /proc for ChatGPT process discovery")?;

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
            bail!(
                "ChatGPT desktop launcher does not exist: {:?}",
                self.launcher
            );
        }

        spawn_launcher(&self.launcher, effective_uid())
    }
}

#[cfg(target_os = "linux")]
fn effective_uid() -> Option<u32> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("Uid:"))?;
    line.split_ascii_whitespace().nth(2)?.parse().ok()
}

#[cfg(not(target_os = "linux"))]
fn effective_uid() -> Option<u32> {
    None
}

fn launcher_arguments(launcher: &Path, effective_uid: Option<u32>) -> Vec<&'static str> {
    if cfg!(target_os = "linux")
        && effective_uid == Some(0)
        && launcher == Path::new(DEFAULT_CHATGPT_LAUNCHER)
    {
        vec!["--no-sandbox", "--force-renderer-accessibility"]
    } else {
        Vec::new()
    }
}

fn spawn_launcher(launcher: &Path, effective_uid: Option<u32>) -> Result<()> {
    let mut command = Command::new(launcher);
    for argument in launcher_arguments(launcher, effective_uid) {
        command.arg(argument);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("launch ChatGPT desktop via {launcher:?}"))?;
    Ok(())
}

fn executable_matches(candidate: &Path, launcher: &Path) -> bool {
    candidate == launcher
        || candidate
            .file_name()
            .and_then(|name| name.to_str())
            .zip(launcher.file_name().and_then(|name| name.to_str()))
            .is_some_and(|(candidate_name, launcher_name)| {
                candidate_name.eq_ignore_ascii_case(launcher_name)
            })
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
        .flat_map(|part| std::iter::once(part).chain(part.split_ascii_whitespace()))
        .any(|part| {
            let part = part.trim_matches(|ch| ch == '"' || ch == '\'');
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
    fn launcher_arguments_limit_no_sandbox_to_linux_root_default_launcher() {
        let default = Path::new(DEFAULT_CHATGPT_LAUNCHER);
        if cfg!(target_os = "linux") {
            assert_eq!(
                launcher_arguments(default, Some(0)),
                vec!["--no-sandbox", "--force-renderer-accessibility"]
            );
        } else {
            assert!(launcher_arguments(default, Some(0)).is_empty());
        }
        assert!(launcher_arguments(default, Some(1000)).is_empty());
        assert!(launcher_arguments(default, None).is_empty());
        assert!(launcher_arguments(Path::new("/custom/chatgpt"), Some(0)).is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn spawned_desktop_process_does_not_inherit_cli_stdio() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, SystemTime, UNIX_EPOCH};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "fabushi-chatgpt-process-stdio-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create stdio contract directory");
        let report = root.join("stdio.txt");
        let launcher = root.join("fake-chatgpt");
        let script = format!(
            r#"#!/usr/bin/python3
import os
with open(r"{}", "w", encoding="utf-8") as report:
    for descriptor in range(3):
        report.write(os.readlink(f"/proc/self/fd/{{descriptor}}") + "\n")
"#,
            report.display()
        );
        std::fs::write(&launcher, script).expect("write fake launcher");
        let mut permissions = std::fs::metadata(&launcher)
            .expect("fake launcher metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&launcher, permissions).expect("make fake launcher executable");

        spawn_launcher(&launcher, Some(1000)).expect("spawn fake desktop launcher");

        let mut observed = None;
        for _ in 0..50 {
            if let Ok(value) = std::fs::read_to_string(&report)
                && value.lines().count() == 3
            {
                observed = Some(value);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let observed = observed.expect("fake launcher must report all inherited stdio descriptors");
        let descriptors = observed.lines().collect::<Vec<_>>();
        assert_eq!(descriptors, vec!["/dev/null", "/dev/null", "/dev/null"]);

        let _ = std::fs::remove_dir_all(root);
    }

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
        assert!(executable_matches(
            Path::new("/usr/lib/chatgpt/ChatGPT"),
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
        assert!(command_line_matches(
            b"/usr/lib/chatgpt/ChatGPT --no-sandbox\0",
            Path::new("/usr/bin/chatgpt")
        ));
        assert!(!command_line_matches(
            b"/usr/bin/other\0chatgpt-helper\0",
            Path::new("/usr/bin/chatgpt")
        ));
    }
}
