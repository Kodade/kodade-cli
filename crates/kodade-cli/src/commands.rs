use anyhow::{anyhow, bail, Context, Result};
use kodade_cli_proto::{
    decode, encode, server_message_name, AgentStateKind, ClientMessage, Event, LayoutSnapshot,
    PaneId, PaneSnapshot, QueryKind, ServerMessage, SessionFile, TabId, TabInfo, WorkspaceId,
    WorkspaceInfo,
};
use regex::Regex;
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
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

/// Fetch the pane text exactly as `pane read` does, including scrollback when
/// requested. Keeping this at the socket seam makes waits work for background
/// tabs and workspaces too.
pub async fn read_pane(
    socket: &Path,
    pane: PaneId,
    scrollback: bool,
    lines: Option<usize>,
) -> Result<String> {
    match request(
        socket,
        ClientMessage::ReadPane {
            id: pane,
            scrollback,
            lines,
        },
    )
    .await?
    {
        ServerMessage::PaneText { text, .. } => Ok(text),
        other => bail!("daemon sent an unexpected {}", server_message_name(&other)),
    }
}

/// Compile the caller's requested output matcher once before polling.
pub fn output_matcher(text: &str, regex: bool) -> Result<OutputMatcher> {
    if regex {
        Ok(OutputMatcher::Regex(
            Regex::new(text).context("invalid --regex pattern")?,
        ))
    } else {
        Ok(OutputMatcher::Literal(text.to_owned()))
    }
}

pub enum OutputMatcher {
    Literal(String),
    Regex(Regex),
}

impl OutputMatcher {
    pub fn matches(&self, text: &str) -> bool {
        match self {
            Self::Literal(needle) => text.contains(needle),
            Self::Regex(pattern) => pattern.is_match(text),
        }
    }
}

