//! Agent integration configuration. User-owned hooks and settings survive updates.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::{fs, path::Path};

/// Integrations that install a lifecycle hook / notify entry for a known agent.
pub const INTEGRATIONS: &[&str] = &[
    "claude-code",
    "codex",
    "gemini-cli",
    "copilot",
    "cursor",
    "droid",
    "kimi",
    "qwen",
    "omp",
    "kilo",
    "hermes",
    "antigravity",
    "devin",
    "mastra",
    "grok",
    "opencode",
    "pi",
];

/// Marker shared by every Ködade-managed hook command. It is deliberately
/// distinct from a user's arbitrary `kodade-cli` invocation.
const REPORT_PREFIX: &str = "KODADE_INTEGRATION=kodade-cli;";
const LEGACY_REPORT_PREFIX: &str = "kodade-cli agent report $KODADE_PANE ";

/// Command each hook/notify entry runs; `state` is the state it reports.
fn report_command(state: &str, source: &str, agent: &str) -> String {
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
            "copilot" => (".copilot/hooks/kodade-cli.json", "hooks"),
            "cursor" => (".cursor/hooks.json", "hooks"),
            "droid" => (".factory/hooks.json", "hooks"),
            "kimi" => (".kimi-code/config.toml", "hooks"),
            "qwen" => (".qwen/settings.json", "hooks"),
            "omp" => (
                ".omp/agent/extensions/kodade-cli-agent-state.ts",
                "extension",
            ),
            "kilo" => (".config/kilo/plugin/kodade-cli-agent-state.ts", "plugin"),
            "hermes" => (
                ".hermes/plugins/kodade_cli_agent_state/__init__.py",
                "plugin",
            ),
            "antigravity" => (".gemini/config/hooks.json", "hooks"),
            "devin" => (".config/devin/config.json", "hooks"),
            "mastra" => (".mastracode/hooks.json", "hooks"),
            "grok" => (".grok/hooks/kodade-cli.json", "hooks"),
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
        let path = match *agent {
            "copilot" => Some(
                copilot_dir(home.as_deref(), std::env::var_os("COPILOT_HOME"))?
                    .join("hooks/kodade-cli.json"),
            ),
            "antigravity" => home
                .as_deref()
                .map(|home| antigravity_config_dir(home).join("hooks.json")),
            "devin" => home
                .as_deref()
                .map(devin_config_dir)
                .map(|dir| dir.join("config.json")),
            "grok" => home
                .as_deref()
                .map(grok_config_dir)
                .map(|dir| dir.join("hooks/kodade-cli.json")),
            _ => home.as_ref().map(|home| home.join(target)),
        };
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

/// Devin's documented hook configuration is Claude-compatible. Its published
/// payload does not provide a stable native session id, so these hooks report
/// lifecycle state only.
fn devin_hooks() -> Value {
    json!({
        "SessionStart": [{ "hooks": [{ "type": "command", "command": report_command("idle", "kodade:devin", "devin") }] }],
        "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": report_command("working", "kodade:devin", "devin") }] }],
        "PreToolUse": [{ "hooks": [{ "type": "command", "command": report_command("working", "kodade:devin", "devin") }] }],
        "PostToolUse": [{ "hooks": [{ "type": "command", "command": report_command("working", "kodade:devin", "devin") }] }],
        "PermissionRequest": [{ "hooks": [{ "type": "command", "command": report_command("blocked", "kodade:devin", "devin") }] }],
        "Stop": [{ "hooks": [{ "type": "command", "command": report_command("done", "kodade:devin", "devin") }] }],
        "SessionEnd": [{ "hooks": [{ "type": "command", "command": report_command("done", "kodade:devin", "devin") }] }]
    })
}

fn devin_config_dir(home: &Path) -> std::path::PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|path| !path.is_empty())
        .map(Into::into)
        .unwrap_or_else(|| home.join(".config"))
        .join("devin")
}

pub fn integrate_devin(write: bool) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    let path = devin_config_dir(&home).join("config.json");
    if !write {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "hooks": devin_hooks() }))?
        );
        return Ok(());
    }
    if !path.parent().is_some_and(Path::exists) {
        bail!(
            "Devin config directory {} does not exist",
            path.parent().unwrap().display()
        );
    }
    merge_hook_settings(&path, &devin_hooks())?;
    println!("installed Devin hooks in {}", path.display());
    Ok(())
}

pub fn unintegrate_devin() -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    remove_hook_settings(&devin_config_dir(&home).join("config.json"))
}

