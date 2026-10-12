use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Command;

/// Start a separate Electron app instance through LaunchServices.
/// Authentication stays inside the user-owned desktop client.
pub fn launch_desktop(app: &Path, profile: &Path, port: u16) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("desktop launch requires macOS");
    }
    if !app.join("Contents/MacOS/ChatGPT").is_file() {
        bail!("ChatGPT desktop app not found at {}", app.display());
    }
    if port == 0 {
        bail!("desktop debugging port must be nonzero");
    }
    std::fs::create_dir_all(profile)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(profile, std::fs::Permissions::from_mode(0o700))?;
    }
    let status = Command::new("/usr/bin/open")
        .arg("-n")
        .arg(app)
        .arg("--args")
        .args(desktop_arguments(profile, port))
        .status()
        .context("failed to launch ChatGPT desktop through LaunchServices")?;
    if !status.success() {
        bail!("ChatGPT desktop launch failed: {status}");
    }
    Ok(())
}

fn desktop_arguments(profile: &Path, port: u16) -> Vec<String> {
    vec![
        format!("--user-data-dir={}", profile.display()),
        "--remote-debugging-address=127.0.0.1".into(),
        format!("--remote-debugging-port={port}"),
        "--disable-backgrounding-occluded-windows".into(),
        "--disable-renderer-backgrounding".into(),
        "--disable-background-timer-throttling".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_arguments_keep_profile_as_one_argument_and_bind_loopback() {
        let args = desktop_arguments(Path::new("/tmp/desktop profile"), 9223);
        assert_eq!(args[0], "--user-data-dir=/tmp/desktop profile");
        assert_eq!(args[1], "--remote-debugging-address=127.0.0.1");
        assert_eq!(args[2], "--remote-debugging-port=9223");
        assert_eq!(args.len(), 6);
        assert!(args.contains(&"--disable-background-timer-throttling".into()));
    }
}
