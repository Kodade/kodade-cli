//! Agent integration configuration. User-owned hooks and settings survive updates.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::{fs, path::Path};

/// Integrations that install a lifecycle hook / notify entry for a known agent.
pub const INTEGRATIONS: &[&str] = &["claude-code", "codex", "gemini-cli", "opencode", "pi"];

/// Marker shared by every Ködade-managed hook command. It is deliberately
/// distinct from a user's arbitrary `kodade-cli` invocation.
const REPORT_PREFIX: &str = "KODADE_INTEGRATION=kodade-cli;";
const LEGACY_REPORT_PREFIX: &str = "kodade-cli agent report $KODADE_PANE ";

/// Command each hook/notify entry runs; `state` is the state it reports.
fn report_command(state: &str, source: &str, agent: &str, _payload_key: &str) -> String {
    format!(
        "{REPORT_PREFIX} if [ -n \"${{KODADE_PANE:-}}\" ] && [ -n \"${{KODADE_SOCKET:-}}\" ]; then \"${{KODADE_BIN:-kodade-cli}}\" agent report \"$KODADE_PANE\" {state} --source {source} --native-agent {agent} --hook-json >/dev/null 2>&1 || true; fi"
    )
}

fn is_report_command(command: &str) -> bool {
    command.starts_with(REPORT_PREFIX) || command.starts_with(LEGACY_REPORT_PREFIX)
}

/// List every known integration and whether its config directory is present.
pub fn integrate_list() -> Result<()> {
    let home = dirs::home_dir();
    for agent in INTEGRATIONS {
        let (target, mechanism) = match *agent {
            "claude-code" => (".claude/settings.json", "hooks"),
            "codex" => (".codex/hooks.json", "hooks"),
            "gemini-cli" => (".gemini/settings.json", "hooks"),
            "opencode" => (
                ".config/opencode/plugins/kodade-cli-agent-state.js",
                "plugin (official docs; fixture-tested)",
            ),
            "pi" => (
                ".pi/agent/extensions/kodade-cli-agent-state.ts",
                "extension (Pi 0.85.1 locally verified)",
            ),
            _ => continue,
        };
        let path = home.as_ref().map(|home| home.join(target));
        let available = match &path {
            // "available" = the agent's config directory exists on this machine.
            Some(path) => path.parent().map(|parent| parent.exists()).unwrap_or(false),
            None => false,
        };
        let status = if available { "available" } else { "not found" };
        let shown = path
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| target.into());
        println!("{agent:<12} {status:<10} {shown} ({mechanism})");
    }
    Ok(())
}

pub fn integrate_claude_code(write: bool) -> Result<()> {
    let snippet = claude_hooks();
    if !write {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "hooks": snippet }))?
        );
        return Ok(());
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    let path = home.join(".claude/settings.json");
    merge_hook_settings(&path, &claude_hooks())?;
    println!("installed Claude Code hooks in {}", path.display());
    Ok(())
}

pub fn unintegrate_claude_code() -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    let path = home.join(".claude/settings.json");
    remove_hook_settings(&path)?;
    println!("removed Ködade Claude Code hooks from {}", path.display());
    Ok(())
}

/// Codex hooks are a separate JSON document. Keeping Ködade entries there
/// preserves any existing `notify` command in config.toml.
fn codex_hooks() -> Value {
    json!({
        "Stop": [{ "hooks": [{ "type": "command", "command": report_command("done", "kodade:codex", "codex", "thread_id") }] }],
        "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": report_command("working", "kodade:codex", "codex", "thread_id") }] }],
        "PermissionRequest": [{ "hooks": [{ "type": "command", "command": report_command("blocked", "kodade:codex", "codex", "thread_id") }] }]
    })
}

pub fn integrate_codex(write: bool, _force: bool) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    let path = home.join(".codex/hooks.json");
    if !write {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "hooks": codex_hooks() }))?
        );
        return Ok(());
    }
    merge_hook_settings(&path, &codex_hooks())?;
    println!("installed Codex hooks in {}", path.display());
    Ok(())
}

pub fn unintegrate_codex() -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    let path = home.join(".codex/hooks.json");
    remove_hook_settings(&path)?;
    println!("removed Ködade Codex hooks from {}", path.display());
    Ok(())
}