/// Mastra Code stores flat command entries in its global hooks document.
fn mastra_hooks() -> Value {
    let hook = |state| json!({ "type": "command", "command": report_command(state, "kodade:mastra", "mastra"), "timeout": 10_000 });
    json!({
        "SessionStart": [hook("idle")], "UserPromptSubmit": [hook("working")],
        "AgentStart": [hook("working")], "PreToolUse": [hook("working")],
        "PermissionRequest": [hook("blocked")], "PermissionResult": [hook("working")],
        "SubagentStart": [hook("working")], "SubagentEnd": [hook("working")],
        "Interrupt": [hook("idle")], "AgentEnd": [hook("done")], "Stop": [hook("done")]
    })
}

pub fn integrate_mastra(write: bool) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    let path = home.join(".mastracode/hooks.json");
    if !write {
        println!("{}", serde_json::to_string_pretty(&mastra_hooks())?);
        return Ok(());
    }
    merge_root_hook_settings(&path, &mastra_hooks())?;
    println!("installed Mastra Code hooks in {}", path.display());
    Ok(())
}

pub fn unintegrate_mastra() -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    remove_root_hook_settings(&home.join(".mastracode/hooks.json"))
}

/// Grok merges each JSON file in its hooks directory. Keep Ködade's entry in
/// a dedicated file so installation never rewrites a user's hook document.
fn grok_hooks() -> Value {
    json!({ "hooks": { "SessionStart": [{ "hooks": [{ "type": "command", "command": report_command("working", "kodade:grok", "grok"), "timeout": 10 }] }] } })
}

fn grok_config_dir(home: &Path) -> std::path::PathBuf {
    std::env::var_os("GROK_HOME")
        .filter(|path| !path.is_empty())
        .map(Into::into)
        .unwrap_or_else(|| home.join(".grok"))
}

pub fn integrate_grok(write: bool) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    let path = grok_config_dir(&home).join("hooks/kodade-cli.json");
    if !write {
        println!("{}", serde_json::to_string_pretty(&grok_hooks())?);
        return Ok(());
    }
    if !path
        .parent()
        .and_then(Path::parent)
        .is_some_and(Path::exists)
    {
        bail!(
            "Grok config directory {} does not exist",
            path.parent().unwrap().parent().unwrap().display()
        );
    }
    write_owned_file(
        &path,
        &format!("{}\n", serde_json::to_string_pretty(&grok_hooks())?),
    )?;
    println!("installed Grok hooks in {}", path.display());
    Ok(())
}

pub fn unintegrate_grok() -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    remove_owned_file(&grok_config_dir(&home).join("hooks/kodade-cli.json"))
}

/// Antigravity command hooks consume JSON on stdin and expect a JSON object on
/// stdout. Its documented lifecycle does not expose a resume command, so this
/// reports state only and deliberately does not persist `conversationId`.
fn antigravity_report_command(state: &str) -> String {
    format!(
        "{REPORT_PREFIX} if [ -n \"${{KODADE_PANE:-}}\" ] && [ -n \"${{KODADE_SOCKET:-}}\" ]; then \"${{KODADE_BIN:-kodade-cli}}\" agent report \"$KODADE_PANE\" {state} --source kodade:antigravity >/dev/null 2>&1 || true; fi; printf '{{}}\\n'"
    )
}

/// Antigravity discovers named hook blocks from `~/.gemini/config/hooks.json`.
/// Ködade owns only its block, so sibling hook definitions remain intact.
fn antigravity_hooks() -> Value {
    json!({
        "PreInvocation": [{
            "type": "command",
            "command": antigravity_report_command("working"),
            "timeout": 5,
        }],
        "Stop": [{
            "type": "command",
            "command": antigravity_report_command("done"),
            "timeout": 5,
        }],
    })
}

fn antigravity_config_dir(home: &Path) -> std::path::PathBuf {
    std::env::var_os("ANTIGRAVITY_CLI_CONFIG_DIR")
        .filter(|path| !path.is_empty())
        .map(Into::into)
        .unwrap_or_else(|| home.join(".gemini/config"))
}

pub fn integrate_antigravity(write: bool) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    let path = antigravity_config_dir(&home).join("hooks.json");
    if !write {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "kodade-cli": antigravity_hooks() }))?
        );
        return Ok(());
    }
    merge_named_hook_block(&path, "kodade-cli", antigravity_hooks())?;
    println!("installed Antigravity hooks in {}", path.display());
    Ok(())
}

pub fn unintegrate_antigravity() -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    remove_named_hook_block(
        &antigravity_config_dir(&home).join("hooks.json"),
        "kodade-cli",
    )
}

/// Copilot CLI uses independently-loaded, versioned hook documents.
fn copilot_hooks() -> Value {
    let command = |state| {
        json!({
            "type": "command",
            "command": report_command(state, "kodade:copilot", "copilot"),
            "timeoutSec": 5
        })
    };
    json!({
        "version": 1,
        "hooks": {
            "userPromptSubmitted": [command("working")],
            "agentStop": [command("done")],
            "permissionRequest": [command("blocked")],
            "errorOccurred": [command("blocked")]
        }
    })
}

