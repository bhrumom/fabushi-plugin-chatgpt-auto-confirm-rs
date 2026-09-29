use anyhow::{Context, Result, bail};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

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

#[derive(Debug, Clone)]
pub struct ManagedBrowserConfig {
    pub browser_binary: PathBuf,
    pub profile_dir: PathBuf,
    pub port: u16,
    pub headed: bool,
    pub initial_url: String,
    pub log_dir: PathBuf,
}

pub struct ManagedChromium {
    launch: BrowserLaunch,
    child: Mutex<Option<Child>>,
    profile_lock_path: PathBuf,
    _profile_lock: File,
    stdout_log: PathBuf,
    stderr_log: PathBuf,
}

impl ManagedChromium {
    pub fn launch(config: ManagedBrowserConfig) -> Result<Self> {
        std::fs::create_dir_all(&config.profile_dir)?;
        std::fs::create_dir_all(&config.log_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&config.profile_dir, std::fs::Permissions::from_mode(0o700))?;
        }

        let profile_lock_path = config.profile_dir.join(".fabushi-browser-owner.lock");
        let profile_lock = acquire_profile_lock(&profile_lock_path)?;
        let stdout_log = config.log_dir.join("chromium.stdout.log");
        let stderr_log = config.log_dir.join("chromium.stderr.log");
        let stdout = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&stdout_log)?;
        let stderr = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&stderr_log)?;

        let mut command = Command::new(&config.browser_binary);
        command
            .arg("--remote-debugging-address=127.0.0.1")
            .arg(format!("--remote-debugging-port={}", config.port))
            .arg(format!("--user-data-dir={}", config.profile_dir.display()))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-component-update")
            .arg(&config.initial_url)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        if !config.headed {
            command.arg("--headless=new").arg("--disable-gpu");
        }
        let child = command
            .spawn()
            .context("failed to start managed Chromium")?;
        let launch = BrowserLaunch {
            pid: child.id(),
            endpoint: format!("http://127.0.0.1:{}", config.port),
            profile_dir: config.profile_dir,
            browser_binary: config.browser_binary,
            headed: config.headed,
        };
        write_profile_owner(&profile_lock_path, launch.pid)?;
        Ok(Self {
            launch,
            child: Mutex::new(Some(child)),
            profile_lock_path,
            _profile_lock: profile_lock,
            stdout_log,
            stderr_log,
        })
    }

    pub fn launch_info(&self) -> &BrowserLaunch {
        &self.launch
    }

    pub fn stdout_log(&self) -> &Path {
        &self.stdout_log
    }

    pub fn stderr_log(&self) -> &Path {
        &self.stderr_log
    }

    pub fn is_alive(&self) -> Result<bool> {
        let mut child = self
            .child
            .lock()
            .map_err(|_| anyhow::anyhow!("chromium child mutex poisoned"))?;
        match child.as_mut() {
            Some(child) => Ok(child.try_wait()?.is_none()),
            None => Ok(false),
        }
    }

    pub fn force_terminate(&self) -> Result<()> {
        let mut child = self
            .child
            .lock()
            .map_err(|_| anyhow::anyhow!("chromium child mutex poisoned"))?;
        if let Some(child) = child.as_mut()
            && child.try_wait()?.is_none()
        {
            child.kill().context("force terminate Chromium")?;
            let _ = child.wait();
        }
        *child = None;
        let _ = std::fs::remove_file(&self.profile_lock_path);
        Ok(())
    }
}

impl Drop for ManagedChromium {
    fn drop(&mut self) {
        if let Ok(child) = self.child.get_mut()
            && let Some(child) = child.as_mut()
            && child.try_wait().ok().flatten().is_none()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(&self.profile_lock_path);
    }
}

fn acquire_profile_lock(path: &Path) -> Result<File> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let stale = std::fs::read_to_string(path)
                .ok()
                .and_then(|value| value.trim().parse::<u32>().ok())
                .is_none_or(|pid| !process_exists(pid));
            if !stale {
                bail!("Chromium profile is already owned: {}", path.display());
            }
            std::fs::remove_file(path)
                .with_context(|| format!("remove stale profile lock {}", path.display()))?;
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .with_context(|| format!("acquire profile lock {}", path.display()))
        }
        Err(error) => {
            Err(error).with_context(|| format!("acquire profile lock {}", path.display()))
        }
    }
}

fn write_profile_owner(path: &Path, pid: u32) -> Result<()> {
    let mut file = OpenOptions::new().write(true).truncate(true).open(path)?;
    writeln!(file, "{pid}")?;
    file.sync_data()?;
    Ok(())
}

fn process_exists(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        PathBuf::from(format!("/proc/{pid}")).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        false
    }
}

#[cfg(test)]
mod managed_tests {
    use super::*;

    #[test]
    fn live_profile_owner_cannot_be_reacquired() {
        let root =
            std::env::temp_dir().join(format!("fabushi-browser-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(".lock");
        let _lock = acquire_profile_lock(&path).unwrap();
        write_profile_owner(&path, std::process::id()).unwrap();
        assert!(acquire_profile_lock(&path).is_err());
        drop(_lock);
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn stale_profile_owner_is_reclaimed() {
        let root =
            std::env::temp_dir().join(format!("fabushi-browser-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(".lock");
        std::fs::write(&path, "4294967295\n").unwrap();
        let _lock = acquire_profile_lock(&path).unwrap();
        drop(_lock);
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(root);
    }
}
