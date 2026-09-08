//! Detached lifecycle hooks for enabled local extensions.

use anyhow::{anyhow, bail, Context, Result};
use kodade_cli_proto::{PluginEventHook, PluginManifest};
use serde::Deserialize;
use std::{
    collections::BTreeMap, fs, path::PathBuf, process::Stdio, sync::OnceLock, time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    sync::{Mutex, Semaphore},
};

const HOOK_TIMEOUT: Duration = Duration::from_secs(30);
const KILL_GRACE: Duration = Duration::from_secs(2);
const MAX_HOOK_OUTPUT: usize = 64 * 1024;
const MAX_LOG_BYTES: u64 = 256 * 1024;
const MAX_CONCURRENT_HOOKS: usize = 4;

#[derive(Clone)]
pub struct Hook {
    pub plugin: String,
    pub directory: PathBuf,
    pub event: String,
    pub command: String,
}

#[derive(Deserialize, Default)]
struct Registry {
    #[serde(default)]
    plugins: BTreeMap<String, Installed>,
}
#[derive(Deserialize)]
struct Installed {
    path: PathBuf,
    enabled: bool,
    version: String,
}

fn root() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .ok_or_else(|| anyhow!("home directory unavailable"))?
        .join(".config/kodade-cli/plugins"))
}

pub fn load() -> Result<Vec<Hook>> {
    let root = root()?;
    let source = match fs::read_to_string(root.join("registry.toml")) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let registry: Registry = toml::from_str(&source).context("parse plugin registry")?;
    let mut hooks = Vec::new();
    for (id, installed) in registry.plugins {
        if !installed.enabled {
            continue;
        }
        let result = load_plugin(&id, &installed);
        let (canonical, manifest) = match result {
            Ok(value) => value,
            Err(error) => {
                eprintln!("Ködade plugin {id} skipped: {error}");
                continue;
            }
        };
        for hook in manifest.startup {
            hooks.push(Hook {
                plugin: id.clone(),
                directory: canonical.clone(),
                event: "startup".into(),
                command: hook.command,
            });
        }
        for PluginEventHook { event, command } in manifest.events {
            hooks.push(Hook {
                plugin: id.clone(),
                directory: canonical.clone(),
                event,
                command,
            });
        }
    }
    Ok(hooks)
}

fn load_plugin(id: &str, installed: &Installed) -> Result<(PathBuf, PluginManifest)> {
    let canonical =
        fs::canonicalize(&installed.path).with_context(|| format!("resolve plugin {id}"))?;
    if !canonical.is_dir() {
        bail!("plugin path is not a directory");
    }
    let manifest: PluginManifest =
        toml::from_str(&fs::read_to_string(canonical.join("kodade-plugin.toml"))?)
            .context("parse plugin manifest")?;
    validate_manifest(&manifest, id, &installed.version)?;
    Ok((canonical, manifest))
}

fn validate_manifest(manifest: &PluginManifest, id: &str, version: &str) -> Result<()> {
    if manifest.id != id || manifest.version != version {
        bail!("plugin registry metadata does not match manifest for {id}");
    }
    kodade_cli_proto::validate_plugin_manifest(manifest, env!("CARGO_PKG_VERSION"))
}

/// Reload on every event so disable, unlink, and manifest changes take effect
/// without interrupting the daemon's live PTYs. A broken plugin is reported and
/// skipped rather than preventing the session from starting.
pub fn run(event: &str, session: String, socket: PathBuf, pane: Option<u64>) {
    let event = event.to_string();
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        // Snapshot-only callers in synchronous tests have no runtime. A real
        // detached daemon always has one; skipping here avoids poisoning its
        // shared state over an optional extension hook.
        eprintln!("Ködade plugin hook skipped outside Tokio runtime");
        return;
    };
    runtime.spawn(async move {
        let hooks = match load() {
            Ok(hooks) => hooks,
            Err(error) => {
                eprintln!("Ködade plugin registry: {error}");
                return;
            }
        };
        for hook in hooks.into_iter().filter(|hook| hook.event == event) {
            let Ok(permit) = hook_semaphore().clone().try_acquire_owned() else {
                eprintln!("Ködade plugin hook queue full; skipping {}", hook.plugin);
                continue;
            };
            let session = session.clone();
            let socket = socket.clone();
            let event = event.clone();
            tokio::spawn(async move {
                let _permit = permit;
                run_hook(hook, event, session, socket, pane).await;
            });
        }
    });
}