pub fn integrate_copilot(write: bool) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    let path =
        copilot_dir(Some(&home), std::env::var_os("COPILOT_HOME"))?.join("hooks/kodade-cli.json");
    if !write {
        println!("{}", serde_json::to_string_pretty(&copilot_hooks())?);
        return Ok(());
    }
    write_owned_file(
        &path,
        &format!("{}\n", serde_json::to_string_pretty(&copilot_hooks())?),
    )?;
    println!("installed Copilot CLI hooks in {}", path.display());
    Ok(())
}

pub fn unintegrate_copilot() -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    remove_owned_file(
        &copilot_dir(Some(&home), std::env::var_os("COPILOT_HOME"))?.join("hooks/kodade-cli.json"),
    )
}

/// Copilot CLI replaces its complete user configuration root when
/// `COPILOT_HOME` is set, including the user hook directory.
fn copilot_dir(
    home: Option<&Path>,
    configured: Option<std::ffi::OsString>,
) -> Result<std::path::PathBuf> {
    if let Some(dir) = configured.filter(|dir| !dir.is_empty()) {
        return Ok(dir.into());
    }
    Ok(home
        .ok_or_else(|| anyhow!("home directory unavailable"))?
        .join(".copilot"))
}

fn cursor_hooks() -> Value {
    json!({
        "sessionStart": [{ "command": report_command("working", "kodade:cursor", "cursor") }],
        "beforeSubmitPrompt": [{ "command": report_command("working", "kodade:cursor", "cursor") }],
        "stop": [{ "command": report_command("done", "kodade:cursor", "cursor") }],
        "sessionEnd": [{ "command": report_command("done", "kodade:cursor", "cursor") }]
    })
}

pub fn integrate_cursor(write: bool) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    let path = home.join(".cursor/hooks.json");
    if !write {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"version": 1, "hooks": cursor_hooks()}))?
        );
        return Ok(());
    }
    merge_versioned_hook_settings(&path, &cursor_hooks())?;
    println!("installed Cursor hooks in {}", path.display());
    Ok(())
}

pub fn unintegrate_cursor() -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    remove_hook_settings(&home.join(".cursor/hooks.json"))
}

fn droid_hooks() -> Value {
    json!({
        "SessionStart": [{ "hooks": [{ "type": "command", "command": report_command("working", "kodade:droid", "droid"), "timeout": 5 }] }],
        "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": report_command("working", "kodade:droid", "droid"), "timeout": 5 }] }],
        "Stop": [{ "hooks": [{ "type": "command", "command": report_command("done", "kodade:droid", "droid"), "timeout": 5 }] }],
        "Notification": [{ "matcher": "permission_prompt|elicitation_dialog", "hooks": [{ "type": "command", "command": report_command("blocked", "kodade:droid", "droid"), "timeout": 5 }] }]
    })
}

pub fn integrate_droid(write: bool) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    let path = home.join(".factory/hooks.json");
    if !write {
        println!("{}", serde_json::to_string_pretty(&droid_hooks())?);
        return Ok(());
    }
    merge_root_hook_settings(&path, &droid_hooks())?;
    println!("installed Droid hooks in {}", path.display());
    Ok(())
}

pub fn unintegrate_droid() -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    remove_root_hook_settings(&home.join(".factory/hooks.json"))
}

fn qwen_hooks() -> Value {
    json!({
        "SessionStart": [{ "hooks": [{ "type": "command", "command": report_command("working", "kodade:qwen", "qwen") }] }],
        "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": report_command("working", "kodade:qwen", "qwen") }] }],
        "Stop": [{ "hooks": [{ "type": "command", "command": report_command("done", "kodade:qwen", "qwen") }] }],
        "PermissionRequest": [{ "hooks": [{ "type": "command", "command": report_command("blocked", "kodade:qwen", "qwen") }] }]
    })
}

pub fn integrate_qwen(write: bool) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    let path = home.join(".qwen/settings.json");
    if !write {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"hooks": qwen_hooks()}))?
        );
        return Ok(());
    }
    merge_hook_settings(&path, &qwen_hooks())?;
    println!("installed Qwen Code hooks in {}", path.display());
    Ok(())
}

pub fn unintegrate_qwen() -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    remove_hook_settings(&home.join(".qwen/settings.json"))
}

const KIMI_HOOKS_BEGIN: &str = "# KODADE CLI managed hooks: begin";
const KIMI_HOOKS_END: &str = "# KODADE CLI managed hooks: end";

