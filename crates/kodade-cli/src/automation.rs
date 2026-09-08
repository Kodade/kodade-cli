//! Script-safe agent control built on the existing pane protocol.
//!
//! The module deliberately keeps targeting and prompt lifecycle rules at one
//! seam: callers never need to race a focus change just to address a pane.

use std::{
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{bail, Result};
use kodade_cli_proto::{AgentStateKind, ClientMessage, PaneId, PaneSnapshot};

use crate::{commands, keys, paste};

const POLL: Duration = Duration::from_millis(250);
/// One prompt is written under one PTY writer lock, preventing concurrent
/// scripts from mixing bracketed-paste frames. Larger input should use files.
const MAX_PROMPT_BYTES: usize = 64 * 1024;

/// Starts a command in a new pane. This is intentionally the only start path:
/// automation must never mistake a busy interactive shell for an idle agent.
pub async fn start(
    socket: &Path,
    workspace: Option<kodade_cli_proto::WorkspaceId>,
    tab: Option<kodade_cli_proto::TabId>,
    name: Option<String>,
    command: Vec<String>,
) -> Result<PaneSnapshot> {
    let layout = commands::layout(
        commands::request(
            socket,
            ClientMessage::NewPane {
                workspace,
                tab,
                split: None,
                command: Some(command),
                name,
                context: None,
            },
        )
        .await?,
    )?;
    let id = commands::focused_pane(&layout)?;
    commands::pane_snapshot(
        commands::request(
            socket,
            ClientMessage::Query(kodade_cli_proto::QueryKind::Pane(id)),
        )
        .await?,
    )
}

/// Resolves a named target and rejects shells and unknown processes before an
/// automation action can write to them.
pub async fn agent_target(socket: &Path, target: &str) -> Result<PaneSnapshot> {
    let pane = commands::resolve_agent_target(socket, target).await?;
    if pane.agent.is_none() {
        bail!("target '{target}' is not a recognized agent pane")
    }
    Ok(pane)
}

pub async fn read(
    socket: &Path,
    target: &str,
    scrollback: bool,
    lines: Option<usize>,
) -> Result<(PaneSnapshot, String)> {
    let pane = agent_target(socket, target).await?;
    let text = commands::read_pane(socket, pane.id, scrollback, lines).await?;
    Ok((pane, text))
}

pub async fn send_keys(
    socket: &Path,
    target: &str,
    keys_to_send: &[String],
    literal: bool,
) -> Result<PaneSnapshot> {
    let pane = agent_target(socket, target).await?;
    let bytes = if literal {
        keys::literal(keys_to_send)
    } else {
        keys::parse_all(keys_to_send)?
    };
    send(socket, pane.id, bytes).await?;
    Ok(pane)
}

pub async fn focus(socket: &Path, target: &str) -> Result<PaneSnapshot> {
    let pane = agent_target(socket, target).await?;
    commands::layout(commands::request(socket, ClientMessage::FocusPaneId { id: pane.id }).await?)?;
    Ok(pane)
}

/// Prompt outcome after optional state-aware settling.
pub enum PromptOutcome {
    Sent(PaneSnapshot),
    Settled(PaneSnapshot),
    TimedOut,
}

/// Identity captured before a prompt; the daemon rechecks it for every write
/// and the waiter refuses a pane that changes identity afterward.
struct AgentGuard<'a> {
    pane: PaneId,
    agent: &'a str,
    generation: u64,
}