/// Gemini has its own lifecycle event names; see geminicli.com/docs/hooks/.
/// Its notification event covers tool permission requests.
fn gemini_hooks() -> Value {
    json!({
        "AfterAgent": [{ "matcher": "*", "hooks": [{ "type": "command", "command": report_command("done", "kodade:gemini-cli", "gemini", "session_id") }] }],
        "BeforeAgent": [{ "matcher": "*", "hooks": [{ "type": "command", "command": report_command("working", "kodade:gemini-cli", "gemini", "session_id") }] }],
        "Notification": [{ "matcher": "*", "hooks": [{ "type": "command", "command": report_command("blocked", "kodade:gemini-cli", "gemini", "session_id") }] }]
    })
}

pub fn integrate_gemini(write: bool, _force: bool) -> Result<()> {
    if !write {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "hooks": gemini_hooks() }))?
        );
        return Ok(());
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    let path = home.join(".gemini/settings.json");
    merge_hook_settings(&path, &gemini_hooks())?;
    println!("installed Gemini CLI hooks in {}", path.display());
    Ok(())
}

pub fn unintegrate_gemini() -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    let path = home.join(".gemini/settings.json");
    remove_hook_settings(&path)?;
    println!("removed Ködade Gemini CLI hooks from {}", path.display());
    Ok(())
}

pub fn integrate_pi(write: bool) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    let root = std::env::var_os("PI_CODING_AGENT_DIR")
        .map(Into::into)
        .unwrap_or_else(|| home.join(".pi/agent"));
    let path = root.join("extensions/kodade-cli-agent-state.ts");
    if !write {
        println!("{}", pi_extension());
        return Ok(());
    }
    if !root.exists() {
        bail!("Pi agent directory {} does not exist", root.display());
    }
    write_owned_file(&path, pi_extension())?;
    println!("installed Pi extension in {}", path.display());
    Ok(())
}

pub fn unintegrate_pi() -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    let root = std::env::var_os("PI_CODING_AGENT_DIR")
        .map(Into::into)
        .unwrap_or_else(|| home.join(".pi/agent"));
    remove_owned_file(&root.join("extensions/kodade-cli-agent-state.ts"))?;
    Ok(())
}

pub fn integrate_opencode(write: bool) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    let path = home.join(".config/opencode/plugins/kodade-cli-agent-state.js");
    if !write {
        println!("{}", opencode_plugin());
        return Ok(());
    }
    if !path
        .parent()
        .and_then(Path::parent)
        .is_some_and(Path::exists)
    {
        bail!(
            "OpenCode config directory {} does not exist",
            path.parent().unwrap().parent().unwrap().display()
        );
    }
    write_owned_file(&path, opencode_plugin())?;
    println!("installed OpenCode plugin in {}", path.display());
    Ok(())
}

pub fn unintegrate_opencode() -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    remove_owned_file(&home.join(".config/opencode/plugins/kodade-cli-agent-state.js"))
}

fn write_owned_file(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    crate::atomic_file::write(path, contents.as_bytes())
}

fn remove_owned_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Pi 0.85.1's installed extension documentation verifies these lifecycle
/// events and the global extension location. It stays silent outside a TUI pane.
fn pi_extension() -> &'static str {
    r#"// Installed by Ködade CLI. Pi 0.85.1 extension API.
import { spawn } from "node:child_process";
const enabled = () => process.env.KODADE_PANE && process.env.KODADE_SOCKET;
function report(state, session, path) {
  if (!enabled()) return;
  const args = ["agent", "report", process.env.KODADE_PANE, state, "--source", "kodade:pi", "--native-agent", "pi"];
  if (typeof path === "string" && path.startsWith("/")) args.push("--native-session-path", path);
  else if (typeof session === "string" && session.length) args.push("--native-session-id", session);
  const child = spawn(process.env.KODADE_BIN || "kodade-cli", args, { detached: true, stdio: "ignore" });
  child.on("error", () => {});
  child.unref();
}
export default function (pi) {
  if (!enabled()) return;
  let tui = false;
  let session, path;
  function updateSession(ctx) {
    try { session = ctx?.sessionManager?.getSessionId?.(); } catch { session = undefined; }
    try { path = ctx?.sessionManager?.getSessionFile?.(); } catch { path = undefined; }
  }
  pi.on("session_start", (_event, ctx) => { tui = ctx?.mode === "tui"; updateSession(ctx); if (tui) report("idle", session, path); });
  pi.on("agent_start", (_event, ctx) => { updateSession(ctx); if (tui) report("working", session, path); });
  pi.on("agent_settled", (_event, ctx) => { updateSession(ctx); if (tui) report("done", session, path); });
}
"#
}