fn kimi_hooks_toml() -> String {
    let command =
        |state| toml::Value::String(report_command(state, "kodade:kimi", "kimi")).to_string();
    [
        KIMI_HOOKS_BEGIN.to_owned(),
        format!("[[hooks]]\nevent = \"SessionStart\"\ncommand = {}\ntimeout = 5", command("working")),
        format!("[[hooks]]\nevent = \"UserPromptSubmit\"\ncommand = {}\ntimeout = 5", command("working")),
        format!("[[hooks]]\nevent = \"Stop\"\ncommand = {}\ntimeout = 5", command("done")),
        format!("[[hooks]]\nevent = \"Notification\"\nmatcher = \"permission_prompt\"\ncommand = {}\ntimeout = 5", command("blocked")),
        KIMI_HOOKS_END.to_owned(),
    ].join("\n\n")
}

fn remove_kimi_hook_block(source: &str) -> String {
    let Some(begin) = source.find(KIMI_HOOKS_BEGIN) else {
        return source.to_owned();
    };
    let Some(end_offset) = source[begin..].find(KIMI_HOOKS_END) else {
        return source.to_owned();
    };
    let end = begin + end_offset + KIMI_HOOKS_END.len();
    format!("{}{}", &source[..begin], &source[end..])
        .trim()
        .to_owned()
}

pub fn integrate_kimi(write: bool) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    let path = home.join(".kimi-code/config.toml");
    let hooks = kimi_hooks_toml();
    if !write {
        println!("{hooks}");
        return Ok(());
    }
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let preserved = remove_kimi_hook_block(&source);
    let updated = if preserved.trim().is_empty() {
        hooks
    } else {
        format!("{preserved}\n\n{hooks}")
    };
    toml::from_str::<toml::Value>(&updated).context("validate Kimi config.toml")?;
    write_owned_file(&path, &format!("{updated}\n"))?;
    println!("installed Kimi Code hooks in {}", path.display());
    Ok(())
}

pub fn unintegrate_kimi() -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    let path = home.join(".kimi-code/config.toml");
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !source.contains(KIMI_HOOKS_BEGIN) {
        return Ok(());
    }
    crate::atomic_file::write(
        &path,
        format!("{}\n", remove_kimi_hook_block(&source)).as_bytes(),
    )
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
        "Stop": [{ "hooks": [{ "type": "command", "command": report_command("done", "kodade:codex", "codex") }] }],
        "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": report_command("working", "kodade:codex", "codex") }] }],
        "PermissionRequest": [{ "hooks": [{ "type": "command", "command": report_command("blocked", "kodade:codex", "codex") }] }]
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
        "AfterAgent": [{ "matcher": "*", "hooks": [{ "type": "command", "command": report_command("done", "kodade:gemini-cli", "gemini") }] }],
        "BeforeAgent": [{ "matcher": "*", "hooks": [{ "type": "command", "command": report_command("working", "kodade:gemini-cli", "gemini") }] }],
        "Notification": [{ "matcher": "*", "hooks": [{ "type": "command", "command": report_command("blocked", "kodade:gemini-cli", "gemini") }] }]
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

/// OMP keeps its Pi-compatible agent resources below its own config directory.
/// `omp config path` reports `~/.omp/agent` by default; PI_CONFIG_DIR selects
/// a different relative config directory.
fn omp_agent_dir(home: &Path) -> std::path::PathBuf {
    let config = std::env::var_os("PI_CONFIG_DIR").unwrap_or_else(|| ".omp".into());
    let config = std::path::PathBuf::from(config);
    let base = if config.is_absolute() {
        config
    } else {
        home.join(config)
    };
    base.join("agent")
}

pub fn integrate_omp(write: bool) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    let root = omp_agent_dir(&home);
    let path = root.join("extensions/kodade-cli-agent-state.ts");
    if !write {
        println!("{}", omp_extension());
        return Ok(());
    }
    if !root.exists() {
        bail!("OMP agent directory {} does not exist", root.display());
    }
    write_owned_file(&path, omp_extension())?;
    println!("installed OMP extension in {}", path.display());
    Ok(())
}

pub fn unintegrate_omp() -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    remove_owned_file(&omp_agent_dir(&home).join("extensions/kodade-cli-agent-state.ts"))
}

/// Kilo auto-loads TypeScript plugins from its global `plugin/` directory.
/// Kilo documents session lifecycle events, but not a CLI flag that resumes a
/// specific session ID, so this adapter deliberately reports lifecycle only.
pub fn integrate_kilo(write: bool) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    let path = home.join(".config/kilo/plugin/kodade-cli-agent-state.ts");
    if !write {
        println!("{}", kilo_plugin());
        return Ok(());
    }
    if !path
        .parent()
        .and_then(Path::parent)
        .is_some_and(Path::exists)
    {
        bail!(
            "Kilo config directory {} does not exist",
            path.parent().unwrap().parent().unwrap().display()
        );
    }
    write_owned_file(&path, kilo_plugin())?;
    println!("installed Kilo plugin in {}", path.display());
    Ok(())
}

