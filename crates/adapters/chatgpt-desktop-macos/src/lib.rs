#![cfg(target_os = "macos")]

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use axuielement::{AXUIElement, system_wide};
use fabushi_chatgpt_application::{ChatProcessHealth, ChatProcessPort, ChatSurfacePort};
use fabushi_chatgpt_domain::{ChatSurfaceSnapshot, DraftFingerprint, ReasoningPreset};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const ACCESSIBILITY_HELP: &str = "ChatGPT accessibility access is unavailable; grant the Rust CLI app access in System Settings > Privacy & Security > Accessibility";
const CHATGPT_EXECUTABLE: &str = "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT";

fn chatgpt_pid() -> Result<Option<i32>> {
    let output = Command::new("/bin/ps")
        .args(["-Ao", "pid=,args="])
        .output()
        .context("discover ChatGPT.app process with ps")?;
    if !output.status.success() {
        bail!("ps process discovery failed with {}", output.status);
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    Ok(listing.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let pid = fields.next()?.parse::<i32>().ok()?;
        (fields.next()? == CHATGPT_EXECUTABLE).then_some(pid)
    }))
}

#[derive(Debug, Clone, Default)]
pub struct ChatGptDesktopMacProcess;

#[async_trait]
impl ChatProcessPort for ChatGptDesktopMacProcess {
    async fn health(&self) -> Result<ChatProcessHealth> {
        Ok(if chatgpt_pid()?.is_some() {
            ChatProcessHealth::Running
        } else {
            ChatProcessHealth::NotRunning
        })
    }