/// Sends sanitized text as a paste and then an explicit Enter. `--wait` and
/// `--until` observe fresh output or a transition to working before accepting
/// a settled state, so a stale idle/done state cannot satisfy a new prompt.
pub async fn prompt(
    socket: &Path,
    target: &str,
    text: &str,
    wait: bool,
    until: Option<AgentStateKind>,
    timeout: Option<u64>,
) -> Result<PromptOutcome> {
    if timeout.is_some() && !wait && until.is_none() {
        bail!("--timeout requires --wait or --until")
    }
    let deadline = timeout.map(|secs| Instant::now() + Duration::from_secs(secs));
    let pane = match deadline {
        Some(deadline) => {
            match tokio::time::timeout_at(deadline.into(), agent_target(socket, target)).await {
                Ok(result) => result?,
                Err(_) => return Ok(PromptOutcome::TimedOut),
            }
        }
        None => agent_target(socket, target).await?,
    };
    if pane.state == AgentStateKind::Blocked {
        bail!(
            "agent '{}' is blocked; resolve its request before prompting",
            target
        )
    }
    let baseline_screen = pane.screen.contents.clone();
    let baseline_state = pane.state;
    let baseline_activity_revision = pane.activity_revision;
    let clean = paste::sanitize(text);
    let agent = pane
        .agent
        .clone()
        .expect("agent_target guarantees an agent");
    let mut submission = paste::wrap(&clean, pane.screen.bracketed_paste);
    // Bracketed paste inserts text without submitting it. Sending Enter in the
    // same guarded write makes the whole prompt one ordered PTY submission.
    submission.push(b'\r');
    if submission.len() > MAX_PROMPT_BYTES {
        bail!(
            "prompt is {} bytes; the automation limit is {MAX_PROMPT_BYTES} bytes",
            submission.len()
        );
    }
    if !guarded_before_deadline(
        socket,
        pane.id,
        &agent,
        pane.agent_generation,
        submission,
        deadline,
    )
    .await?
    {
        return Ok(PromptOutcome::TimedOut);
    }

    if !wait && until.is_none() {
        return Ok(PromptOutcome::Sent(pane));
    }
    wait_for_settle(
        socket,
        &AgentGuard {
            pane: pane.id,
            agent: &agent,
            generation: pane.agent_generation,
        },
        &baseline_screen,
        baseline_state,
        baseline_activity_revision,
        until,
        deadline,
    )
    .await
}

async fn send(socket: &Path, pane: PaneId, bytes: Vec<u8>) -> Result<()> {
    commands::layout(
        commands::request(socket, ClientMessage::SendToPane { id: pane, bytes }).await?,
    )?;
    Ok(())
}

async fn guarded_prompt(
    socket: &Path,
    pane: PaneId,
    expected_agent: &str,
    expected_generation: u64,
    bytes: Vec<u8>,
) -> Result<PaneSnapshot> {
    commands::pane_snapshot(
        commands::request(
            socket,
            ClientMessage::PromptAgent {
                pane,
                expected_agent: expected_agent.into(),
                expected_generation,
                bytes,
            },
        )
        .await?,
    )
}

async fn guarded_before_deadline(
    socket: &Path,
    pane: PaneId,
    expected_agent: &str,
    expected_generation: u64,
    bytes: Vec<u8>,
    deadline: Option<Instant>,
) -> Result<bool> {
    match deadline {
        Some(deadline) => match tokio::time::timeout_at(
            deadline.into(),
            guarded_prompt(socket, pane, expected_agent, expected_generation, bytes),
        )
        .await
        {
            Ok(result) => {
                result?;
                Ok(true)
            }
            Err(_) => Ok(false),
        },
        None => {
            guarded_prompt(socket, pane, expected_agent, expected_generation, bytes).await?;
            Ok(true)
        }
    }
}