pub fn unintegrate_kilo() -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    remove_owned_file(&home.join(".config/kilo/plugin/kodade-cli-agent-state.ts"))
}

/// Hermes discovers Python plugins in `~/.hermes/plugins/`. The callbacks use
/// its documented session ID and lifecycle arguments without parsing payloads.
pub fn integrate_hermes(write: bool) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    let path = home.join(".hermes/plugins/kodade_cli_agent_state/__init__.py");
    if !write {
        println!("{}", hermes_plugin());
        return Ok(());
    }
    if !path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .is_some_and(Path::exists)
    {
        bail!(
            "Hermes config directory {} does not exist",
            path.parent().unwrap().parent().unwrap().display()
        );
    }
    write_owned_file(&path, hermes_plugin())?;
    println!("installed Hermes plugin in {}", path.display());
    Ok(())
}

pub fn unintegrate_hermes() -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    remove_owned_file(&home.join(".hermes/plugins/kodade_cli_agent_state/__init__.py"))
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
  child.on("error", () => {}); child.unref();
}
export default function (pi) {
  if (!enabled()) return;
  let tui = false, session, path;
  const update = (ctx) => { try { session = ctx?.sessionManager?.getSessionId?.(); } catch {} try { path = ctx?.sessionManager?.getSessionFile?.(); } catch {} };
  pi.on("session_start", (_event, ctx) => { tui = ctx?.mode === "tui"; update(ctx); if (tui) report("idle", session, path); });
  pi.on("agent_start", (_event, ctx) => { update(ctx); if (tui) report("working", session, path); });
  pi.on("agent_settled", (_event, ctx) => { update(ctx); if (tui) report("done", session, path); });
}
"#
}

fn omp_extension() -> &'static str {
    r#"// Installed by Ködade CLI. OMP uses the Pi extension API.
import { spawn } from "node:child_process";
const enabled = () => process.env.KODADE_PANE && process.env.KODADE_SOCKET;
function report(state, session, path) {
  if (!enabled()) return;
  const args = ["agent", "report", process.env.KODADE_PANE, state, "--source", "kodade:omp", "--native-agent", "omp"];
  if (typeof path === "string" && path.startsWith("/")) args.push("--native-session-path", path);
  else if (typeof session === "string" && session.length) args.push("--native-session-id", session);
  const child = spawn(process.env.KODADE_BIN || "kodade-cli", args, { detached: true, stdio: "ignore" });
  child.on("error", () => {}); child.unref();
}
export default function (pi) {
  if (!enabled()) return;
  let tui = false, session, path;
  const update = (ctx) => { try { session = ctx?.sessionManager?.getSessionId?.(); } catch {} try { path = ctx?.sessionManager?.getSessionFile?.(); } catch {} };
  pi.on("session_start", (_event, ctx) => { tui = ctx?.mode === "tui"; update(ctx); if (tui) report("idle", session, path); });
  pi.on("agent_start", (_event, ctx) => { update(ctx); if (tui) report("working", session, path); });
  pi.on("agent_settled", (_event, ctx) => { update(ctx); if (tui) report("done", session, path); });
}
"#
}

/// Kilo's plugin API supplies session lifecycle events. Its public CLI
/// documentation does not expose an ID-based resume flag, so omit native ID.
fn kilo_plugin() -> &'static str {
    r#"// Installed by Ködade CLI. Kilo plugin API.
import { spawn } from "node:child_process";
const enabled = () => process.env.KODADE_PANE && process.env.KODADE_SOCKET;
function report(state) {
  if (!enabled()) return;
  const args = ["agent", "report", process.env.KODADE_PANE, state, "--source", "kodade:kilo", "--native-agent", "kilo"];
  const child = spawn(process.env.KODADE_BIN || "kodade-cli", args, { detached: true, stdio: "ignore" });
  child.on("error", () => {}); child.unref();
}
const server = async () => ({
  "chat.message": async () => report("working"),
  event: async ({ event }) => {
    switch (event?.type) {
      case "session.created": case "session.updated": report("working"); break;
      case "session.status":
        if (event?.properties?.status?.type === "idle") report("done");
        else if (event?.properties?.status?.type === "busy" || event?.properties?.status?.type === "retry") report("working");
        break;
      case "session.idle": report("done"); break;
      case "session.error": case "permission.asked": report("blocked"); break;
    }
  },
});
export default { id: "kodade-cli-agent-state", server };
"#
}

/// Hermes' documented plugin callbacks include `session_id`, session start/end,
/// LLM start/end, and approval lifecycle. Spawn preserves the interactive TUI.
fn hermes_plugin() -> &'static str {
    r#"# Installed by Ködade CLI. Hermes plugin API.
import os
import subprocess