async fn run_hook(hook: Hook, event: String, session: String, socket: PathBuf, pane: Option<u64>) {
    let mut command = tokio::process::Command::new("sh");
    command
        .args(["-lc", &hook.command])
        .current_dir(&hook.directory)
        .env("KODADE_PLUGIN", &hook.plugin)
        .env("KODADE_EVENT", &event)
        .env("KODADE_SESSION", session)
        .env("KODADE_SOCKET", socket)
        .env(
            "KODADE_PANE",
            pane.map(|id| id.to_string()).unwrap_or_default(),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let result = run_bounded(&mut command).await;
    let entry = match result {
        Ok((status, stdout, stderr)) => format!(
            "[{event}] status={status}\n{}{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        ),
        Err(error) => format!("[{event}] failed: {error}\n"),
    };
    if let Err(error) = append_log(&hook.plugin, &entry).await {
        eprintln!("Ködade plugin {} log write: {error}", hook.plugin);
    }
}

async fn run_bounded(
    command: &mut tokio::process::Command,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>)> {
    run_bounded_for(command, HOOK_TIMEOUT).await
}

async fn run_bounded_for(
    command: &mut tokio::process::Command,
    timeout: Duration,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>)> {
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().context("start plugin hook")?;
    #[cfg(unix)]
    let pid = child
        .id()
        .ok_or_else(|| anyhow!("plugin hook has no process id"))? as i32;
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let stdout_task = tokio::spawn(read_limited(stdout));
    let stderr_task = tokio::spawn(read_limited(stderr));
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => status.context("wait for plugin hook")?,
        Err(_) => {
            #[cfg(unix)]
            terminate_group(pid, libc::SIGTERM);
            #[cfg(unix)]
            settle_group(pid).await;
            let _ = child.wait().await;
            bail!("hook timed out after {} seconds", timeout.as_secs());
        }
    };
    // A shell may exit while a background child still owns the output pipe.
    // Hooks are bounded units of work, so no descendant may survive the hook.
    #[cfg(unix)]
    terminate_group(pid, libc::SIGTERM);
    #[cfg(unix)]
    settle_group(pid).await;
    let stdout = match tokio::time::timeout(KILL_GRACE, stdout_task).await {
        Ok(result) => result.context("join hook stdout")??,
        Err(_) => {
            #[cfg(unix)]
            terminate_group(pid, libc::SIGKILL);
            Vec::new()
        }
    };
    let stderr = match tokio::time::timeout(KILL_GRACE, stderr_task).await {
        Ok(result) => result.context("join hook stderr")??,
        Err(_) => {
            #[cfg(unix)]
            terminate_group(pid, libc::SIGKILL);
            Vec::new()
        }
    };
    Ok((status, stdout, stderr))
}

async fn read_limited<R: tokio::io::AsyncRead + Unpin>(mut reader: R) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let remaining = MAX_HOOK_OUTPUT.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..count.min(remaining)]);
    }
    Ok(output)
}

async fn append_log(plugin: &str, entry: &str) -> Result<()> {
    let _guard = log_lock().lock().await;
    let logs = root()?.join("logs");
    fs::create_dir_all(&logs)?;
    let path = logs.join(format!("{plugin}.log"));
    let contents = bounded_log(fs::read(&path).unwrap_or_default(), entry.as_bytes());
    fs::write(path, contents)?;
    Ok(())
}

