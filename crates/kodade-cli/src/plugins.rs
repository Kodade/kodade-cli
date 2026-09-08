//! Local extension registry and manifest validation.

use crate::commands;
use anyhow::{anyhow, bail, Context, Result};
use kodade_cli_proto::{ClientMessage, PluginAction, PluginManifest, PluginPane};
use serde::{Deserialize, Serialize};

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

const MANIFEST: &str = "kodade-plugin.toml";
const REGISTRY: &str = "registry.toml";
const INSTALL_TIMEOUT: Duration = Duration::from_secs(60);
const KILL_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PluginRegistry {
    #[serde(default)]
    pub plugins: BTreeMap<String, InstalledPlugin>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPlugin {
    pub path: PathBuf,
    pub linked: bool,
    pub enabled: bool,
    pub version: String,
}

#[derive(Debug, Clone)]
pub struct LoadedPlugin {
    pub manifest: PluginManifest,
    pub installed: InstalledPlugin,
}

#[derive(Debug, Clone)]
pub struct PaletteAction {
    pub plugin: String,
    pub directory: PathBuf,
    pub action: PluginAction,
}

pub async fn command(
    socket: &Path,
    session: &str,
    remote: bool,
    command: crate::cli::PluginCommand,
) -> Result<()> {
    match command {
        crate::cli::PluginCommand::List { json } => {
            let plugins = installed()?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &plugins
                            .iter()
                            .map(|plugin| (&plugin.manifest, &plugin.installed))
                            .collect::<Vec<_>>()
                    )?
                );
            } else {
                for plugin in plugins {
                    println!(
                        "{:<20} {:<10} {}",
                        plugin.manifest.id,
                        if plugin.installed.enabled {
                            "enabled"
                        } else {
                            "disabled"
                        },
                        plugin.installed.path.display()
                    );
                }
            }
            Ok(())
        }
        crate::cli::PluginCommand::Link { path } => {
            let plugin = link(path)?;
            println!("linked {} {}", plugin.id, plugin.version);
            Ok(())
        }
        crate::cli::PluginCommand::Install { source } => {
            let plugin = install(&source).await?;
            println!("installed {} {}", plugin.id, plugin.version);
            Ok(())
        }
        crate::cli::PluginCommand::Enable { id } => {
            set_enabled(&id, true)?;
            println!("enabled {id}");
            Ok(())
        }
        crate::cli::PluginCommand::Disable { id } => {
            set_enabled(&id, false)?;
            println!("disabled {id}");
            Ok(())
        }
        crate::cli::PluginCommand::Unlink { id } => {
            unlink(&id)?;
            println!("unlinked {id}");
            Ok(())
        }
        crate::cli::PluginCommand::Uninstall { id } => {
            uninstall(&id)?;
            println!("uninstalled {id}");
            Ok(())
        }
        crate::cli::PluginCommand::Pane { id, pane } => {
            if remote {
                anyhow::bail!("plugin pane is local-only; install the extension on the remote host and run it there");
            }
            let (plugin, pane) = self::pane(&id, &pane)?;
            let layout =
                commands::layout(commands::request(socket, commands::layout_query()).await?)?;
            let workspace = layout
                .workspaces
                .iter()
                .find(|item| item.id == layout.active_workspace)
                .map(|item| item.name.as_str())
                .unwrap_or("");
            let focused_pane = layout
                .panes
                .iter()
                .find(|item| item.focused)
                .map(|item| item.id.0.to_string())
                .unwrap_or_default();
            commands::layout(
                commands::request(
                    socket,
                    ClientMessage::NewPane {
                        workspace: None,
                        tab: None,
                        split: None,
                        command: Some(pane_command(
                            &plugin.manifest.id,
                            &plugin.installed.path,
                            &pane.command,
                            None,
                            workspace,
                            &focused_pane,
                        )),
                        name: Some(format!("{} · {}", plugin.manifest.name, pane.name)),
                    },
                )
                .await?,
            )?;
            Ok(())
        }
        crate::cli::PluginCommand::Run { id, action } => {
            if remote {
                anyhow::bail!("plugin run is local-only; install the extension on the remote host and run it there");
            }
            let (plugin, action) = self::action(&id, &action)?;
            if action.pane {
                let layout =
                    commands::layout(commands::request(socket, commands::layout_query()).await?)?;
                let workspace = layout
                    .workspaces
                    .iter()
                    .find(|item| item.id == layout.active_workspace)
                    .map(|item| item.name.as_str())
                    .unwrap_or("");
                let focused_pane = layout
                    .panes
                    .iter()
                    .find(|item| item.focused)
                    .map(|item| item.id.0.to_string())
                    .unwrap_or_default();
                commands::layout(
                    commands::request(
                        socket,
                        ClientMessage::NewPane {
                            workspace: None,
                            tab: None,
                            split: None,
                            command: Some(pane_command(
                                &plugin.manifest.id,
                                &plugin.installed.path,
                                &action.command,
                                Some(&action.id),
                                workspace,
                                &focused_pane,
                            )),
                            name: Some(format!("{} · {}", plugin.manifest.name, action.name)),
                        },
                    )
                    .await?,
                )?;
                return Ok(());
            }
            let layout =
                commands::layout(commands::request(socket, commands::layout_query()).await?)?;
            let workspace = layout
                .workspaces
                .iter()
                .find(|item| item.id == layout.active_workspace)
                .map(|item| item.name.as_str())
                .unwrap_or("");
            let pane = layout
                .panes
                .iter()
                .find(|item| item.focused)
                .map(|item| item.id.0.to_string())
                .unwrap_or_default();
            let mut command = tokio::process::Command::new("sh");
            command
                .args(["-lc", &action.command])
                .current_dir(&plugin.installed.path)
                .env("KODADE_PLUGIN", &plugin.manifest.id)
                .env("KODADE_ACTION", &action.id)
                .env("KODADE_SESSION", session)
                .env("KODADE_SOCKET", socket)
                .env("KODADE_WORKSPACE", workspace)
                .env("KODADE_TAB", layout.active_tab.0.to_string())
                .env("KODADE_PANE", pane);
            let status =
                run_bounded_command(&mut command, Duration::from_secs(30), "plugin action").await?;
            if !status.success() {
                anyhow::bail!("plugin action {} failed with {status}", action.id);
            }
            Ok(())
        }
    }
}