def _report(state, session_id=None):
    pane = os.environ.get("KODADE_PANE")
    if not pane or not os.environ.get("KODADE_SOCKET"):
        return
    args = [os.environ.get("KODADE_BIN", "kodade-cli"), "agent", "report", pane, state,
            "--source", "kodade:hermes", "--native-agent", "hermes"]
    if isinstance(session_id, str) and session_id:
        args.extend(["--native-session-id", session_id])
    try:
        subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                         start_new_session=True)
    except OSError:
        pass


def _start(session_id=None, **_kwargs): _report("working", session_id)
def _done(session_id=None, **_kwargs): _report("done", session_id)
def _blocked(session_id=None, **_kwargs): _report("blocked", session_id)

def register(ctx):
    ctx.register_hook("on_session_start", _start)
    ctx.register_hook("pre_llm_call", _start)
    ctx.register_hook("post_llm_call", _done)
    ctx.register_hook("on_session_end", _done)
    ctx.register_hook("pre_approval_request", _blocked)
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
        "Stop": [{ "hooks": [{ "type": "command", "command": report_command("done", "kodade:claude-code", "claude") }] }],
        "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": report_command("working", "kodade:claude-code", "claude") }] }],
        "Notification": [{ "hooks": [{ "type": "command", "command": report_command("blocked", "kodade:claude-code", "claude") }] }]
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
    merge_hooks_value(&mut settings, new_hooks)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    crate::atomic_file::write(
        path,
        format!("{}\n", serde_json::to_string_pretty(&settings)?).as_bytes(),
    )
}

fn merge_versioned_hook_settings(path: &Path, new_hooks: &Value) -> Result<()> {
    let mut settings: Value = match fs::read_to_string(path) {
        Ok(source) => serde_json::from_str(&source).context("parse hooks.json")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({ "version": 1 }),
        Err(error) => return Err(error.into()),
    };
    let object = settings
        .as_object_mut()
        .ok_or_else(|| anyhow!("hooks.json must be an object"))?;
    object.entry("version").or_insert_with(|| json!(1));
    merge_hooks_value(&mut settings, new_hooks)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    crate::atomic_file::write(
        path,
        format!("{}\n", serde_json::to_string_pretty(&settings)?).as_bytes(),
    )
}

fn merge_root_hook_settings(path: &Path, new_hooks: &Value) -> Result<()> {
    let original: Value = match fs::read_to_string(path) {
        Ok(source) => serde_json::from_str(&source).context("parse hooks.json")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error.into()),
    };
    let mut wrapper = json!({ "hooks": original });
    merge_hooks_value(&mut wrapper, new_hooks)?;
    let hooks = wrapper["hooks"].take();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    crate::atomic_file::write(
        path,
        format!("{}\n", serde_json::to_string_pretty(&hooks)?).as_bytes(),
    )
}

fn merge_named_hook_block(path: &Path, name: &str, block: Value) -> Result<()> {
    let mut hooks: serde_json::Map<String, Value> = match fs::read_to_string(path) {
        Ok(source) => serde_json::from_str(&source).context("parse hooks.json")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Default::default(),
        Err(error) => return Err(error.into()),
    };
    hooks.insert(name.to_owned(), block);
    write_owned_file(
        path,
        &format!("{}\n", serde_json::to_string_pretty(&hooks)?),
    )
}

fn remove_named_hook_block(path: &Path, name: &str) -> Result<()> {
    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut hooks: serde_json::Map<String, Value> =
        serde_json::from_str(&source).context("parse hooks.json")?;
    if hooks.remove(name).is_some() {
        crate::atomic_file::write(
            path,
            format!("{}\n", serde_json::to_string_pretty(&hooks)?).as_bytes(),
        )?;
    }
    Ok(())
}

fn merge_hooks_value(settings: &mut Value, new_hooks: &Value) -> Result<()> {
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
            if entry["command"].as_str().is_some_and(is_report_command) {
                return false;
            }
            let Some(nested) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
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
    remove_hooks_value(&mut settings)?;
    crate::atomic_file::write(
        path,
        format!("{}\n", serde_json::to_string_pretty(&settings)?).as_bytes(),
    )
}

