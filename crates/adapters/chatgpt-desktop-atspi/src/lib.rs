use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use fabushi_chatgpt_application::ChatSurfacePort;
use fabushi_chatgpt_domain::{ChatSurfaceSnapshot, ReasoningPreset};
use serde::de::DeserializeOwned;
use serde_json::Value;
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
    fn bridge_binds_terminal_copy_after_fabushi_marker() {
        assert!(BRIDGE.contains("marker_info"));
        assert!(BRIDGE.contains("copy_after"));
        assert!(BRIDGE.contains("after = items[marker_index + 1"));
    }

    #[test]
    fn reasoning_effect_uses_semantic_power_control_and_reobserves_positions() {
        assert!(BRIDGE.contains("name == \"Power\""));
        assert!(BRIDGE.contains("KEY_SYM"));
        assert!(BRIDGE.contains("reasoning_position"));
        assert!(BRIDGE.contains("next_position == current"));
    }

    #[test]
    fn desktop_adapter_has_no_cdp_or_dom_dependency() {
        let cargo = include_str!("../Cargo.toml");
        assert!(!cargo.contains("chatgpt-cdp"));
        assert!(!BRIDGE.contains("querySelector"));
        assert!(!BRIDGE.contains("Runtime.evaluate"));
    }
}