    async fn ensure_running(&self) -> Result<()> {
        if self.health().await? == ChatProcessHealth::Running {
            return Ok(());
        }
        let status = Command::new("/usr/bin/open")
            .args(["-a", "ChatGPT"])
            .status()
            .context("launch ChatGPT.app with open -a ChatGPT")?;
        if !status.success() {
            bail!("open -a ChatGPT exited with {status}");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct ChatGptDesktopMacSurface;

impl ChatGptDesktopMacSurface {
    fn application(&self) -> Result<AXUIElement> {
        let pid = chatgpt_pid()?.ok_or_else(|| anyhow::anyhow!("ChatGPT.app is not running"))?;
        AXUIElement::from_pid(pid).ok_or_else(|| anyhow::anyhow!(ACCESSIBILITY_HELP))
    }

    fn walk(element: AXUIElement, output: &mut Vec<AXUIElement>, budget: usize) {
        if output.len() >= budget {
            return;
        }
        let children = element.children().unwrap_or_default();
        output.push(element);
        for child in children {
            if output.len() >= budget {
                break;
            }
            Self::walk(child, output, budget);
        }
    }

    fn tree(&self) -> Result<Vec<AXUIElement>> {
        let app = self.application().context(ACCESSIBILITY_HELP)?;
        let mut nodes = Vec::new();
        Self::walk(app, &mut nodes, 12_000);
        if nodes.len() <= 1 {
            bail!("{ACCESSIBILITY_HELP}; ChatGPT exposed no readable accessibility tree");
        }
        Ok(nodes)
    }

    fn role(node: &AXUIElement) -> Option<String> {
        node.string_attribute("AXRole").ok().flatten()
    }

    fn label(node: &AXUIElement) -> String {
        ["AXTitle", "AXDescription", "AXHelp", "AXPlaceholderValue"]
            .into_iter()
            .filter_map(|key| node.string_attribute(key).ok().flatten())
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    }

    fn enabled(node: &AXUIElement) -> bool {
        node.bool_attribute("AXEnabled")
            .ok()
            .flatten()
            .unwrap_or(true)
    }

    fn value(node: &AXUIElement) -> String {
        node.string_attribute("AXValue")
            .ok()
            .flatten()
            .unwrap_or_default()
            .to_lowercase()
    }

    fn reasoning_control(nodes: &[AXUIElement]) -> Option<AXUIElement> {
        let mut controls = nodes
            .iter()
            .filter(|node| {
                matches!(
                    Self::role(node).as_deref(),
                    Some("AXButton" | "AXPopUpButton")
                ) && Self::enabled(node)
                    && ["reasoning", "thinking", "power"]
                        .iter()
                        .any(|hint| Self::label(node).contains(hint))
            })
            .cloned();
        let control = controls.next()?;
        controls.next().is_none().then_some(control)
    }

    fn parse_preset(text: &str) -> Option<ReasoningPreset> {
        let words = text
            .to_lowercase()
            .split(|ch: char| !ch.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if words
            .windows(2)
            .any(|pair| pair[0] == "extra" && pair[1] == "high")
        {
            Some(ReasoningPreset::ExtraHigh)
        } else if words.iter().any(|word| word == "instant") {
            Some(ReasoningPreset::Instant)
        } else if words.iter().any(|word| word == "medium") {
            Some(ReasoningPreset::Medium)
        } else if words.iter().any(|word| word == "pro") {
            Some(ReasoningPreset::Pro)
        } else if words.iter().any(|word| word == "high") {
            Some(ReasoningPreset::High)
        } else {
            None
        }
    }

    fn preset_name(preset: ReasoningPreset) -> &'static str {
        match preset {
            ReasoningPreset::Instant => "instant",
            ReasoningPreset::Medium => "medium",
            ReasoningPreset::High => "high",
            ReasoningPreset::ExtraHigh => "extra high",
            ReasoningPreset::Pro => "pro",
        }
    }

    fn composers(nodes: &[AXUIElement]) -> Vec<AXUIElement> {
        nodes
            .iter()
            .filter(|node| {
                matches!(
                    Self::role(node).as_deref(),
                    Some("AXTextArea" | "AXTextField")
                ) && Self::enabled(node)
                    && ["message", "prompt", "ask"]
                        .iter()
                        .any(|hint| Self::label(node).contains(hint))
            })
            .cloned()
            .collect()
    }

    fn named_controls(nodes: &[AXUIElement], words: &[&str], roles: &[&str]) -> Vec<AXUIElement> {
        nodes
            .iter()
            .filter(|node| {
                Self::enabled(node)
                    && Self::role(node)
                        .as_deref()
                        .is_some_and(|role| roles.contains(&role))
                    && words.iter().any(|word| Self::label(node).contains(word))
            })
            .cloned()
            .collect()
    }

    fn attachment_ready_text(role: &str, text: &str, file_name: &str) -> bool {
        let wanted = file_name.trim().to_lowercase();
        if wanted.is_empty() {
            return false;
        }
        let text = text.to_lowercase();
        text.contains(&wanted)
            && (matches!(
                role,
                "AXButton" | "AXStaticText" | "AXGroup" | "AXRow" | "AXCell"
            ) || ["attachment", "uploaded", "remove", "附件", "上传", "移除"]
                .iter()
                .any(|word| text.contains(word)))
    }

    fn attachment_ready_in(nodes: &[AXUIElement], file_name: &str) -> bool {
        nodes.iter().any(|node| {
            let role = Self::role(node).unwrap_or_default();
            let text = format!("{} {}", Self::label(node), Self::value(node));
            Self::attachment_ready_text(&role, &text, file_name)
        })
    }

    fn safe_file_panel_shape(cancel_count: usize, confirm_count: usize) -> bool {
        cancel_count == 1 && confirm_count == 1
    }

    fn stage_attachment(file_name: &str, bytes: &[u8]) -> Result<PathBuf> {
        let candidate = file_name.trim();
        if candidate.is_empty()
            || candidate.contains('/')
            || candidate.contains('\\')
            || candidate.contains('\0')
        {
            bail!("unsafe desktop attachment file name");
        }
        let digest = format!("{:x}", Sha256::digest(bytes));
        let root = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("fabushi-chatgpt-auto-confirm")
            .join("attachments")
            .join(digest);
        fs::create_dir_all(&root)
            .with_context(|| format!("create native attachment staging directory {root:?}"))?;
        let path = root.join(candidate);
        fs::write(&path, bytes)
            .with_context(|| format!("write native attachment staging file {path:?}"))?;
        Ok(path)
    }

    fn focused_file_panel(&self) -> Result<(AXUIElement, Vec<AXUIElement>)> {
        let system = system_wide().ok_or_else(|| anyhow::anyhow!(ACCESSIBILITY_HELP))?;
        let window = system
            .focused_window()
            .context("read focused macOS Accessibility window")?
            .ok_or_else(|| {
                anyhow::anyhow!("native file chooser did not expose a focused window")
            })?;
        let role = Self::role(&window).unwrap_or_default();
        if !matches!(role.as_str(), "AXWindow" | "AXSheet") {
            bail!("focused Accessibility surface is not a window/sheet file chooser");
        }
        let mut nodes = Vec::new();
        Self::walk(window.clone(), &mut nodes, 4_000);
        let cancel = Self::named_controls(&nodes, &["cancel", "取消"], &["AXButton"]);
        let confirm = Self::named_controls(
            &nodes,
            &["open", "choose", "select", "upload", "打开", "选择", "上传"],
            &["AXButton"],
        );
        if !Self::safe_file_panel_shape(cancel.len(), confirm.len()) {
            bail!(
                "native file chooser requires exactly one Cancel and one Open/Choose action; found cancel={} confirm={}",
                cancel.len(),
                confirm.len()
            );
        }
        Ok((window, nodes))
    }

    fn open_go_to_path_sheet() -> Result<()> {
        let status = Command::new("/usr/bin/osascript")
            .args([
                "-e",
                "tell application \"System Events\" to keystroke \"g\" using {command down, shift down}",
            ])
            .status()
            .context("open macOS file chooser Go to Folder sheet through System Events")?;
        if !status.success() {
            bail!("System Events could not open the native file chooser path entry");
        }
        Ok(())
    }

    fn set_focused_file_path(path: &Path) -> Result<()> {
        let path = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("native attachment staging path is not UTF-8"))?;
        let system = system_wide().ok_or_else(|| anyhow::anyhow!(ACCESSIBILITY_HELP))?;
        let window = system
            .focused_window()
            .context("read focused file chooser window")?
            .ok_or_else(|| anyhow::anyhow!("native file chooser lost focused window"))?;
        let focused = system
            .focused_ui_element()
            .context("read focused file chooser path field")?
            .ok_or_else(|| {
                anyhow::anyhow!("native file chooser did not expose a focused path field")
            })?;
        if focused.pid().ok() != window.pid().ok()
            || Self::role(&focused).as_deref() != Some("AXTextField")
            || !focused.is_attribute_settable("AXValue").unwrap_or(false)
        {
            bail!("native file chooser path entry is not the unique focused editable text field");
        }
        focused
            .set_string_attribute("AXValue", path)
            .context("set exact attachment path in native file chooser")?;
        let observed = focused
            .string_attribute("AXValue")
            .context("re-read native file chooser path")?;
        if observed.as_deref() != Some(path) {
            bail!("native file chooser did not retain the exact attachment path");
        }
        let actions = focused.action_names().unwrap_or_default();
        if !actions.iter().any(|action| action == "AXConfirm") {
            bail!("native file chooser path entry does not expose AXConfirm");
        }
        focused
            .perform_action("AXConfirm")
            .context("confirm exact attachment path in native file chooser")?;
        Ok(())
    }
}

#[async_trait]
impl ChatSurfacePort for ChatGptDesktopMacSurface {
    async fn observe(&self) -> Result<ChatSurfaceSnapshot> {
        let nodes = self.tree()?;
        let composers = Self::composers(&nodes);
        let mut snapshot = ChatSurfaceSnapshot {
            app_healthy: true,
            composer_ready: composers.len() == 1,
            reasoning_picker_available: Self::reasoning_control(&nodes).is_some(),
            ..ChatSurfaceSnapshot::default()
        };
        if let Some(control) = Self::reasoning_control(&nodes) {
            snapshot.selected_reasoning_preset = Self::parse_preset(&format!(
                "{} {}",
                Self::label(&control),
                Self::value(&control)
            ));
        }
        if let [composer] = composers.as_slice()
            && let Some(value) = composer.string_attribute("AXValue").ok().flatten()
        {
            snapshot.draft_fingerprint = Some(DraftFingerprint::new(format!(
                "sha256:{:x}",
                Sha256::digest(value.as_bytes())
            )));
        }
        Ok(snapshot)
    }

    async fn set_reasoning_preset(&self, preset: ReasoningPreset) -> Result<bool> {
        let nodes = self.tree()?;
        let Some(control) = Self::reasoning_control(&nodes) else {
            bail!("could not identify exactly one ChatGPT reasoning control through Accessibility");
        };
        let observed = Self::parse_preset(&format!(
            "{} {}",
            Self::label(&control),
            Self::value(&control)
        ));
        if observed == Some(preset) {
            return Ok(true);
        }
        control
            .perform_action("AXPress")
            .context("open ChatGPT reasoning menu")?;
        let menu_nodes = self.tree()?;
        let target = Self::preset_name(preset);
        let choices = menu_nodes
            .iter()
            .filter(|node| {
                matches!(
                    Self::role(node).as_deref(),
                    Some("AXMenuItem" | "AXRadioButton" | "AXButton")
                ) && Self::enabled(node)
                    && Self::parse_preset(&Self::label(node)) == Some(preset)
            })
            .cloned()
            .collect::<Vec<_>>();
        let [choice] = choices.as_slice() else {
            bail!(
                "expected exactly one enabled reasoning option '{target}', found {}",
                choices.len()
            );
        };
        choice
            .perform_action("AXPress")
            .context("select ChatGPT reasoning option")?;
        Ok(true)
    }

    async fn send_prompt(&self, prompt: &str) -> Result<()> {
        if prompt.trim().is_empty() {
            bail!("refusing to send an empty prompt");
        }
        let nodes = self.tree()?;
        let composers = Self::composers(&nodes);
        let [composer] = composers.as_slice() else {
            bail!(
                "expected exactly one enabled ChatGPT composer, found {}",
                composers.len()
            );
        };
        if !composer.is_attribute_settable("AXValue").unwrap_or(false) {
            bail!("ChatGPT composer does not expose a settable AXValue");
        }
        let buttons = nodes
            .iter()
            .filter(|node| {
                Self::role(node).as_deref() == Some("AXButton")
                    && Self::enabled(node)
                    && ["send", "send message"]
                        .iter()
                        .any(|label| Self::label(node).contains(label))
            })
            .cloned()
            .collect::<Vec<_>>();
        let [button] = buttons.as_slice() else {
            bail!(
                "expected exactly one enabled ChatGPT Send button, found {}",
                buttons.len()
            );
        };
        composer
            .set_string_attribute("AXValue", prompt)
            .context("set ChatGPT composer through macOS Accessibility")?;
        let observed = composer
            .string_attribute("AXValue")
            .context("re-read ChatGPT composer after setting prompt")?;
        if observed.as_deref() != Some(prompt) {
            bail!("ChatGPT composer did not retain the exact prompt; refusing Send");
        }
        button
            .perform_action("AXPress")
            .context("press ChatGPT Send button through macOS Accessibility")?;
        Ok(())
    }

    async fn attach_file(&self, file_name: &str, bytes: &[u8]) -> Result<bool> {
        let nodes = self.tree()?;
        if Self::attachment_ready_in(&nodes, file_name) {
            return Ok(true);
        }
        let triggers = Self::named_controls(
            &nodes,
            &[
                "attach",
                "attach files",
                "add files",
                "add photos & files",
                "add photos and files",
                "upload",
                "附件",
                "添加文件",
                "上传",
            ],
            &["AXButton", "AXMenuButton"],
        );
        let [trigger] = triggers.as_slice() else {
            bail!(
                "expected exactly one enabled ChatGPT attachment control, found {}",
                triggers.len()
            );
        };
        let staged = Self::stage_attachment(file_name, bytes)?;
        trigger
            .perform_action("AXPress")
            .context("open ChatGPT attachment menu through Accessibility")?;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let after_trigger = self.tree()?;
        let uploads = Self::named_controls(
            &after_trigger,
            &[
                "upload files",
                "upload from computer",
                "from computer",
                "上传文件",
                "从电脑上传",
            ],
            &["AXButton", "AXMenuItem"],
        );
        if let [upload] = uploads.as_slice() {
            upload
                .perform_action("AXPress")
                .context("open native file chooser from ChatGPT attachment menu")?;
            tokio::time::sleep(std::time::Duration::from_millis(350)).await;
        } else if uploads.len() > 1 {
            bail!("multiple ChatGPT upload-from-computer actions are visible");
        }

        self.focused_file_panel()?;
        Self::open_go_to_path_sheet()?;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        Self::set_focused_file_path(&staged)?;
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let (_window, panel_nodes) = self.focused_file_panel()?;
        let confirm = Self::named_controls(
            &panel_nodes,
            &["open", "choose", "select", "upload", "打开", "选择", "上传"],
            &["AXButton"],
        );
        let [button] = confirm.as_slice() else {
            bail!("native file chooser did not expose exactly one final Open/Choose action");
        };
        button
            .perform_action("AXPress")
            .context("choose staged attachment through native file chooser")?;
        Ok(true)
    }

    async fn attachment_ready(&self, file_name: &str) -> Result<bool> {
        Ok(Self::attachment_ready_in(&self.tree()?, file_name))
    }

    async fn approve_current_conversation(&self) -> Result<bool> {
        bail!("macOS authorization controls are not yet mapped to bounded semantic evidence")
    }

    async fn dismiss_rate_limit_notice(&self) -> Result<bool> {
        bail!("macOS rate-limit controls are not yet mapped to verified accessibility evidence")
    }

    async fn recover_current_surface(&self) -> Result<()> {
        bail!("macOS surface recovery is not yet mapped to verified accessibility evidence")
    }

    async fn start_fresh_conversation(&self) -> Result<()> {
        let nodes = self.tree()?;
        let composers = Self::composers(&nodes);
        let [composer] = composers.as_slice() else {
            bail!(
                "expected exactly one enabled ChatGPT composer before starting a new chat, found {}",
                composers.len()
            );
        };
        let draft = composer
            .string_attribute("AXValue")
            .context("read current ChatGPT draft before starting a new chat")?
            .unwrap_or_default();
        if !draft.trim().is_empty() {
            bail!("refusing to leave the current ChatGPT conversation with an unsent draft");
        }
        if nodes.iter().any(|node| {
            Self::role(node).as_deref() == Some("AXButton")
                && Self::enabled(node)
                && ["stop generating", "stop responding"]
                    .iter()
                    .any(|label| Self::label(node).contains(label))
        }) {
            bail!("refusing to start a new chat while ChatGPT is generating a response");
        }
        let new_chat = nodes
            .iter()
            .filter(|node| {
                Self::role(node).as_deref() == Some("AXButton")
                    && Self::enabled(node)
                    && ["new chat", "new conversation"]
                        .iter()
                        .any(|label| Self::label(node).contains(label))
            })
            .cloned()
            .collect::<Vec<_>>();
        let [button] = new_chat.as_slice() else {
            bail!(
                "expected exactly one enabled ChatGPT New chat button, found {}",
                new_chat.len()
            );
        };
        button
            .perform_action("AXPress")
            .context("start a new ChatGPT conversation through Accessibility")?;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let after = self.tree()?;
        let composers = Self::composers(&after);
        let [composer] = composers.as_slice() else {
            bail!("ChatGPT did not expose exactly one composer after starting a new chat");
        };
        if composer
            .string_attribute("AXValue")
            .context("verify the new ChatGPT composer")?
            .is_some_and(|value| !value.trim().is_empty())
        {
            bail!("ChatGPT new conversation composer is not empty after the new-chat action");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_staging_rejects_path_traversal_names() {
        assert!(ChatGptDesktopMacSurface::stage_attachment("../secret.txt", b"x").is_err());
        assert!(ChatGptDesktopMacSurface::stage_attachment("folder/file.txt", b"x").is_err());
    }

    #[test]
    fn file_panel_shape_requires_unique_cancel_and_confirm_actions() {
        assert!(ChatGptDesktopMacSurface::safe_file_panel_shape(1, 1));
        assert!(!ChatGptDesktopMacSurface::safe_file_panel_shape(0, 1));
        assert!(!ChatGptDesktopMacSurface::safe_file_panel_shape(1, 0));
        assert!(!ChatGptDesktopMacSurface::safe_file_panel_shape(1, 2));
        assert!(!ChatGptDesktopMacSurface::safe_file_panel_shape(2, 1));
    }

    #[test]
    fn attachment_readiness_is_bound_to_exact_file_name_semantics() {
        assert!(ChatGptDesktopMacSurface::attachment_ready_text(
            "AXButton",
            "Remove attachment notes.txt",
            "notes.txt"
        ));
        assert!(ChatGptDesktopMacSurface::attachment_ready_text(
            "AXStaticText",
            "notes.txt uploaded",
            "notes.txt"
        ));
        assert!(!ChatGptDesktopMacSurface::attachment_ready_text(
            "AXButton",
            "Remove attachment other.txt",
            "notes.txt"
        ));
        assert!(!ChatGptDesktopMacSurface::attachment_ready_text(
            "AXUnknown",
            "notes.txt",
            "notes.txt"
        ));
    }
}