/// Resolve a script target that names a numeric pane id or a unique
/// recognized-agent label / pane title. The caller validates whether
/// the resulting pane is an agent before it sends input.
pub async fn resolve_agent_target(socket: &Path, target: &str) -> Result<PaneSnapshot> {
    let layout = layout(request(socket, layout_query()).await?)?;
    if let Ok(id) = target.parse::<u64>() {
        return pane_snapshot(
            request(socket, ClientMessage::Query(QueryKind::Pane(PaneId(id)))).await?,
        );
    }

    // Layout snapshots carry all recognized agents in sidebar metadata, while
    // full pane titles travel on individual Pane queries. Querying this small
    // candidate set keeps title resolution global without broadening the wire
    // protocol or relying on the focused tab.
    let candidate_ids = layout
        .workspaces
        .iter()
        .flat_map(|workspace| workspace.tabs.iter())
        .flat_map(|tab| tab.agents.iter())
        .map(|agent| agent.pane)
        .chain(layout.panes.iter().map(|pane| pane.id))
        .collect::<std::collections::HashSet<_>>();
    let mut matches = Vec::new();
    for id in candidate_ids {
        let pane =
            pane_snapshot(request(socket, ClientMessage::Query(QueryKind::Pane(id))).await?)?;
        if pane.agent.as_deref() == Some(target) || pane.title == target {
            matches.push(pane);
        }
    }
    match matches.len() {
        0 => bail!("agent target '{target}' not found; use a pane id, agent name, or pane title"),
        1 => Ok(matches.remove(0)),
        _ => bail!("agent target '{target}' is ambiguous; use a pane id"),
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
    let read = session_socket_paths(&dir)?;
    let mut entries = Vec::new();
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

fn session_socket_paths(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = Vec::new();
    let mut read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(entries),
        Err(error) => return Err(error).context("read the Ködade CLI runtime directory"),
    }
    .filter_map(Result::ok)
    .map(|entry| entry.path())
    .filter(|path| path.extension().is_some_and(|ext| ext == "sock"))
    .collect::<Vec<_>>();
    read.sort();
    Ok(read)
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

/// Workspaces that carry a git branch — the `worktree list` set. Includes any
/// workspace rooted in a repo, not just linked worktrees (#22).
pub fn worktree_workspaces(layout: &LayoutSnapshot) -> Vec<&kodade_cli_proto::WorkspaceInfo> {
    layout
        .workspaces
        .iter()
        .filter(|workspace| workspace.branch.is_some())
        .collect()
}

/// Resolve a `worktree remove` target — a workspace id, workspace name, or the
/// branch of a worktree workspace — to a workspace id.
pub fn resolve_worktree(layout: &LayoutSnapshot, needle: &str) -> Result<WorkspaceId> {
    if let Ok(id) = resolve_workspace(layout, needle) {
        return Ok(id);
    }
    layout
        .workspaces
        .iter()
        .find(|workspace| workspace.branch.as_deref() == Some(needle))
        .map(|workspace| workspace.id)
        .ok_or_else(|| anyhow!("no worktree workspace for '{needle}'"))
}

/// Human-readable `worktree list`: one line per branch workspace with its id,
/// name, branch, root, and parent workspace name when nested (#22).
pub fn format_worktrees(layout: &LayoutSnapshot) -> String {
    worktree_workspaces(layout)
        .into_iter()
        .map(|workspace| {
            let root = workspace
                .root
                .as_deref()
                .map(|root| root.display().to_string())
                .unwrap_or_default();
            let parent = workspace
                .parent
                .and_then(|id| layout.workspaces.iter().find(|item| item.id == id))
                .map(|parent| format!("  parent {}", parent.name))
                .unwrap_or_default();
            format!(
                "{}  {}  ⎇ {}  {root}{parent}",
                workspace.id.0,
                workspace.name,
                workspace.branch.as_deref().unwrap_or("-"),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
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
                branch: None,
                parent: None,
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
                agent_generation: 1,
                activity_revision: 0,
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
    fn worktree_list_and_resolve_by_branch() {
        let mut layout = fixture();
        layout.workspaces[0].branch = Some("main".into());
        layout.workspaces.push(WorkspaceInfo {
            id: WorkspaceId(2),
            name: "repo:feat-a".into(),
            active: false,
            state: AgentStateKind::Idle,
            root: Some("/tmp/wt/repo/feat-a".into()),
            color: None,
            branch: Some("feat-a".into()),
            parent: Some(WorkspaceId(1)),
            tabs: vec![],
        });
        // Both branch workspaces are listed; the child shows its parent.
        assert_eq!(worktree_workspaces(&layout).len(), 2);
        let text = format_worktrees(&layout);
        assert!(text.contains("⎇ feat-a"));
        assert!(text.contains("parent repo"));
        // Remove resolves by branch, workspace id, or name.
        assert_eq!(resolve_worktree(&layout, "feat-a").unwrap(), WorkspaceId(2));
        assert_eq!(resolve_worktree(&layout, "2").unwrap(), WorkspaceId(2));
        assert!(resolve_worktree(&layout, "nope").is_err());
    }

    #[test]
    fn parses_reported_agent_states() {
        assert_eq!(parse_state("done").unwrap(), AgentStateKind::Done);
        assert!(parse_state("busy").is_err());
    }

    #[test]
    fn output_matchers_keep_literal_default_and_validate_regexes() {
        assert!(output_matcher("build [42]", false)
            .unwrap()
            .matches("build [42] complete"));
        assert!(output_matcher(r"build \d+", true)
            .unwrap()
            .matches("build 42 complete"));
        assert!(output_matcher("[", true).is_err());
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
    fn session_listing_ignores_private_hook_socket_subdirectory() {
        let directory =
            std::env::temp_dir().join(format!("kodade-session-list-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(directory.join("hooks")).unwrap();
        fs::write(directory.join("work.sock"), []).unwrap();
        fs::write(directory.join("hooks/123.sock"), []).unwrap();
        assert_eq!(
            session_socket_paths(&directory).unwrap(),
            vec![directory.join("work.sock")]
        );
        fs::remove_dir_all(directory).unwrap();
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
            branch: None,
            parent: None,
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