/// OpenCode documents auto-loaded local plugins and `session.status` events.
/// The adapter deliberately reports only the documented busy/idle states.
fn opencode_plugin() -> &'static str {
    r#"// Installed by Ködade CLI. OpenCode plugin API (official docs fixture).
import { spawn } from "node:child_process";
const enabled = () => process.env.KODADE_PANE && process.env.KODADE_SOCKET;
function report(state, session) {
  if (!enabled()) return;
  const args = ["agent", "report", process.env.KODADE_PANE, state, "--source", "kodade:opencode", "--native-agent", "opencode"];
  if (typeof session === "string" && session.length) args.push("--native-session-id", session);
  const child = spawn(process.env.KODADE_BIN || "kodade-cli", args, { detached: true, stdio: "ignore" });
  child.on("error", () => {});
  child.unref();
}
export const KodadeCli = async () => ({
  event: async ({ event }) => {
    if (!enabled() || event?.type !== "session.status") return;
    const status = event.properties?.status?.type;
    const session = event.properties?.sessionID || event.properties?.sessionId;
    if (status === "busy" || status === "retry") report("working", session);
    else if (status === "idle") report("done", session);
  },
});
"#
}

/// Opt-in manifest refresh: download the manifest index and each listed file
/// from the repo's `main` into the user override directory. This is the only
/// command that ever touches the network.
pub fn update_manifests() -> Result<()> {
    const BASE: &str = "https://raw.githubusercontent.com/Kodade/kodade-cli/main/crates/kodade-cli-daemon/manifests/";
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    let dir = home.join(".config/kodade-cli/agent-detection");
    fs::create_dir_all(&dir).context("create agent-detection directory")?;
    let index = curl(&format!("{BASE}index.txt"))?;
    let mut downloads = Vec::new();
    for name in manifest_names(&index)? {
        let contents = curl(&format!("{BASE}{name}"))?;
        validate_download(name, &contents)?;
        downloads.push((name, contents));
    }
    // Download and validate everything before changing a working installation.
    for (name, contents) in downloads {
        let path = dir.join(name);
        crate::atomic_file::write(&path, contents.as_bytes())?;
        println!("wrote {}", path.display());
    }
    Ok(())
}

fn validate_download(name: &str, contents: &str) -> Result<()> {
    let parsed = kodade_cli_daemon::validate_agent_manifest(contents)?;
    if Some(parsed.as_str()) != name.strip_suffix(".toml") {
        bail!("manifest name does not match {name}");
    }
    Ok(())
}

fn manifest_names(index: &str) -> Result<Vec<&str>> {
    let names: Vec<_> = index
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if names.is_empty() || names.len() > 128 {
        bail!("manifest index must contain 1–128 entries");
    }
    let mut seen = std::collections::HashSet::new();
    for name in &names {
        let valid = name.strip_suffix(".toml").is_some_and(|stem| {
            !stem.is_empty()
                && stem
                    .bytes()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == b'-' || ch == b'_')
        });
        if !valid || !seen.insert(*name) {
            bail!("invalid or duplicate manifest filename: {name}");
        }
    }
    Ok(names)
}

/// Fetch a URL with the system `curl` so no HTTP crate enters the dependency set.
fn curl(url: &str) -> Result<String> {
    let output = std::process::Command::new("curl")
        .args([
            "-fsSL",
            "--connect-timeout",
            "10",
            "--max-time",
            "30",
            "--max-filesize",
            "1048576",
            url,
        ])
        .output()
        .context("run curl")?;
    if !output.status.success() {
        bail!("curl failed for {url}");
    }
    String::from_utf8(output.stdout).context("curl returned non-UTF-8 data")
}

fn claude_hooks() -> Value {
    json!({
        "Stop": [{ "hooks": [{ "type": "command", "command": report_command("done", "kodade:claude-code", "claude", "session_id") }] }],
        "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": report_command("working", "kodade:claude-code", "claude", "session_id") }] }],
        "Notification": [{ "hooks": [{ "type": "command", "command": report_command("blocked", "kodade:claude-code", "claude", "session_id") }] }]
    })
}

