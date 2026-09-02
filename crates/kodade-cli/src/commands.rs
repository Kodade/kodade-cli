use anyhow::{anyhow, bail, Context, Result};
use kodade_cli_proto::{
    decode, encode, AgentStateKind, ClientMessage, LayoutSnapshot, PaneId, PaneSnapshot, QueryKind,
    ServerMessage,
};
use serde_json::{json, Value};
use std::{fs, path::Path};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

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
    merge_claude_settings_path(&path)?;
    println!("installed Claude Code hooks in {}", path.display());
    Ok(())
}

fn claude_hooks() -> Value {
    json!({
        "Stop": [{ "hooks": [{ "type": "command", "command": "kodade-cli agent report $KODADE_PANE idle -s \"$KODADE_SESSION\"" }] }],
        "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "kodade-cli agent report $KODADE_PANE working -s \"$KODADE_SESSION\"" }] }],
        "Notification": [{ "hooks": [{ "type": "command", "command": "kodade-cli agent report $KODADE_PANE blocked -s \"$KODADE_SESSION\"" }] }]
    })
}

fn merge_claude_settings_path(path: &Path) -> Result<()> {
    let mut settings: Value = match fs::read_to_string(path) {
        Ok(source) => serde_json::from_str(&source).context("parse Claude settings.json")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error.into()),
    };
    let hooks = settings
        .as_object_mut()
        .ok_or_else(|| anyhow!("Claude settings.json must be an object"))?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow!("Claude hooks must be an object"))?;
    for (event, entries) in claude_hooks().as_object().expect("hooks object") {
        let destination = hooks
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| anyhow!("Claude hook event must be an array"))?;
        let command = entries[0]["hooks"][0]["command"].as_str().expect("command");
        let exists = destination.iter().any(|entry| {
            entry["hooks"].as_array().is_some_and(|nested| {
                nested
                    .iter()
                    .any(|hook| hook["command"].as_str() == Some(command))
            })
        });
        if !exists {
            destination.extend(entries.as_array().expect("entries").iter().cloned());
        }
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

pub async fn request(session: &str, message: ClientMessage) -> Result<ServerMessage> {
    let path = kodade_cli_daemon::socket_path(session);
    let stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("no Ködade CLI daemon for session '{session}'"))?;
    let (reader, mut writer) = stream.into_split();
    writer.write_all(&encode(&message)?).await?;
    let mut lines = BufReader::new(reader).lines();
    let line = lines
        .next_line()
        .await?
        .ok_or_else(|| anyhow!("daemon closed the connection"))?;
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
        ServerMessage::Welcome { .. } => bail!("daemon sent an unexpected welcome"),
        ServerMessage::Error { message } => bail!("{message}"),
    }
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
    use kodade_cli_proto::{LayoutTree, Screen, TabId, TabInfo, WorkspaceId, WorkspaceInfo};

    fn fixture() -> LayoutSnapshot {
        LayoutSnapshot {
            active_workspace: WorkspaceId(1),
            active_tab: TabId(2),
            workspaces: vec![WorkspaceInfo {
                id: WorkspaceId(1),
                name: "repo".into(),
                active: true,
                state: AgentStateKind::Blocked,
                tabs: vec![],
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
            }],
            zoomed: false,
        }
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
    fn merges_claude_settings_without_duplicate_hooks() {
        let temp = std::env::temp_dir().join(format!("kodade-cli-hooks-{}", std::process::id()));
        let path = temp.join("settings.json");
        fs::create_dir_all(&temp).unwrap();
        fs::write(&path, r#"{"theme":"dark","hooks":{"Stop":[]}}"#).unwrap();
        merge_claude_settings_path(&path).unwrap();
        merge_claude_settings_path(&path).unwrap();
        let settings: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(settings["theme"], "dark");
        assert_eq!(settings["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert_eq!(
            settings["hooks"]["Notification"].as_array().unwrap().len(),
            1
        );
        fs::remove_dir_all(temp).unwrap();
    }
}
