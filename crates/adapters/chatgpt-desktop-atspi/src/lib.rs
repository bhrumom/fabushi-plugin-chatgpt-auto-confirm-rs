use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use fabushi_chatgpt_application::ChatSurfacePort;
use fabushi_chatgpt_domain::{ChatSurfaceSnapshot, ReasoningPreset};
use serde::de::DeserializeOwned;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use tokio::process::Command;

const BRIDGE: &str = include_str!("bridge.py");

#[derive(Debug, Clone)]
pub struct ChatGptDesktopAtspi {
    python: PathBuf,
}

impl Default for ChatGptDesktopAtspi {
    fn default() -> Self {
        Self::new("/usr/bin/python3")
    }
}

impl ChatGptDesktopAtspi {
    pub fn new(python: impl Into<PathBuf>) -> Self {
        Self {
            python: python.into(),
        }
    }

    pub fn python(&self) -> &Path {
        &self.python
    }

    async fn bridge_value(&self, op: &str, arg: Option<&str>) -> Result<Value> {
        let mut command = Command::new(&self.python);
        command.arg("-c").arg(BRIDGE).arg(op);
        if let Some(arg) = arg {
            command.arg(arg);
        }
        let output = command
            .output()
            .await
            .with_context(|| format!("run ChatGPT desktop AT-SPI bridge operation {op}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            bail!(
                "AT-SPI bridge operation {op} failed: status={} stdout={} stderr={}",
                output.status,
                stdout.trim(),
                stderr.trim()
            );
        }

        let value: Value = serde_json::from_slice(&output.stdout)
            .with_context(|| format!("decode AT-SPI bridge output for {op}"))?;
        if let Some(message) = value.get("error").and_then(Value::as_str) {
            bail!("AT-SPI bridge operation {op} failed: {message}");
        }
        Ok(value)
    }

    async fn bridge<T: DeserializeOwned>(&self, op: &str, arg: Option<&str>) -> Result<T> {
        serde_json::from_value(self.bridge_value(op, arg).await?)
            .with_context(|| format!("decode semantic AT-SPI result for {op}"))
    }

    pub async fn observed_reasoning_preset(&self) -> Result<Option<ReasoningPreset>> {
        self.bridge("reasoning", None).await
    }
}

#[async_trait]
impl ChatSurfacePort for ChatGptDesktopAtspi {
    async fn observe(&self) -> Result<ChatSurfaceSnapshot> {
        self.bridge("snapshot", None).await
    }

    async fn set_reasoning_preset(&self, preset: ReasoningPreset) -> Result<bool> {
        let target = preset.index().to_string();
        self.bridge("set-reasoning", Some(&target)).await
    }

    async fn send_prompt(&self, prompt: &str) -> Result<()> {
        let sent: bool = self.bridge("send", Some(prompt)).await?;
        if !sent {
            bail!("AT-SPI bridge did not confirm Send action");
        }
        Ok(())
    }