/// Merge a JSON hooks map into a Claude-style `settings.json` without dropping
/// existing keys or re-adding a hook whose command is already present.
fn merge_hook_settings(path: &Path, new_hooks: &Value) -> Result<()> {
    let mut settings: Value = match fs::read_to_string(path) {
        Ok(source) => serde_json::from_str(&source).context("parse settings.json")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error.into()),
    };
    let hooks = settings
        .as_object_mut()
        .ok_or_else(|| anyhow!("settings.json must be an object"))?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow!("hooks must be an object"))?;
    for destination in hooks.values_mut().filter_map(Value::as_array_mut) {
        // Replace only our hooks; users can share an entry and its matcher.
        destination.retain_mut(|entry| {
            let Some(nested) = entry["hooks"].as_array_mut() else {
                return true;
            };
            let before = nested.len();
            nested.retain(|hook| !hook["command"].as_str().is_some_and(is_report_command));
            nested.len() == before || !nested.is_empty()
        });
    }
    // Empty obsolete managed events can be removed; unrelated values survive.
    hooks.retain(|_, entries| !entries.as_array().is_some_and(Vec::is_empty));
    for (event, entries) in new_hooks.as_object().expect("hooks object") {
        let destination = hooks
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| anyhow!("hook event must be an array"))?;
        destination.extend(entries.as_array().expect("entries").iter().cloned());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    crate::atomic_file::write(
        path,
        format!("{}\n", serde_json::to_string_pretty(&settings)?).as_bytes(),
    )?;
    Ok(())
}