fn root() -> PathBuf {
    crate::config::config_path()
        .parent()
        .expect("config file has parent")
        .join("plugins")
}

fn registry_path() -> PathBuf {
    root().join(REGISTRY)
}

fn managed_root() -> PathBuf {
    root().join("managed")
}

pub fn load_registry() -> Result<PluginRegistry> {
    let path = registry_path();
    match fs::read_to_string(&path) {
        Ok(source) => toml::from_str(&source).context("parse plugin registry"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(PluginRegistry::default()),
        Err(error) => Err(error.into()),
    }
}

fn write_registry(registry: &PluginRegistry) -> Result<()> {
    crate::atomic_file::write(
        &registry_path(),
        toml::to_string_pretty(registry)?.as_bytes(),
    )
}

pub fn parse_manifest(path: &Path) -> Result<PluginManifest> {
    let source = fs::read_to_string(path.join(MANIFEST))
        .with_context(|| format!("read {}", path.join(MANIFEST).display()))?;
    let manifest: PluginManifest = toml::from_str(&source).context("parse plugin manifest")?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

pub fn validate_manifest(manifest: &PluginManifest) -> Result<()> {
    kodade_cli_proto::validate_plugin_manifest(manifest, env!("CARGO_PKG_VERSION"))
}

pub fn link(path: PathBuf) -> Result<PluginManifest> {
    let path = fs::canonicalize(&path)
        .with_context(|| format!("resolve plugin directory {}", path.display()))?;
    if !path.is_dir() {
        bail!("plugin path is not a directory: {}", path.display());
    }
    let manifest = parse_manifest(&path)?;
    let mut registry = load_registry()?;
    if registry.plugins.contains_key(&manifest.id) {
        bail!("plugin {} is already registered", manifest.id);
    }
    registry.plugins.insert(
        manifest.id.clone(),
        InstalledPlugin {
            path,
            linked: true,
            enabled: true,
            version: manifest.version.clone(),
        },
    );
    write_registry(&registry)?;
    Ok(manifest)
}

pub fn set_enabled(id: &str, enabled: bool) -> Result<()> {
    let mut registry = load_registry()?;
    let plugin = registry
        .plugins
        .get_mut(id)
        .ok_or_else(|| anyhow!("plugin {id} is not installed"))?;
    plugin.enabled = enabled;
    write_registry(&registry)
}

pub fn unlink(id: &str) -> Result<()> {
    let mut registry = load_registry()?;
    let plugin = registry
        .plugins
        .get(id)
        .ok_or_else(|| anyhow!("plugin {id} is not installed"))?;
    if !plugin.linked {
        bail!("plugin {id} is managed; use plugin uninstall");
    }
    registry.plugins.remove(id);
    write_registry(&registry)
}

pub fn uninstall(id: &str) -> Result<()> {
    let mut registry = load_registry()?;
    let plugin = registry
        .plugins
        .get(id)
        .ok_or_else(|| anyhow!("plugin {id} is not installed"))?;
    if plugin.linked {
        bail!("plugin {id} is linked; use plugin unlink (its directory is protected)");
    }
    let install_root = managed_install_root(&plugin.path)?;
    if install_root.exists() {
        fs::remove_dir_all(&install_root)
            .with_context(|| format!("remove managed plugin {}", install_root.display()))?;
    }
    registry.plugins.remove(id);
    write_registry(&registry)
}

fn managed_install_root(path: &Path) -> Result<PathBuf> {
    let managed = fs::canonicalize(managed_root()).context("resolve managed plugin directory")?;
    managed_install_root_in(&managed, path)
}

fn managed_install_root_in(managed: &Path, path: &Path) -> Result<PathBuf> {
    let path = fs::canonicalize(path).context("resolve managed plugin path")?;
    let mut components = path
        .strip_prefix(managed)
        .map_err(|_| anyhow!("plugin is not in Ködade's managed plugin directory"))?
        .components();
    let first = components
        .next()
        .ok_or_else(|| anyhow!("plugin path is the managed directory"))?;
    let std::path::Component::Normal(first) = first else {
        bail!("invalid managed plugin path");
    };
    Ok(managed.join(first))
}

pub fn installed() -> Result<Vec<LoadedPlugin>> {
    load_registry()?
        .plugins
        .into_iter()
        .map(|(id, installed)| {
            let manifest =
                parse_manifest(&installed.path).with_context(|| format!("load plugin {id}"))?;
            if manifest.id != id {
                bail!(
                    "plugin registry id {id} does not match manifest {}",
                    manifest.id
                );
            }
            Ok(LoadedPlugin {
                manifest,
                installed,
            })
        })
        .collect()
}

pub fn action(id: &str, action: &str) -> Result<(LoadedPlugin, PluginAction)> {
    let plugin = installed()?
        .into_iter()
        .find(|plugin| plugin.manifest.id == id)
        .ok_or_else(|| anyhow!("plugin {id} is not installed"))?;
    if !plugin.installed.enabled {
        bail!("plugin {id} is disabled");
    }
    let action = plugin
        .manifest
        .actions
        .iter()
        .find(|item| item.id == action)
        .cloned()
        .ok_or_else(|| anyhow!("plugin {id} has no action {action}"))?;
    Ok((plugin, action))
}

pub fn pane(id: &str, name: &str) -> Result<(LoadedPlugin, PluginPane)> {
    let plugin = installed()?
        .into_iter()
        .find(|plugin| plugin.manifest.id == id)
        .ok_or_else(|| anyhow!("plugin {id} is not installed"))?;
    if !plugin.installed.enabled {
        bail!("plugin {id} is disabled");
    }
    let pane = plugin
        .manifest
        .panes
        .iter()
        .find(|item| item.name == name)
        .cloned()
        .ok_or_else(|| anyhow!("plugin {id} has no pane {name}"))?;
    Ok((plugin, pane))
}

pub fn palette_actions() -> Result<Vec<PaletteAction>> {
    Ok(installed()?
        .into_iter()
        .filter(|plugin| plugin.installed.enabled)
        .flat_map(|plugin| {
            plugin
                .manifest
                .actions
                .into_iter()
                .map(move |action| PaletteAction {
                    plugin: plugin.manifest.id.clone(),
                    directory: plugin.installed.path.clone(),
                    action,
                })
        })
        .collect())
}

/// Build a shell command for a pane action without interpolating plugin data
/// into shell source. `env` supplies the context, then the fixed wrapper moves
/// into the plugin directory before invoking the declared command.
pub fn pane_command(
    plugin: &str,
    directory: &Path,
    command: &str,
    action: Option<&str>,
    workspace: &str,
    pane: &str,
) -> Vec<String> {
    let mut args = vec![
        "env".into(),
        format!("KODADE_PLUGIN={plugin}"),
        format!("KODADE_PLUGIN_DIR={}", directory.to_string_lossy()),
        format!("KODADE_PLUGIN_COMMAND={command}"),
        format!("KODADE_WORKSPACE={workspace}"),
        // The daemon supplies KODADE_PANE for the newly created pane. Keep the
        // originating focus separate so a plugin never mistakes it for itself.
        format!("KODADE_TARGET_PANE={pane}"),
    ];
    if let Some(action) = action {
        args.push(format!("KODADE_ACTION={action}"));
    }
    args.extend([
        "sh".into(),
        "-lc".into(),
        "cd \"$KODADE_PLUGIN_DIR\" && sh -lc \"$KODADE_PLUGIN_COMMAND\"".into(),
    ]);
    args
}

struct ManagedStaging {
    path: PathBuf,
    published: bool,
}

impl ManagedStaging {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            published: false,
        }
    }
    fn publish(&mut self, destination: &Path) -> Result<()> {
        fs::rename(&self.path, destination)
            .with_context(|| format!("publish managed plugin {}", destination.display()))?;
        self.published = true;
        Ok(())
    }
}
impl Drop for ManagedStaging {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

pub async fn install(source: &str) -> Result<PluginManifest> {
    let (owner, repo, subdir) = github_source(source)?;
    let destination = managed_root().join(format!("{owner}-{repo}"));
    if destination.exists() {
        bail!(
            "managed plugin source already exists: {}",
            destination.display()
        );
    }
    let mut registry = load_registry()?;
    fs::create_dir_all(managed_root())?;
    let staging_path = managed_root().join(format!(
        ".{owner}-{repo}-install-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    let mut staging = ManagedStaging::new(staging_path);
    let mut clone = tokio::process::Command::new("git");
    clone.args([
        "clone",
        "--depth",
        "1",
        &format!("https://github.com/{owner}/{repo}.git"),
        staging.path.to_string_lossy().as_ref(),
    ]);
    let status = run_bounded_command(&mut clone, INSTALL_TIMEOUT, "GitHub clone").await?;
    if !status.success() {
        bail!("git clone failed for {owner}/{repo}");
    }
    let staged_plugin = subdir
        .map(|subdir| staging.path.join(subdir))
        .unwrap_or_else(|| staging.path.clone());
    let manifest = parse_manifest(&staged_plugin)?;
    if registry.plugins.contains_key(&manifest.id) {
        bail!("plugin {} is already registered", manifest.id);
    }
    if let Some(build) = &manifest.build {
        let mut build_command = tokio::process::Command::new("sh");
        build_command
            .args(["-lc", build])
            .current_dir(&staged_plugin);
        let status =
            run_bounded_command(&mut build_command, INSTALL_TIMEOUT, "plugin build").await?;
        if !status.success() {
            bail!("plugin build failed");
        }
    }
    staging.publish(&destination)?;
    let path = subdir
        .map(|subdir| destination.join(subdir))
        .unwrap_or_else(|| destination.clone());
    registry.plugins.insert(
        manifest.id.clone(),
        InstalledPlugin {
            path,
            linked: false,
            enabled: true,
            version: manifest.version.clone(),
        },
    );
    if let Err(error) = write_registry(&registry) {
        let _ = fs::remove_dir_all(&destination);
        return Err(error);
    }
    Ok(manifest)
}

/// Runs an installer child in its own process group. A timeout terminates the
/// whole group, reaps its leader, and returns an actionable failure instead of
/// leaving an unattended install or build behind.
pub async fn run_bounded_command(
    command: &mut tokio::process::Command,
    timeout: Duration,
    label: &str,
) -> Result<std::process::ExitStatus> {
    command.stdin(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().with_context(|| format!("start {label}"))?;
    let pid = child
        .id()
        .ok_or_else(|| anyhow!("{label} has no process id"))? as i32;
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => {
            let status = status.with_context(|| format!("wait for {label}"))?;
            settle_group(pid).await;
            Ok(status)
        }
        Err(_) => {
            terminate_group(pid, libc::SIGTERM);
            settle_group(pid).await;
            let _ = child.wait().await;
            bail!("{label} timed out after {} seconds", timeout.as_secs());
        }
    }
}

async fn settle_group(pid: i32) {
    // The leader can exit on TERM while a grandchild ignores it. Check the
    // process group independently, then always escalate after the grace.
    terminate_group(pid, libc::SIGTERM);
    if group_alive(pid) {
        tokio::time::sleep(KILL_GRACE).await;
        terminate_group(pid, libc::SIGKILL);
    }
}

fn group_alive(pid: i32) -> bool {
    unsafe {
        libc::kill(-pid, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

fn terminate_group(pid: i32, signal: i32) {
    // The child made itself group leader in pre_exec; a negative pid targets
    // the entire group, including shell descendants from a plugin build.
    unsafe {
        libc::kill(-pid, signal);
    }
}

fn github_source(source: &str) -> Result<(&str, &str, Option<&str>)> {
    let parts: Vec<_> = source.split('/').collect();
    if !(2..=3).contains(&parts.len()) || parts.iter().any(|part| !valid_path_component(part)) {
        bail!("GitHub source must be OWNER/REPO[/SUBDIR] using safe path components");
    }
    Ok((parts[0], parts[1], parts.get(2).copied()))
}

fn valid_path_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn manifest() -> PluginManifest {
        PluginManifest {
            manifest_version: 1,
            id: "demo-plugin".into(),
            name: "Demo".into(),
            version: "0.1.0".into(),
            min_kodade_version: None,
            build: None,
            actions: vec![PluginAction {
                id: "hello".into(),
                name: "Hello".into(),
                command: "printf hello".into(),
                description: String::new(),
                pane: false,
            }],
            startup: vec![],
            events: vec![],
            panes: vec![],
        }
    }
    #[test]
    fn manifest_rejects_unsupported_versions_and_duplicate_actions() {
        let mut invalid = manifest();
        invalid.manifest_version = 2;
        assert!(validate_manifest(&invalid).is_err());
        let mut duplicate = manifest();
        duplicate.actions.push(duplicate.actions[0].clone());
        assert!(validate_manifest(&duplicate).is_err());
        assert!(validate_manifest(&manifest()).is_ok());
    }
    #[test]
    fn github_sources_are_constrained() {
        assert_eq!(
            github_source("owner/repo/subdir").unwrap(),
            ("owner", "repo", Some("subdir"))
        );
        assert!(github_source("Kodade/ExamplePlugin").is_ok());
        assert!(github_source("owner/../repo").is_err());
        assert!(github_source("https://github.com/owner/repo").is_err());
    }

    #[test]
    fn pane_wrapper_preserves_context_without_interpolating_command() {
        let command = pane_command(
            "demo-plugin",
            Path::new("/tmp/demo plugin"),
            "printf '%s' \"$KODADE_PLUGIN\"",
            Some("hello"),
            "work",
            "42",
        );
        assert_eq!(command[0], "env");
        assert!(command.contains(&"KODADE_PLUGIN=demo-plugin".into()));
        assert!(command.contains(&"KODADE_ACTION=hello".into()));
        assert!(command.contains(&"KODADE_TARGET_PANE=42".into()));
        assert!(!command
            .iter()
            .any(|value| value.starts_with("KODADE_PANE=")));
        assert!(command.last().unwrap().contains("KODADE_PLUGIN_COMMAND"));
    }

    #[tokio::test]
    async fn timed_out_installer_kills_shell_descendants() {
        let marker = std::env::temp_dir().join(format!(
            "kodade-plugin-timeout-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut command = tokio::process::Command::new("sh");
        command.args([
            "-lc",
            &format!("sleep 1; touch {}", marker.to_string_lossy()),
        ]);
        assert!(
            run_bounded_command(&mut command, Duration::from_millis(20), "test")
                .await
                .is_err()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!marker.exists(), "timed out descendant escaped its group");
    }

    #[test]
    fn failed_install_staging_is_removed_before_publish() {
        let path = std::env::temp_dir().join(format!(
            "kodade-plugin-stage-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        {
            let _staging = ManagedStaging::new(path.clone());
        }
        assert!(!path.exists());
    }

    #[test]
    fn managed_subdir_resolves_to_the_whole_owned_clone() {
        let root = std::env::temp_dir().join(format!("kodade-managed-{}", std::process::id()));
        let clone = root.join("owner-repo");
        let subdir = clone.join("plugin");
        fs::create_dir_all(&subdir).unwrap();
        let managed = fs::canonicalize(&root).unwrap();
        assert_eq!(
            managed_install_root_in(&managed, &subdir).unwrap(),
            clone.canonicalize().unwrap()
        );
        assert!(managed_install_root_in(&managed, Path::new("/tmp")).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
