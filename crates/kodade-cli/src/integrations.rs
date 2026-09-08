//! Agent integration configuration. User-owned hooks and settings survive updates.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::{fs, path::Path};

/// Integrations that install a lifecycle hook / notify entry for a known agent.
pub const INTEGRATIONS: &[&str] = &["claude-code", "codex", "gemini-cli"];

/// Prefix shared by every Ködade report hook/notify command, used to detect and
/// replace a previously installed entry (e.g. migrating an old Stop->idle hook).
const REPORT_PREFIX: &str = "kodade-cli agent report $KODADE_PANE ";

/// Command each hook/notify entry runs; `state` is the state it reports.
fn report_command(state: &str) -> String {
    format!("{REPORT_PREFIX}{state} -s \"$KODADE_SESSION\"")
}

/// List every known integration and whether its config directory is present.
pub fn integrate_list() -> Result<()> {
    let home = dirs::home_dir();
    for agent in INTEGRATIONS {
        let (target, mechanism) = match *agent {
            "claude-code" => (".claude/settings.json", "hooks"),
            "codex" => (".codex/config.toml", "notify"),
            "gemini-cli" => (".gemini/settings.json", "hooks"),
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

/// Codex runs a single `notify` program with a JSON payload appended as the last
/// argument. `sh -c '<script>'` receives that payload as `$0`, which the report
/// script ignores.
fn codex_notify() -> toml_edit::Array {
    let mut array = toml_edit::Array::new();
    array.push("sh");
    array.push("-c");
    array.push(report_command("done"));
    array
}

pub fn integrate_codex(write: bool, force: bool) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    let path = home.join(".codex/config.toml");
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let merged = match merge_codex_notify(&source, force)? {
        Some(merged) => merged,
        None => {
            println!(
                "Codex already has a `notify` entry in {}.\n\
                 Ködade will not overwrite it. Re-run with --force to replace it, or add manually:\n{}",
                path.display(),
                notify_preview()
            );
            return Ok(());
        }
    };
    if !write {
        println!("{}", notify_preview());
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    crate::atomic_file::write(&path, merged.as_bytes())?;
    println!("installed Codex notify hook in {}", path.display());
    Ok(())
}

/// Insert the Ködade `notify` entry into a Codex config, preserving comments and
/// other keys. Returns `None` when a `notify` already exists and `force` is off.
fn merge_codex_notify(source: &str, force: bool) -> Result<Option<String>> {
    let mut doc = source
        .parse::<toml_edit::DocumentMut>()
        .context("parse Codex config.toml")?;
    let ours = doc
        .get("notify")
        .and_then(toml_edit::Item::as_array)
        .and_then(|array| array.get(2))
        .and_then(toml_edit::Value::as_str)
        .is_some_and(|command| command.starts_with(REPORT_PREFIX));
    if doc.contains_key("notify") && !force && !ours {
        return Ok(None);
    }
    doc["notify"] = toml_edit::value(codex_notify());
    Ok(Some(doc.to_string()))
}

fn notify_preview() -> String {
    let mut preview = toml_edit::DocumentMut::new();
    preview["notify"] = toml_edit::value(codex_notify());
    preview.to_string().trim_end().to_string()
}

/// Gemini has its own lifecycle event names; see geminicli.com/docs/hooks/.
/// Its notification event covers tool permission requests.
fn gemini_hooks() -> Value {
    json!({
        "AfterAgent": [{ "matcher": "*", "hooks": [{ "type": "command", "command": report_command("done") }] }],
        "BeforeAgent": [{ "matcher": "*", "hooks": [{ "type": "command", "command": report_command("working") }] }],
        "Notification": [{ "matcher": "*", "hooks": [{ "type": "command", "command": report_command("blocked") }] }]
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
        "Stop": [{ "hooks": [{ "type": "command", "command": report_command("done") }] }],
        "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": report_command("working") }] }],
        "Notification": [{ "hooks": [{ "type": "command", "command": report_command("blocked") }] }]
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
            nested.retain(|hook| {
                !hook["command"]
                    .as_str()
                    .is_some_and(|command| command.starts_with(REPORT_PREFIX))
            });
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
    fn codex_notify_merge_preserves_and_guards() {
        // Fresh config: notify is added and the report command is present.
        let merged = merge_codex_notify("model = \"gpt-5\"\n", false)
            .unwrap()
            .expect("notify inserted");
        assert!(merged.contains("model = \"gpt-5\""));
        assert!(merged.contains("kodade-cli agent report"));
        // Existing notify without --force is left untouched.
        assert!(merge_codex_notify("notify = [\"x\"]\n", false)
            .unwrap()
            .is_none());
        // --force replaces it.
        let forced = merge_codex_notify("notify = [\"x\"]\n", true)
            .unwrap()
            .expect("notify replaced");
        assert!(forced.contains("kodade-cli agent report"));
        assert!(!forced.contains("\"x\""));
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
}