/// Remove only command entries generated by Ködade. Empty event arrays are
/// pruned, while every user setting, matcher, and sibling hook remains.
fn remove_hook_settings(path: &Path) -> Result<()> {
    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut settings: Value = serde_json::from_str(&source).context("parse settings.json")?;
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(());
    };
    for destination in hooks.values_mut().filter_map(Value::as_array_mut) {
        destination.retain_mut(|entry| {
            let Some(nested) = entry["hooks"].as_array_mut() else {
                return true;
            };
            nested.retain(|hook| !hook["command"].as_str().is_some_and(is_report_command));
            !nested.is_empty()
        });
    }
    hooks.retain(|_, entries| !entries.as_array().is_some_and(Vec::is_empty));
    crate::atomic_file::write(
        path,
        format!("{}\n", serde_json::to_string_pretty(&settings)?).as_bytes(),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downloaded_manifest_must_match_the_daemon_schema() {
        assert!(validate_download("test.toml", "name='test'").is_err());
        assert!(validate_download(
            "test.toml",
            "name='test'\ndisplay='Test'\n[[rule]]\nstate='bogus'\nany=['yes']"
        )
        .is_err());
        assert!(validate_download(
            "test.toml",
            "name='test'\ndisplay='Test'\n[[rule]]\nstate='blocked'\nany=['']"
        )
        .is_err());
        assert!(
            validate_download("test.toml", "name='test'\ndisplay='Test'\nprocess=['test']").is_ok()
        );
    }

    #[test]
    fn gemini_uses_its_documented_lifecycle_events() {
        let hooks = gemini_hooks();
        assert!(hooks.get("BeforeAgent").is_some());
        assert!(hooks.get("AfterAgent").is_some());
        assert!(hooks.get("Stop").is_none());
        assert!(hooks.get("UserPromptSubmit").is_none());
    }

    #[test]
    fn manifest_index_rejects_traversal_and_duplicates() {
        for index in [
            "../config.toml",
            "/tmp/config.toml",
            "codex.toml\ncodex.toml",
            "",
        ] {
            assert!(manifest_names(index).is_err());
        }
        assert_eq!(
            manifest_names("codex.toml\nclaude-code.toml\n").unwrap(),
            vec!["codex.toml", "claude-code.toml"]
        );
    }

    #[test]
    fn codex_hooks_use_the_owned_lifecycle_surface() {
        let hooks = codex_hooks();
        for event in ["UserPromptSubmit", "Stop", "PermissionRequest"] {
            let command = hooks[event][0]["hooks"][0]["command"].as_str().unwrap();
            assert!(command.starts_with(REPORT_PREFIX));
            assert!(command.contains("${KODADE_BIN:-kodade-cli}"));
            assert!(command.contains("${KODADE_SOCKET:-}"));
            assert!(!command.contains(" -s "));
            assert!(command.contains(">/dev/null 2>&1 || true"));
        }
    }

    #[test]
    fn generated_extensions_use_the_inherited_socket_and_documented_status_shape() {
        for extension in [pi_extension(), opencode_plugin()] {
            assert!(!extension.contains("\"-s\""));
            assert!(extension.contains("child.on(\"error\", () => {})"));
        }
        let opencode = opencode_plugin();
        assert!(opencode.contains("event.properties?.status?.type"));
        assert!(opencode.contains("status === \"busy\""));
        assert!(opencode.contains("status === \"idle\""));
    }

    #[test]
    fn integrations_report_native_ids_as_argv_data() {
        let claude_hooks = claude_hooks();
        let codex_hooks = codex_hooks();
        let claude = claude_hooks["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        let codex = codex_hooks["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(claude.contains("--hook-json"));
        assert!(claude.contains("kodade:claude-code"));
        assert!(codex.contains("--hook-json"));
        assert!(codex.contains("--native-agent codex"));
        assert!(pi_extension().contains("getSessionFile"));
        assert!(opencode_plugin().contains("sessionID || event.properties?.sessionId"));
    }

    #[test]
    fn merges_claude_settings_without_duplicate_hooks() {
        let temp = std::env::temp_dir().join(format!("kodade-cli-hooks-{}", std::process::id()));
        let path = temp.join("settings.json");
        fs::create_dir_all(&temp).unwrap();
        // Seed a retired Stop->idle hook plus an unrelated user hook.
        fs::write(
            &path,
            r#"{"theme":"dark","hooks":{"Stop":[{"hooks":[{"type":"command","command":"kodade-cli agent report $KODADE_PANE idle -s \"$KODADE_SESSION\""}]},{"hooks":[{"type":"command","command":"echo keep"}]}]}}"#,
        )
        .unwrap();
        merge_hook_settings(&path, &claude_hooks()).unwrap();
        merge_hook_settings(&path, &claude_hooks()).unwrap();
        let settings: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(settings["theme"], "dark");
        let stop = settings["hooks"]["Stop"].as_array().unwrap();
        // The retired idle hook is replaced (not duplicated); the user hook stays.
        assert_eq!(stop.len(), 2);
        assert!(stop
            .iter()
            .any(|entry| entry["hooks"][0]["command"] == "echo keep"));
        // The Ködade Stop hook now reports `done`, not the retired `idle`.
        assert!(stop.iter().any(|entry| {
            entry["hooks"][0]["command"]
                .as_str()
                .is_some_and(|c| c.contains(" done "))
        }));
        assert!(!stop.iter().any(|entry| {
            entry["hooks"][0]["command"]
                .as_str()
                .is_some_and(|c| c.contains(" idle "))
        }));
        assert_eq!(
            settings["hooks"]["Notification"].as_array().unwrap().len(),
            1
        );
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn upgrading_integration_preserves_sibling_hooks_and_matcher() {
        let temp =
            std::env::temp_dir().join(format!("kodade-sibling-hooks-{}", std::process::id()));
        fs::create_dir_all(&temp).unwrap();
        let path = temp.join("settings.json");
        let original = json!({"hooks": {"Stop": [{"matcher": "custom", "hooks": [
            {"type": "command", "command": format!("{REPORT_PREFIX}idle")},
            {"type": "command", "command": "echo user-hook", "timeout": 42}
        ]}]}});
        fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
        merge_hook_settings(&path, &claude_hooks()).unwrap();
        merge_hook_settings(&path, &claude_hooks()).unwrap();
        let updated: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let entries = updated["hooks"]["Stop"].as_array().unwrap();
        assert!(entries.iter().any(|entry| entry == &json!({
            "matcher": "custom", "hooks": [{"type":"command", "command":"echo user-hook", "timeout":42}]
        })), "unrelated hook and its entry metadata must survive: {updated}");
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn uninstall_is_idempotent_and_preserves_user_hook_siblings() {
        let temp = std::env::temp_dir().join(format!("kodade-uninstall-{}", std::process::id()));
        fs::create_dir_all(&temp).unwrap();
        let path = temp.join("hooks.json");
        merge_hook_settings(&path, &codex_hooks()).unwrap();
        let mut settings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        settings["hooks"]["Stop"][0]["hooks"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"command","command":"echo user"}));
        fs::write(&path, serde_json::to_vec(&settings).unwrap()).unwrap();
        remove_hook_settings(&path).unwrap();
        remove_hook_settings(&path).unwrap();
        let settings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            settings["hooks"]["Stop"][0]["hooks"],
            json!([{"type":"command","command":"echo user"}])
        );
        assert!(settings["hooks"].get("UserPromptSubmit").is_none());
        fs::remove_dir_all(temp).unwrap();
    }
}