    async fn attach_file(&self, file_name: &str, bytes: &[u8]) -> Result<bool> {
        let candidate = file_name.trim();
        if candidate.is_empty()
            || candidate.contains('/')
            || candidate.contains('\\')
            || candidate.contains('\0')
        {
            bail!("unsafe desktop attachment file name");
        }
        let digest = format!("{:x}", Sha256::digest(bytes));
        let staging_root = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from))
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
            .unwrap_or_else(std::env::temp_dir);
        let directory = staging_root
            .join("fabushi-chatgpt-auto-confirm")
            .join("attachments")
            .join(&digest);
        fs::create_dir_all(&directory)
            .with_context(|| format!("create native attachment staging directory {directory:?}"))?;
        let path = directory.join(candidate);
        fs::write(&path, bytes)
            .with_context(|| format!("write native attachment staging file {path:?}"))?;
        let path = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("native attachment staging path is not UTF-8"))?;
        self.bridge("attach", Some(path)).await
    }

    async fn attachment_ready(&self, file_name: &str) -> Result<bool> {
        self.bridge("attachment-ready", Some(file_name)).await
    }

    async fn approve_current_conversation(&self) -> Result<bool> {
        self.bridge("approve", None).await
    }

    async fn dismiss_rate_limit_notice(&self) -> Result<bool> {
        self.bridge("dismiss-rate-limit", None).await
    }

    async fn recover_current_surface(&self) -> Result<()> {
        let recovered: bool = self.bridge("recover", None).await?;
        if !recovered {
            bail!("AT-SPI bridge did not confirm surface recovery request");
        }
        Ok(())
    }

    async fn start_fresh_conversation(&self) -> Result<()> {
        let started: bool = self.bridge("fresh", None).await?;
        if !started {
            bail!("AT-SPI bridge did not confirm fresh conversation action");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_is_atspi_and_fail_closed_for_persistent_authorization() {
        assert!(BRIDGE.contains("import pyatspi"));
        assert!(BRIDGE.contains("PERSISTENT_WORDS"));
        assert!(BRIDGE.contains("len(candidates) != 1"));
        assert!(BRIDGE.contains("Reject") || BRIDGE.contains("REJECT_WORDS"));
    }

    #[test]
    fn authorization_projection_requires_one_bounded_card_shape() {
        assert!(BRIDGE.contains("def authorization_cards(items):"));
        assert!(BRIDGE.contains("is_descendant(item[\"node\"], container)"));
        assert!(BRIDGE.contains("reject_item is not None and options_item is not None"));
        assert!(BRIDGE.contains("auth_present = bool(auth_cards)"));
        assert!(BRIDGE.contains("auth_actionable = any(card[\"actionable\"]"));
        assert!(!BRIDGE.contains("auth_present = bool(reject and allow and options)"));
        assert!(BRIDGE.contains(
            "cards = [card for card in authorization_cards(items) if card[\"actionable\"]]"
        ));
    }

    #[test]
    fn bridge_projects_activity_separately_from_terminal_prose() {
        assert!(BRIDGE.contains("def assistant_activity_scope(node"));
        assert!(BRIDGE.contains("def assistant_activity_trace(items):"));
        assert!(BRIDGE.contains("style == \"assistant-message\" and tone == \"tertiary\""));
        assert!(BRIDGE.contains("if assistant_activity_scope(item[\"node\"]) is not None:"));
        assert!(BRIDGE.contains("\"assistant_visible_work_trace\": work_trace"));
        assert!(BRIDGE.contains("progress_material = prose + \"|\" + \"\\n\".join(work_trace)"));
    }

    #[test]
    fn bridge_binds_terminal_copy_to_bounded_response_scope() {
        assert!(BRIDGE.contains("def response_local_copy_evidence("));
        assert!(BRIDGE.contains("def bounded_common_ancestor("));
        assert!(BRIDGE.contains("not is_descendant(marker_node, scope)"));
        assert!(BRIDGE.contains(
            "copy_after = response_local_copy_evidence(items, marker_index, response_text_items)"
        ));
        assert!(BRIDGE.contains("latest_text_item = response_text_items[-1]"));
        assert!(BRIDGE.contains("for copy_item in reversed(copies):"));
        assert!(!BRIDGE.contains("for text_item in reversed(response_text_items):"));
        assert!(!BRIDGE.contains("copy_after = any("));
    }

    #[test]
    fn reasoning_effect_uses_semantic_power_control_and_reobserves_positions() {
        assert!(BRIDGE.contains("item[\"name\"] == \"Power\""));
        assert!(BRIDGE.contains("KEY_SYM"));
        assert!(BRIDGE.contains("reasoning_position"));
        assert!(BRIDGE.contains("next_position == current"));
    }

    #[test]
    fn attachment_projection_is_filename_bound_and_native_action_is_fail_closed() {
        assert!(BRIDGE.contains("def attachment_ready_for(items, file_name):"));
        assert!(BRIDGE.contains("wanted not in value"));
        assert!(BRIDGE.contains("def file_chooser_scope(node):"));
        assert!(BRIDGE.contains("len(focused) == 1"));
        assert!(BRIDGE.contains("len(location) != 1"));
        assert!(!BRIDGE.contains("\"attachment_ready\": True"));
        assert!(BRIDGE.contains("elif op == \"attach\":"));
        assert!(BRIDGE.contains("elif op == \"attachment-ready\":"));
    }

    #[test]
    fn desktop_adapter_has_no_cdp_or_dom_dependency() {
        let cargo = include_str!("../Cargo.toml");
        assert!(!cargo.contains("chatgpt-cdp"));
        assert!(!BRIDGE.contains("querySelector"));
        assert!(!BRIDGE.contains("Runtime.evaluate"));
    }
}
