use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone)]
pub struct BrowserLaunch {
    pub pid: u32,
    pub endpoint: String,
    pub profile_dir: PathBuf,
    pub browser_binary: PathBuf,
    pub headed: bool,
}

pub fn find_chromium_binary(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
        bail!("explicit Chromium path does not exist: {}", path.display());
    }

    for candidate in [
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
    ] {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Ok(path);
        }
    }

    bail!("no Chromium/Google Chrome binary found; pass --browser-binary")
}

pub fn launch_chromium(
    browser_binary: &Path,
    profile_dir: &Path,
    port: u16,
    headed: bool,
    initial_url: &str,
) -> Result<BrowserLaunch> {
    std::fs::create_dir_all(profile_dir).with_context(|| {
        format!(
            "failed to create profile directory {}",
            profile_dir.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(profile_dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| {
                format!(
                    "failed to secure profile directory {}",
                    profile_dir.display()
                )
            })?;
    }

    let mut command = Command::new(browser_binary);
    command
        .arg("--remote-debugging-address=127.0.0.1")
        .arg(format!("--remote-debugging-port={port}"))
        .arg(format!("--user-data-dir={}", profile_dir.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-background-networking")
        .arg("--disable-component-update")
        .arg("--disable-sync")
        .arg(initial_url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    if !headed {
        command.arg("--headless=new").arg("--disable-gpu");
    }

    let child = command.spawn().context("failed to start Chromium")?;

    Ok(BrowserLaunch {
        pid: child.id(),
        endpoint: format!("http://127.0.0.1:{port}"),
        profile_dir: profile_dir.to_path_buf(),
        browser_binary: browser_binary.to_path_buf(),
        headed,
    })
}
