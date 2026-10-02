#![cfg(target_os = "macos")]

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use axuielement::AXUIElement;
use fabushi_chatgpt_application::{ChatProcessHealth, ChatProcessPort, ChatSurfacePort};
use fabushi_chatgpt_domain::{ChatSurfaceSnapshot, DraftFingerprint, ReasoningPreset};
use sha2::{Digest, Sha256};
use std::process::Command;

const ACCESSIBILITY_HELP: &str = "ChatGPT accessibility access is unavailable; grant the Rust CLI app access in System Settings > Privacy & Security > Accessibility";

#[derive(Debug, Clone, Default)]
pub struct ChatGptDesktopMacProcess;

#[async_trait]
impl ChatProcessPort for ChatGptDesktopMacProcess {
    async fn health(&self) -> Result<ChatProcessHealth> {
        let output = Command::new("/usr/bin/pgrep")
            .args(["-x", "ChatGPT"])
            .output()
            .context("discover ChatGPT.app process with pgrep")?;
        Ok(if output.status.success() {
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
        let output = Command::new("/usr/bin/pgrep")
            .args(["-x", "ChatGPT"])
            .output()
            .context("locate ChatGPT.app process")?;
        let pid = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .and_then(|line| line.trim().parse::<i32>().ok())
            .ok_or_else(|| anyhow::anyhow!("ChatGPT.app is not running"))?;
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
                matches!(Self::role(node).as_deref(), Some("AXButton" | "AXPopUpButton"))
                    && Self::enabled(node)
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
                matches!(Self::role(node).as_deref(), Some("AXTextArea" | "AXTextField"))
                    && Self::enabled(node)
                    && ["message", "prompt", "ask"]
                        .iter()
                        .any(|hint| Self::label(node).contains(hint))
            })
            .cloned()
            .collect()
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
        bail!("macOS new-chat control is not yet mapped to verified accessibility evidence")
    }
}