fn remove_root_hook_settings(path: &Path) -> Result<()> {
    let mut hooks: Value = match fs::read_to_string(path) {
        Ok(source) => serde_json::from_str(&source).context("parse hooks.json")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut wrapper = json!({ "hooks": hooks.take() });
    remove_hooks_value(&mut wrapper)?;
    crate::atomic_file::write(
        path,
        format!("{}\n", serde_json::to_string_pretty(&wrapper["hooks"])?).as_bytes(),
    )
}

fn remove_hooks_value(settings: &mut Value) -> Result<()> {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(());
    };
    for destination in hooks.values_mut().filter_map(Value::as_array_mut) {
        destination.retain_mut(|entry| {
            if entry["command"].as_str().is_some_and(is_report_command) {
                return false;
            }
            let Some(nested) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            nested.retain(|hook| !hook["command"].as_str().is_some_and(is_report_command));
            !nested.is_empty()
        });
    }
    hooks.retain(|_, entries| !entries.as_array().is_some_and(Vec::is_empty));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::{
        io::Write,
        process::{Command, Stdio},
        time::{SystemTime, UNIX_EPOCH},
    };

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
    fn verified_external_hook_shapes_preserve_lifecycle_and_session_keys() {
        let copilot = copilot_hooks();
        assert_eq!(copilot["version"], 1);
        for event in [
            "userPromptSubmitted",
            "agentStop",
            "permissionRequest",
            "errorOccurred",
        ] {
            assert!(copilot["hooks"][event][0]["command"]
                .as_str()
                .is_some_and(|command| command.contains("--hook-json")));
        }
        assert!(cursor_hooks()["sessionStart"][0]["command"]
            .as_str()
            .is_some_and(|command| command.contains("kodade:cursor")));
        for hooks in [droid_hooks(), qwen_hooks()] {
            assert!(hooks["SessionStart"][0]["hooks"][0]["command"]
                .as_str()
                .is_some_and(|command| command.contains("--hook-json")));
            assert!(hooks["Stop"][0]["hooks"][0]["command"]
                .as_str()
                .is_some_and(|command| command.contains(" done ")));
        }
    }

    #[test]
    fn copilot_home_replaces_the_default_config_root() {
        let home = Path::new("/tmp/kodade-home");
        assert_eq!(
            copilot_dir(Some(home), Some("/tmp/kodade-copilot-home".into())).unwrap(),
            Path::new("/tmp/kodade-copilot-home")
        );
        assert_eq!(
            copilot_dir(Some(home), None).unwrap(),
            home.join(".copilot")
        );
    }

    #[test]
    fn antigravity_uses_flat_lifecycle_handlers_and_preserves_sibling_blocks() {
        let hooks = antigravity_hooks();
        for event in ["PreInvocation", "Stop"] {
            assert_eq!(hooks[event][0]["type"], "command");
            assert!(hooks[event][0]["command"]
                .as_str()
                .is_some_and(|command| command.contains("--source kodade:antigravity")));
        }
        let path = std::env::temp_dir().join(format!("kodade-antigravity-{}", std::process::id()));
        fs::write(&path, r#"{"user":{"Stop":[{"command":"echo keep"}]}}"#).unwrap();
        merge_named_hook_block(&path, "kodade-cli", hooks).unwrap();
        remove_named_hook_block(&path, "kodade-cli").unwrap();
        let retained: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(retained["user"]["Stop"][0]["command"], "echo keep");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn antigravity_config_dir_uses_its_default_root() {
        assert_eq!(
            antigravity_config_dir(Path::new("/tmp/kodade-home")),
            Path::new("/tmp/kodade-home/.gemini/config")
        );
    }

    #[test]
    fn devin_mastra_and_grok_generated_fixtures_match_their_hook_contracts() {
        let devin = devin_hooks();
        for event in [
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PostToolUse",
            "PermissionRequest",
            "Stop",
            "SessionEnd",
        ] {
            let command = devin[event][0]["hooks"][0]["command"].as_str().unwrap();
            assert!(command.contains("kodade:devin"));
            assert!(!command.contains("--native-session-id"));
        }
        assert!(devin["PermissionRequest"][0]["hooks"][0]["command"]
            .as_str()
            .is_some_and(|command| command.contains(" blocked ")));

        let mastra = mastra_hooks();
        for event in ["SessionStart", "AgentStart", "PermissionRequest", "Stop"] {
            let entry = &mastra[event][0];
            assert_eq!(entry["type"], "command");
            assert_eq!(entry["timeout"], 10_000);
            assert!(entry["command"]
                .as_str()
                .is_some_and(|command| command.contains("kodade:mastra")));
        }

        let grok = grok_hooks();
        let command = grok["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(command.contains("kodade:grok"));
        assert!(command.contains("--hook-json"));
    }

    #[test]
    fn remaining_adapter_removal_preserves_user_hooks() {
        let temp =
            std::env::temp_dir().join(format!("kodade-remaining-hooks-{}", std::process::id()));
        fs::create_dir_all(&temp).unwrap();

        let devin = temp.join("devin.json");
        fs::write(
            &devin,
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo keep"}]}]}}"#,
        )
        .unwrap();
        merge_hook_settings(&devin, &devin_hooks()).unwrap();
        merge_hook_settings(&devin, &devin_hooks()).unwrap();
        remove_hook_settings(&devin).unwrap();
        let retained: Value = serde_json::from_slice(&fs::read(&devin).unwrap()).unwrap();
        assert_eq!(
            retained["hooks"]["Stop"][0]["hooks"][0]["command"],
            "echo keep"
        );

        let mastra = temp.join("mastra.json");
        fs::write(
            &mastra,
            r#"{"Stop":[{"type":"command","command":"echo keep"}]}"#,
        )
        .unwrap();
        merge_root_hook_settings(&mastra, &mastra_hooks()).unwrap();
        merge_root_hook_settings(&mastra, &mastra_hooks()).unwrap();
        remove_root_hook_settings(&mastra).unwrap();
        let retained: Value = serde_json::from_slice(&fs::read(&mastra).unwrap()).unwrap();
        assert_eq!(retained["Stop"][0]["command"], "echo keep");
        fs::remove_dir_all(temp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn generated_remaining_hook_commands_receive_the_complete_vendor_payload() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp = std::env::temp_dir().join(format!("kodade-hook-exec-{unique}"));
        fs::create_dir_all(&temp).unwrap();
        let args = temp.join("args");
        let payload_file = temp.join("payload");
        let recorder = temp.join("record-kodade-call");
        fs::write(
            &recorder,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\ncat > {}\n",
                args.display(),
                payload_file.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&recorder, fs::Permissions::from_mode(0o755)).unwrap();
        let payload =
            r#"{"sessionId":"root-id","session_id":"root-snake","nested":{"sessionId":"decoy"}}"#;

        for command in [
            devin_hooks()["Stop"][0]["hooks"][0]["command"]
                .as_str()
                .unwrap(),
            mastra_hooks()["Stop"][0]["command"].as_str().unwrap(),
            grok_hooks()["hooks"]["SessionStart"][0]["hooks"][0]["command"]
                .as_str()
                .unwrap(),
        ] {
            let mut child = Command::new("sh")
                .arg("-c")
                .arg(command)
                .env("KODADE_PANE", "19")
                .env("KODADE_SOCKET", "/tmp/kodade.sock")
                .env("KODADE_BIN", &recorder)
                .stdin(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(payload.as_bytes())
                .unwrap();
            assert!(child.wait().unwrap().success());
            assert_eq!(fs::read_to_string(&payload_file).unwrap(), payload);
            let argv = fs::read_to_string(&args).unwrap();
            assert!(argv.contains("agent\nreport\n19"));
            assert!(argv.contains("--hook-json"));
        }
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn versioned_and_root_hook_files_keep_user_entries() {
        let temp = std::env::temp_dir().join(format!("kodade-extra-hooks-{}", std::process::id()));
        fs::create_dir_all(&temp).unwrap();
        let cursor = temp.join("cursor.json");
        fs::write(
            &cursor,
            r#"{"version":1,"hooks":{"stop":[{"command":"echo keep"}]}}"#,
        )
        .unwrap();
        merge_versioned_hook_settings(&cursor, &cursor_hooks()).unwrap();
        merge_versioned_hook_settings(&cursor, &cursor_hooks()).unwrap();
        let cursor_json: Value = serde_json::from_slice(&fs::read(&cursor).unwrap()).unwrap();
        assert_eq!(cursor_json["hooks"]["stop"].as_array().unwrap().len(), 2);
        remove_hook_settings(&cursor).unwrap();
        let cursor_json: Value = serde_json::from_slice(&fs::read(&cursor).unwrap()).unwrap();
        assert_eq!(
            cursor_json["hooks"]["stop"],
            json!([{ "command": "echo keep" }])
        );

        let droid = temp.join("droid.json");
        fs::write(
            &droid,
            r#"{"Stop":[{"hooks":[{"type":"command","command":"echo keep"}]}]}"#,
        )
        .unwrap();
        merge_root_hook_settings(&droid, &droid_hooks()).unwrap();
        remove_root_hook_settings(&droid).unwrap();
        let droid_json: Value = serde_json::from_slice(&fs::read(&droid).unwrap()).unwrap();
        assert_eq!(
            droid_json["Stop"],
            json!([{ "hooks": [{ "type": "command", "command": "echo keep" }] }])
        );
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn kimi_managed_block_is_idempotent_and_leaves_user_toml() {
        let original = "default_model = \"kimi-code/k3\"\n";
        let installed = format!("{original}\n{}\n", kimi_hooks_toml());
        toml::from_str::<toml::Value>(&installed).unwrap();
        let replaced = format!(
            "{}\n{}\n",
            remove_kimi_hook_block(&installed),
            kimi_hooks_toml()
        );
        assert_eq!(replaced.matches(KIMI_HOOKS_BEGIN).count(), 1);
        assert!(replaced.contains(original.trim()));
        assert!(remove_kimi_hook_block(&replaced).contains(original.trim()));
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