fn bounded_log(mut contents: Vec<u8>, entry: &[u8]) -> Vec<u8> {
    contents.extend_from_slice(entry);
    if contents.len() > MAX_LOG_BYTES as usize {
        contents.drain(..contents.len() - MAX_LOG_BYTES as usize);
    }
    contents
}
fn hook_semaphore() -> &'static std::sync::Arc<Semaphore> {
    static VALUE: OnceLock<std::sync::Arc<Semaphore>> = OnceLock::new();
    VALUE.get_or_init(|| std::sync::Arc::new(Semaphore::new(MAX_CONCURRENT_HOOKS)))
}
fn log_lock() -> &'static Mutex<()> {
    static VALUE: OnceLock<Mutex<()>> = OnceLock::new();
    VALUE.get_or_init(|| Mutex::new(()))
}
#[cfg(unix)]
fn terminate_group(pid: i32, signal: i32) {
    unsafe {
        libc::kill(-pid, signal);
    }
}
#[cfg(unix)]
async fn settle_group(pid: i32) {
    terminate_group(pid, libc::SIGTERM);
    if group_alive(pid) {
        tokio::time::sleep(KILL_GRACE).await;
        terminate_group(pid, libc::SIGKILL);
    }
}
#[cfg(unix)]
fn group_alive(pid: i32) -> bool {
    unsafe {
        libc::kill(-pid, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kodade_cli_proto::{PluginAction, PluginHook, PluginPane};
    #[test]
    fn invalid_hook_manifest_is_rejected_before_execution() {
        let manifest = PluginManifest {
            manifest_version: 1,
            id: "demo".into(),
            name: "Demo".into(),
            version: "1.0.0".into(),
            min_kodade_version: None,
            build: None,
            actions: Vec::<PluginAction>::new(),
            startup: vec![PluginHook {
                command: " ".into(),
            }],
            events: Vec::new(),
            panes: Vec::<PluginPane>::new(),
            link_handlers: vec![],
        };
        assert!(validate_manifest(&manifest, "demo", "1.0.0").is_err());
    }
    #[test]
    fn hook_manifest_enforces_minimum_client_version() {
        let manifest = PluginManifest {
            manifest_version: 1,
            id: "demo".into(),
            name: "Demo".into(),
            version: "1.0.0".into(),
            min_kodade_version: Some("999.0.0".into()),
            build: None,
            actions: vec![],
            startup: vec![],
            events: vec![],
            panes: vec![],
            link_handlers: vec![],
        };
        assert!(validate_manifest(&manifest, "demo", "1.0.0").is_err());
    }

    #[tokio::test]
    async fn timed_out_hook_kills_its_process_group() {
        let marker = std::env::temp_dir().join(format!(
            "kodade-hook-timeout-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut command = tokio::process::Command::new("sh");
        command
            .args([
                "-lc",
                &format!("sleep 1; touch {}", marker.to_string_lossy()),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        assert!(run_bounded_for(&mut command, Duration::from_millis(20))
            .await
            .is_err());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn exited_shell_cannot_leave_a_background_hook_running() {
        let marker = std::env::temp_dir().join(format!(
            "kodade-hook-background-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut command = tokio::process::Command::new("sh");
        command
            .args([
                "-lc",
                &format!(
                    "(trap '' TERM; sleep 5; touch {}) >/dev/null 2>&1 &",
                    marker.to_string_lossy()
                ),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let _ = run_bounded_for(&mut command, Duration::from_secs(1)).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!marker.exists());
    }

    #[test]
    fn log_cap_applies_to_existing_and_single_huge_entries() {
        let result = bounded_log(vec![b'a'; MAX_LOG_BYTES as usize], &[b'b'; 10]);
        assert_eq!(result.len(), MAX_LOG_BYTES as usize);
        assert!(result.ends_with(&[b'b'; 10]));
        let huge = bounded_log(Vec::new(), &vec![b'x'; MAX_LOG_BYTES as usize + 1]);
        assert_eq!(huge.len(), MAX_LOG_BYTES as usize);
    }
}