async fn wait_for_settle(
    socket: &Path,
    guard: &AgentGuard<'_>,
    baseline_screen: &str,
    baseline_state: AgentStateKind,
    baseline_activity_revision: u64,
    until: Option<AgentStateKind>,
    deadline: Option<Instant>,
) -> Result<PromptOutcome> {
    let mut fresh = baseline_state == AgentStateKind::Working;
    loop {
        // A removed/replaced pane makes this query fail. Surface that error to
        // the script rather than accidentally resolving the name a second time.
        let query = async {
            commands::pane_snapshot(
                commands::request(
                    socket,
                    ClientMessage::Query(kodade_cli_proto::QueryKind::Pane(guard.pane)),
                )
                .await?,
            )
        };
        let snapshot = match deadline {
            Some(deadline) => match tokio::time::timeout_at(deadline.into(), query).await {
                Ok(result) => result?,
                Err(_) => return Ok(PromptOutcome::TimedOut),
            },
            None => query.await?,
        };
        if snapshot.agent.as_deref() != Some(guard.agent)
            || snapshot.agent_generation != guard.generation
        {
            bail!("agent pane {} was replaced while waiting", guard.pane.0);
        }
        fresh |= has_fresh_activity(&snapshot, baseline_screen)
            || snapshot.activity_revision > baseline_activity_revision;
        if fresh {
            let reached = until
                .map(|state| snapshot.state == state)
                .unwrap_or_else(|| {
                    matches!(
                        snapshot.state,
                        AgentStateKind::Blocked | AgentStateKind::Done | AgentStateKind::Idle
                    )
                });
            if reached {
                return Ok(PromptOutcome::Settled(snapshot));
            }
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Ok(PromptOutcome::TimedOut);
        }
        if let Some(deadline) = deadline {
            if tokio::time::timeout_at(deadline.into(), tokio::time::sleep(POLL))
                .await
                .is_err()
            {
                return Ok(PromptOutcome::TimedOut);
            }
        } else {
            tokio::time::sleep(POLL).await;
        }
    }
}

fn has_fresh_activity(snapshot: &PaneSnapshot, _baseline_screen: &str) -> bool {
    // A repaint or echoed prompt is not enough: waiters only proceed after the
    // daemon has observed the agent working or asking for attention.
    matches!(
        snapshot.state,
        AgentStateKind::Working | AgentStateKind::Blocked
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use kodade_cli_proto::ServerMessage;

    #[tokio::test]
    async fn start_creates_a_new_pane_in_a_real_daemon_session() {
        let session = format!("automation-start-{}", std::process::id());
        let socket = kodade_cli_daemon::socket_path(&session);
        let _ = std::fs::remove_file(&socket);
        let server = tokio::spawn(kodade_cli_daemon::run(session));
        for _ in 0..40 {
            if tokio::net::UnixStream::connect(&socket).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        let pane = start(
            &socket,
            None,
            None,
            Some("fake-agent".into()),
            vec!["printf".into(), "fake-agent-ready".into()],
        )
        .await
        .expect("start pane");
        assert_eq!(pane.title, "fake-agent");

        let mut output = String::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            output = commands::read_pane(&socket, pane.id, true, None)
                .await
                .expect("read started pane");
            if output.contains("fake-agent-ready") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            output.contains("fake-agent-ready"),
            "started pane output: {output:?}"
        );

        let reply = commands::request(&socket, ClientMessage::KillSession)
            .await
            .expect("stop daemon");
        assert!(matches!(reply, ServerMessage::Shutdown));
        server
            .await
            .expect("server task")
            .expect("server exits cleanly");
    }

    #[test]
    fn stale_idle_or_done_cannot_satisfy_a_new_prompt_wait() {
        let mut snapshot = PaneSnapshot {
            id: PaneId(1),
            title: "agent".into(),
            focused: true,
            scroll_offset: 0,
            screen: Default::default(),
            agent: Some("Codex".into()),
            agent_generation: 1,
            activity_revision: 0,
            state: AgentStateKind::Idle,
            state_reason: "idle".into(),
            state_age_secs: 1,
            cwd: None,
        };
        assert!(!has_fresh_activity(&snapshot, ""));
        snapshot.state = AgentStateKind::Done;
        assert!(!has_fresh_activity(&snapshot, ""));
        snapshot.state = AgentStateKind::Working;
        assert!(has_fresh_activity(&snapshot, ""));
    }
}
