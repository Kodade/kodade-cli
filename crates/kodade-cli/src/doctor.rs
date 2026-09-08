//! Read-only diagnostics for first use and existing sessions.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Result;
use kodade_cli_proto::{ClientMessage, QueryKind, ServerMessage, PROTOCOL_VERSION};
use serde::Serialize;

use crate::{commands, config, connection};

#[derive(Serialize)]
struct Check {
    name: String,
    status: &'static str,
    detail: String,
}

#[derive(Serialize)]
struct Report {
    version: &'static str,
    session: String,
    socket: PathBuf,
    checks: Vec<Check>,
}

pub async fn run(socket: &Path, session: &str, json: bool) -> Result<()> {
    let mut checks = Vec::new();
    let (status, detail) = match config::Config::load_checked() {
        Ok(config) if config.warnings.is_empty() => (
            "ok",
            format!(
                "{}{}",
                config::config_path().display(),
                if config::config_path().exists() {
                    ""
                } else {
                    " (defaults in use)"
                }
            ),
        ),
        Ok(config) => ("warning", config.warnings.join("; ")),
        Err(error) => ("error", error),
    };
    checks.push(Check {
        name: "configuration".into(),
        status,
        detail,
    });
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    checks.push(tool_check("login shell", &shell, true));
    for (label, command) in [
        ("git worktrees", "git"),
        ("SSH machines", "ssh"),
        ("manifest refresh", "curl"),
        ("Codex", "codex"),
        ("Claude Code", "claude"),
        ("Gemini CLI", "gemini"),
        ("OpenCode", "opencode"),
        ("Pi", "pi"),
    ] {
        checks.push(tool_check(label, command, false));
    }
    let (status, detail) = match tokio::time::timeout(
        Duration::from_secs(1),
        commands::request(socket, ClientMessage::Query(QueryKind::Version)),
    )
    .await
    {
        Ok(Ok(ServerMessage::Version { version: protocol })) if protocol == PROTOCOL_VERSION => {
            ("ok", format!("running · protocol {protocol}"))
        }
        Ok(Ok(ServerMessage::Version { version: protocol })) => (
            "error",
            format!("daemon protocol {protocol}; client requires {PROTOCOL_VERSION}"),
        ),
        Ok(Ok(message)) => (
            "error",
            format!(
                "unexpected daemon response: {}",
                kodade_cli_proto::server_message_name(&message)
            ),
        ),
        Ok(Err(error)) if !socket.exists() => (
            "info",
            format!(
                "not running; creating a workspace or attaching starts it automatically ({error})"
            ),
        ),
        Ok(Err(error)) => ("error", error.to_string()),
        Err(_) => ("error", "socket did not answer within 1s".into()),
    };
    checks.push(Check {
        name: "daemon".into(),
        status,
        detail,
    });
    let log = connection::daemon_log(socket);
    if log.exists() {
        checks.push(Check {
            name: "daemon log".into(),
            status: "info",
            detail: log.display().to_string(),
        });
    }
    let report = Report {
        version: env!("CARGO_PKG_VERSION"),
        session: session.into(),
        socket: socket.into(),
        checks,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "Ködade CLI {} · {}\n{}\n",
            report.version,
            session,
            socket.display()
        );
        for check in &report.checks {
            println!("{:<7} {:<18} {}", check.status, check.name, check.detail);
        }
    }
    if report.checks.iter().any(|check| check.status == "error") {
        anyhow::bail!("diagnostics found errors");
    }
    Ok(())
}

fn tool_check(label: &str, command: &str, required: bool) -> Check {
    let path = executable(command);
    Check {
        name: label.into(),
        status: if path.is_some() {
            "ok"
        } else if required {
            "error"
        } else {
            "optional"
        },
        detail: path
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| format!("{command} not on PATH")),
    }
}

/// Check PATH without running an executable or interpreting shell syntax.
fn executable(command: &str) -> Option<PathBuf> {
    let candidates = if command.contains(std::path::MAIN_SEPARATOR) {
        vec![PathBuf::from(command)]
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join(command))
            .collect()
    };
    candidates.into_iter().find(|path| {
        path.metadata().is_ok_and(|metadata| {
            if !metadata.is_file() {
                return false;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            {
                true
            }
        })
    })
}
