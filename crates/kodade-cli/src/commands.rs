use anyhow::{anyhow, bail, Context, Result};
use kodade_cli_proto::{
    decode, encode, server_message_name, AgentStateKind, ClientMessage, Event, LayoutSnapshot,
    PaneId, PaneSnapshot, QueryKind, ServerMessage, SessionFile, TabId, TabInfo, WorkspaceId,
    WorkspaceInfo,
};
use serde_json::{json, Value};
use std::{fs, path::Path, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

/// Poll interval for `agent wait` / `pane wait-output`.
const POLL: Duration = Duration::from_millis(250);

/// How long `session ls` waits for a socket to answer before calling it dead.
const PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// Ceiling on one request (connect plus the first reply line) so a half-open
/// daemon cannot hang a scripting verb forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

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
    fs::write(&path, merged)?;
    println!("installed Codex notify hook in {}", path.display());
    Ok(())
}

/// Insert the Ködade `notify` entry into a Codex config, preserving comments and
/// other keys. Returns `None` when a `notify` already exists and `force` is off.
fn merge_codex_notify(source: &str, force: bool) -> Result<Option<String>> {
    let mut doc = source
        .parse::<toml_edit::DocumentMut>()
        .context("parse Codex config.toml")?;
    if doc.contains_key("notify") && !force {
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

/// Gemini CLI uses Claude-compatible hook events (it even ships `gemini hooks
/// migrate`), so we install the same Stop/UserPromptSubmit/Notification mapping.
fn gemini_hooks() -> Value {
    json!({
        "Stop": [{ "matcher": "*", "hooks": [{ "type": "command", "command": report_command("done") }] }],
        "UserPromptSubmit": [{ "matcher": "*", "hooks": [{ "type": "command", "command": report_command("working") }] }],
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
    const BASE: &str = "https://raw.githubusercontent.com/ContractorKeith/kodade-cli/main/crates/kodade-cli-daemon/manifests/";
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    let dir = home.join(".config/kodade-cli/agent-detection");
    fs::create_dir_all(&dir).context("create agent-detection directory")?;
    let index = curl(&format!("{BASE}index.txt"))?;
    for name in index.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let contents = curl(&format!("{BASE}{name}"))?;
        let path = dir.join(name);
        fs::write(&path, contents)?;
        println!("wrote {}", path.display());
    }
    Ok(())
}

/// Fetch a URL with the system `curl` so no HTTP crate enters the dependency set.
fn curl(url: &str) -> Result<String> {
    let output = std::process::Command::new("curl")
        .args(["-fsSL", url])
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
    for (event, entries) in new_hooks.as_object().expect("hooks object") {
        let destination = hooks
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| anyhow!("Claude hook event must be an array"))?;
        // Drop any prior Ködade report hook for this event so an old command
        // (e.g. the retired Stop->idle) is upgraded rather than duplicated.
        destination.retain(|entry| {
            !entry["hooks"].as_array().is_some_and(|nested| {
                nested.iter().any(|hook| {
                    hook["command"]
                        .as_str()
                        .is_some_and(|command| command.starts_with(REPORT_PREFIX))
                })
            })
        });
        destination.extend(entries.as_array().expect("entries").iter().cloned());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        path,
        format!("{}\n", serde_json::to_string_pretty(&settings)?),
    )?;
    Ok(())
}

pub fn parse_state(value: &str) -> Result<AgentStateKind> {
    match value {
        "blocked" => Ok(AgentStateKind::Blocked),
        "working" => Ok(AgentStateKind::Working),
        "done" => Ok(AgentStateKind::Done),
        "idle" => Ok(AgentStateKind::Idle),
        "unknown" => Ok(AgentStateKind::Unknown),
        _ => bail!("unknown agent state '{value}'"),
    }
}

/// Send one message to the daemon at `socket` and return its reply. The socket
/// is resolved by the caller (`remote::resolve_socket`) so `--remote` transparently
/// redirects every scripting command through the forwarded local socket (#23).
pub async fn request(socket: &Path, message: ClientMessage) -> Result<ServerMessage> {
    let exchange = async {
        let stream = UnixStream::connect(socket)
            .await
            .with_context(|| format!("no Ködade CLI daemon at {}", socket.display()))?;
        let (reader, mut writer) = stream.into_split();
        writer.write_all(&encode(&message)?).await?;
        let mut lines = BufReader::new(reader).lines();
        lines
            .next_line()
            .await?
            .ok_or_else(|| anyhow!("daemon closed the connection"))
    };
    let line = tokio::time::timeout(REQUEST_TIMEOUT, exchange)
        .await
        .map_err(|_| {
            anyhow!(
                "timed out after {}s waiting for the daemon at {}",
                REQUEST_TIMEOUT.as_secs(),
                socket.display()
            )
        })??;
    let reply = decode::<ServerMessage>(line.as_bytes())?;
    if let ServerMessage::Error { message } = &reply {
        bail!("{message}");
    }
    Ok(reply)
}

pub fn layout(reply: ServerMessage) -> Result<LayoutSnapshot> {
    match reply {
        ServerMessage::Layout(layout) => Ok(layout),
        ServerMessage::Shutdown => bail!("daemon shut down"),
        ServerMessage::Error { message } => bail!("{message}"),
        other => bail!("daemon sent an unexpected {}", server_message_name(&other)),
    }
}

/// Extract the `Pane` reply of a `Query(QueryKind::Pane(_))`.
pub fn pane_snapshot(reply: ServerMessage) -> Result<PaneSnapshot> {
    match reply {
        ServerMessage::Pane(pane) => Ok(pane),
        ServerMessage::Error { message } => bail!("{message}"),
        other => bail!("daemon sent an unexpected {}", server_message_name(&other)),
    }
}

/// Extract the `Session` reply of a `Query(QueryKind::Session)` (`layout export`).
pub fn session_file(reply: ServerMessage) -> Result<SessionFile> {
    match reply {
        ServerMessage::Session(file) => Ok(file),
        ServerMessage::Error { message } => bail!("{message}"),
        other => bail!("daemon sent an unexpected {}", server_message_name(&other)),
    }
}

/// Panes of the active tab, ordered by id — the `pane ls` set.
pub fn format_panes(layout: &LayoutSnapshot) -> String {
    let mut panes: Vec<&PaneSnapshot> = layout.panes.iter().collect();
    panes.sort_by_key(|pane| pane.id.0);
    panes
        .iter()
        .map(|pane| {
            format!(
                "{}{}  {}  {}  {}",
                pane.id.0,
                if pane.focused { "*" } else { " " },
                pane.title,
                pane.agent.as_deref().unwrap_or("shell"),
                state_name(pane.state)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Tabs of the active workspace (`tab ls`).
pub fn format_tabs(tabs: &[TabInfo]) -> String {
    tabs.iter()
        .map(|tab| {
            format!(
                "{}{}  {}  {}",
                tab.id.0,
                if tab.active { "*" } else { " " },
                tab.name,
                state_name(tab.state)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Workspaces and their tab counts (`workspace ls`).
pub fn format_workspaces(workspaces: &[WorkspaceInfo]) -> String {
    workspaces
        .iter()
        .map(|workspace| {
            format!(
                "{}{}  {}  {}  {} tab(s)",
                workspace.id.0,
                if workspace.active { "*" } else { " " },
                workspace.name,
                state_name(workspace.state),
                workspace.tabs.len()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Poll one pane until `check` accepts its snapshot. Returns false on timeout
/// (the caller exits 2); `None` waits forever. Polls `Query(Pane)` rather than
/// the layout so a pane in a background tab or workspace is still visible.
pub async fn poll_pane(
    socket: &Path,
    pane: PaneId,
    timeout: Option<u64>,
    mut check: impl FnMut(&PaneSnapshot) -> bool,
) -> Result<bool> {
    let deadline = timeout.map(|secs| std::time::Instant::now() + Duration::from_secs(secs));
    loop {
        let snapshot =
            pane_snapshot(request(socket, ClientMessage::Query(QueryKind::Pane(pane))).await?)?;
        if check(&snapshot) {
            return Ok(true);
        }
        if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
            return Ok(false);
        }
        tokio::time::sleep(POLL).await;
    }
}

/// A session socket found in the runtime directory, with the result of probing it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SessionEntry {
    pub name: String,
    pub path: String,
    /// False when the socket file is stale (nothing answers on it).
    pub alive: bool,
    /// True when the daemon rebuilt this session and no client has attached yet.
    pub restored: bool,
    pub workspaces: usize,
    pub tabs: usize,
    pub panes: usize,
}

/// Enumerate `*.sock` in the runtime directory and probe each one.
pub async fn session_entries() -> Result<Vec<SessionEntry>> {
    let dir = kodade_cli_daemon::socket_dir();
    let mut entries = Vec::new();
    let mut read = match fs::read_dir(&dir) {
        Ok(read) => read,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(entries),
        Err(error) => return Err(error).context("read the Ködade CLI runtime directory"),
    }
    .filter_map(Result::ok)
    .map(|entry| entry.path())
    .filter(|path| path.extension().is_some_and(|ext| ext == "sock"))
    .collect::<Vec<_>>();
    read.sort();
    for path in read {
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default()
            .to_owned();
        let probe = probe_session(&path).await;
        entries.push(SessionEntry {
            name,
            path: path.display().to_string(),
            alive: probe.is_some(),
            restored: probe.as_ref().is_some_and(|layout| layout.restored),
            workspaces: probe.as_ref().map(|l| l.workspaces.len()).unwrap_or(0),
            tabs: probe.as_ref().map(|l| l.tabs.len()).unwrap_or(0),
            panes: probe.as_ref().map(|l| l.panes.len()).unwrap_or(0),
        });
    }
    Ok(entries)
}

/// Connect to a socket and ask for its layout, giving up after [`PROBE_TIMEOUT`].
async fn probe_session(path: &Path) -> Option<LayoutSnapshot> {
    let probe = async {
        let stream = UnixStream::connect(path).await.ok()?;
        let (reader, mut writer) = stream.into_split();
        writer
            .write_all(&encode(&layout_query()).ok()?)
            .await
            .ok()?;
        let line = BufReader::new(reader).lines().next_line().await.ok()??;
        match decode::<ServerMessage>(line.as_bytes()).ok()? {
            ServerMessage::Layout(layout) => Some(layout),
            _ => None,
        }
    };
    tokio::time::timeout(PROBE_TIMEOUT, probe)
        .await
        .ok()
        .flatten()
}

/// `session ls` text: one line per socket with its size and status.
pub fn format_sessions(entries: &[SessionEntry]) -> String {
    entries
        .iter()
        .map(|entry| {
            let status = if !entry.alive {
                " (dead)"
            } else if entry.restored {
                " (restored)"
            } else {
                ""
            };
            format!(
                "{}  {} workspace(s)  {} tab(s)  {} pane(s){status}",
                entry.name, entry.workspaces, entry.tabs, entry.panes
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One line per event for `kodade-cli events` without `--json`.
pub fn format_event(event: &Event) -> String {
    match event {
        Event::AgentStateChanged { pane, from, to } => format!(
            "agent_state_changed  pane {}  {} -> {}",
            pane.0,
            state_name(*from),
            state_name(*to)
        ),
        Event::PaneOpened { pane } => format!("pane_opened  pane {}", pane.0),
        Event::PaneClosed { pane } => format!("pane_closed  pane {}", pane.0),
        Event::TabOpened { tab } => format!("tab_opened  tab {}", tab.0),
        Event::TabClosed { tab } => format!("tab_closed  tab {}", tab.0),
        Event::TabRenamed { tab, name } => format!("tab_renamed  tab {}  {name}", tab.0),
        Event::WorkspaceOpened { workspace } => {
            format!("workspace_opened  workspace {}", workspace.0)
        }
        Event::WorkspaceClosed { workspace } => {
            format!("workspace_closed  workspace {}", workspace.0)
        }
        Event::WorkspaceRenamed { workspace, name } => {
            format!("workspace_renamed  workspace {}  {name}", workspace.0)
        }
        Event::SessionRenamed { name, socket } => {
            format!("session_renamed  {name}  {}", socket.display())
        }
        Event::Notification(notification) => format!(
            "notification  pane {}  {}  {}",
            notification.pane.0,
            notification.agent,
            state_name(notification.state)
        ),
    }
}

/// Subscribe to a session and print every event until the daemon goes away.
pub async fn stream_events(socket: &Path, json: bool) -> Result<()> {
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("no Ködade CLI daemon at {}", socket.display()))?;
    let (reader, mut writer) = stream.into_split();
    writer
        .write_all(&encode(&ClientMessage::Subscribe)?)
        .await?;
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        match decode::<ServerMessage>(line.as_bytes())? {
            ServerMessage::Event(event) => {
                if json {
                    println!("{}", serde_json::to_string(&event)?);
                } else {
                    println!("{}", format_event(&event));
                }
            }
            ServerMessage::Shutdown => return Ok(()),
            ServerMessage::Error { message } => bail!("{message}"),
            // The subscribe reply snapshot and later layouts are not events.
            _ => continue,
        }
    }
    Ok(())
}

pub fn format_ls(layout: &LayoutSnapshot) -> String {
    let mut lines = Vec::new();
    for workspace in &layout.workspaces {
        lines.push(format!(
            "{} ({})",
            workspace.name,
            state_name(workspace.state)
        ));
        if workspace.active {
            for tab in &layout.tabs {
                lines.push(format!("  {} ({})", tab.name, state_name(tab.state)));
                if tab.active {
                    for pane in &layout.panes {
                        lines.push(format!(
                            "    {} · {} · {} · {}",
                            pane.id.0,
                            pane.title,
                            pane.agent.as_deref().unwrap_or("shell"),
                            state_name(pane.state)
                        ));
                    }
                }
            }
        }
    }
    lines.join("\n")
}

/// Panes with a recognized agent, ordered by pane id — the `agent ls` set.
pub fn agent_panes(layout: &LayoutSnapshot) -> Vec<&PaneSnapshot> {
    let mut panes: Vec<_> = layout
        .panes
        .iter()
        .filter(|pane| pane.agent.is_some())
        .collect();
    panes.sort_by_key(|pane| pane.id.0);
    panes
}

pub fn format_agents(layout: &LayoutSnapshot) -> String {
    agent_panes(layout)
        .into_iter()
        .map(|pane| {
            format!(
                "{}  {}  {}  {}",
                pane.id.0,
                pane.agent.as_deref().unwrap_or("shell"),
                state_name(pane.state),
                pane.state_reason
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Detection lines examined for a manifest rule (mirrors the daemon's window).
const EXPLAIN_LINES: usize = 8;

/// `agent explain` output: the chosen state and reason (the reason already names
/// the matched needle) followed by the bottom-8-line window it matched against.
pub fn format_explain(pane: &kodade_cli_proto::PaneSnapshot) -> String {
    let window = bottom_lines(&pane.screen.contents, EXPLAIN_LINES);
    format!(
        "{}  {}\n{}\nmatched window (bottom {EXPLAIN_LINES} lines):\n{}",
        state_name(pane.state),
        pane.state_reason,
        pane.agent
            .as_ref()
            .map(|agent| format!("agent: {agent}"))
            .unwrap_or_else(|| "agent: none".into()),
        window
    )
}

fn bottom_lines(contents: &str, lines: usize) -> String {
    let all: Vec<&str> = contents.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

/// Resolve a `-w` value (workspace name or numeric id) to a workspace id.
pub fn resolve_workspace(layout: &LayoutSnapshot, needle: &str) -> Result<WorkspaceId> {
    if let Ok(id) = needle.parse::<u64>() {
        if layout
            .workspaces
            .iter()
            .any(|item| item.id == WorkspaceId(id))
        {
            return Ok(WorkspaceId(id));
        }
    }
    layout
        .workspaces
        .iter()
        .find(|item| item.name == needle)
        .map(|item| item.id)
        .ok_or_else(|| anyhow!("workspace '{needle}' not found"))
}

/// Resolve a `-t` value (tab name or numeric id) within a workspace (or the
/// active one when `workspace` is `None`).
pub fn resolve_tab(
    layout: &LayoutSnapshot,
    workspace: Option<WorkspaceId>,
    needle: &str,
) -> Result<TabId> {
    let target = workspace.unwrap_or(layout.active_workspace);
    let workspace = layout
        .workspaces
        .iter()
        .find(|item| item.id == target)
        .ok_or_else(|| anyhow!("workspace not found"))?;
    if let Ok(id) = needle.parse::<u64>() {
        if workspace.tabs.iter().any(|tab| tab.id == TabId(id)) {
            return Ok(TabId(id));
        }
    }
    workspace
        .tabs
        .iter()
        .find(|tab| tab.name == needle)
        .map(|tab| tab.id)
        .ok_or_else(|| anyhow!("tab '{needle}' not found"))
}

/// Resolve a tab name or id anywhere in the session. Tab ids are global and a
/// pane can move between workspaces, so `pane move --tab` cannot be scoped to
/// the active workspace the way `-t` on `run` is.
pub fn resolve_tab_anywhere(layout: &LayoutSnapshot, needle: &str) -> Result<TabId> {
    if let Ok(id) = needle.parse::<u64>() {
        if layout
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.tabs.iter())
            .any(|tab| tab.id == TabId(id))
        {
            return Ok(TabId(id));
        }
    }
    let mut matches = layout
        .workspaces
        .iter()
        .flat_map(|workspace| workspace.tabs.iter())
        .filter(|tab| tab.name == needle);
    let first = matches
        .next()
        .ok_or_else(|| anyhow!("tab '{needle}' not found"))?;
    if matches.next().is_some() {
        bail!("tab name '{needle}' is ambiguous; use its id");
    }
    Ok(first.id)
}

/// The focused pane in a snapshot — used to report the pane a `NewPane` reply
/// just created (the daemon focuses new panes).
pub fn focused_pane(layout: &LayoutSnapshot) -> Result<PaneId> {
    layout
        .panes
        .iter()
        .find(|pane| pane.focused)
        .map(|pane| pane.id)
        .ok_or_else(|| anyhow!("no focused pane in reply"))
}

pub fn find_pane(layout: &LayoutSnapshot, id: PaneId) -> Result<&kodade_cli_proto::PaneSnapshot> {
    layout
        .panes
        .iter()
        .find(|pane| pane.id == id)
        .ok_or_else(|| anyhow!("pane {} not found", id.0))
}

pub fn state_name(state: AgentStateKind) -> &'static str {
    match state {
        AgentStateKind::Blocked => "blocked",
        AgentStateKind::Working => "working",
        AgentStateKind::Done => "done",
        AgentStateKind::Idle => "idle",
        AgentStateKind::Unknown => "unknown",
    }
}

pub fn layout_query() -> ClientMessage {
    ClientMessage::Query(QueryKind::Layout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kodade_cli_proto::{
        LayoutTree, Screen, SidebarTabInfo, TabId, TabInfo, WorkspaceId, WorkspaceInfo,
    };

    fn fixture() -> LayoutSnapshot {
        LayoutSnapshot {
            active_workspace: WorkspaceId(1),
            active_tab: TabId(2),
            workspaces: vec![WorkspaceInfo {
                id: WorkspaceId(1),
                name: "repo".into(),
                active: true,
                state: AgentStateKind::Blocked,
                root: None,
                color: None,
                tabs: vec![SidebarTabInfo {
                    id: TabId(2),
                    name: "agents".into(),
                    state: AgentStateKind::Blocked,
                    agents: vec![],
                }],
            }],
            tabs: vec![TabInfo {
                id: TabId(2),
                name: "agents".into(),
                active: true,
                state: AgentStateKind::Blocked,
            }],
            tree: LayoutTree::Leaf { pane: PaneId(7) },
            panes: vec![PaneSnapshot {
                id: PaneId(7),
                title: "codex task".into(),
                focused: true,
                scroll_offset: 0,
                screen: Screen::default(),
                agent: Some("Codex".into()),
                state: AgentStateKind::Blocked,
                state_reason: "manifest rule 'Allow?' matched".into(),
                state_age_secs: 0,
                cwd: None,
            }],
            zoomed: false,
            restored: false,
        }
    }

    #[test]
    fn resolves_workspaces_and_tabs_by_name_or_id() {
        let layout = fixture();
        assert_eq!(resolve_workspace(&layout, "repo").unwrap(), WorkspaceId(1));
        assert_eq!(resolve_workspace(&layout, "1").unwrap(), WorkspaceId(1));
        assert!(resolve_workspace(&layout, "missing").is_err());
        assert_eq!(resolve_tab(&layout, None, "agents").unwrap(), TabId(2));
        assert_eq!(
            resolve_tab(&layout, Some(WorkspaceId(1)), "2").unwrap(),
            TabId(2)
        );
        assert!(resolve_tab(&layout, None, "nope").is_err());
        assert_eq!(focused_pane(&layout).unwrap(), PaneId(7));
    }

    #[test]
    fn formats_layout_and_agents_for_scripts() {
        let layout = fixture();
        assert_eq!(
            format_ls(&layout),
            "repo (blocked)\n  agents (blocked)\n    7 · codex task · Codex · blocked"
        );
        assert_eq!(
            format_agents(&layout),
            "7  Codex  blocked  manifest rule 'Allow?' matched"
        );
    }

    #[test]
    fn parses_reported_agent_states() {
        assert_eq!(parse_state("done").unwrap(), AgentStateKind::Done);
        assert!(parse_state("busy").is_err());
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
    fn explain_shows_bottom_window_and_reason() {
        let mut layout = fixture();
        layout.panes[0].screen.contents = (1..=10)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let text = format_explain(&layout.panes[0]);
        assert!(text.contains("manifest rule 'Allow?' matched"));
        assert!(text.contains("matched window (bottom 8 lines):"));
        assert!(text.contains("line 3")); // first of the last 8 lines
        assert!(!text.contains("line 2"));
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
    fn formats_pane_tab_and_workspace_listings() {
        let layout = fixture();
        let panes = format_panes(&layout);
        assert!(
            panes.starts_with("7*  codex task  Codex  blocked"),
            "{panes}"
        );
        assert_eq!(format_tabs(&layout.tabs), "2*  agents  blocked");
        assert_eq!(
            format_workspaces(&layout.workspaces),
            "1*  repo  blocked  1 tab(s)"
        );
    }

    #[test]
    fn session_listings_mark_dead_and_restored_sockets() {
        let entries = vec![
            SessionEntry {
                name: "default".into(),
                path: "/tmp/default.sock".into(),
                alive: true,
                restored: false,
                workspaces: 1,
                tabs: 2,
                panes: 3,
            },
            SessionEntry {
                name: "work".into(),
                path: "/tmp/work.sock".into(),
                alive: true,
                restored: true,
                workspaces: 1,
                tabs: 1,
                panes: 1,
            },
            SessionEntry {
                name: "stale".into(),
                path: "/tmp/stale.sock".into(),
                alive: false,
                restored: false,
                workspaces: 0,
                tabs: 0,
                panes: 0,
            },
        ];
        let text = format_sessions(&entries);
        assert_eq!(
            text,
            "default  1 workspace(s)  2 tab(s)  3 pane(s)\n\
             work  1 workspace(s)  1 tab(s)  1 pane(s) (restored)\n\
             stale  0 workspace(s)  0 tab(s)  0 pane(s) (dead)"
        );
    }

    #[test]
    fn events_render_as_one_line_each() {
        assert_eq!(
            format_event(&Event::AgentStateChanged {
                pane: PaneId(3),
                from: AgentStateKind::Working,
                to: AgentStateKind::Blocked,
            }),
            "agent_state_changed  pane 3  working -> blocked"
        );
        assert_eq!(
            format_event(&Event::TabRenamed {
                tab: TabId(2),
                name: "agents".into(),
            }),
            "tab_renamed  tab 2  agents"
        );
    }
    #[test]
    fn tab_names_and_ids_resolve_across_workspaces() {
        let mut layout = fixture();
        // A second workspace with its own tab, as `pane move --tab` would see.
        layout.workspaces.push(WorkspaceInfo {
            id: WorkspaceId(9),
            name: "other".into(),
            active: false,
            state: AgentStateKind::Idle,
            root: None,
            color: None,
            tabs: vec![SidebarTabInfo {
                id: TabId(11),
                name: "notes".into(),
                state: AgentStateKind::Idle,
                agents: vec![],
            }],
        });
        assert_eq!(
            resolve_tab_anywhere(&layout, "notes").expect("name in another workspace"),
            TabId(11)
        );
        assert_eq!(
            resolve_tab_anywhere(&layout, "11").expect("id in another workspace"),
            TabId(11)
        );
        // The active-workspace resolver used by `-t` cannot see it.
        assert!(resolve_tab(&layout, None, "notes").is_err());
        assert!(resolve_tab_anywhere(&layout, "nope").is_err());
    }
}
