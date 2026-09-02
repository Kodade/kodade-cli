//! Session layout persistence (#9).
//!
//! The daemon writes a versioned JSON snapshot of its layout — workspaces, tabs,
//! trees, pane titles, cwds, and the command each pane was spawned with — to a
//! per-session state file, debounced so PTY output never triggers a write. On a
//! cold start the daemon rebuilds that layout with fresh panes. Scrollback is
//! never persisted (secrets risk); only structure and metadata are.

#[cfg(test)]
use std::time::Instant;
use std::{
    env, fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use serde::Deserialize;

/// Debounce window: layout changes within this span collapse into one write.
pub const DEBOUNCE: Duration = Duration::from_millis(500);

/// The session-file types now live in the proto crate so `layout export` /
/// `layout apply` can carry them on the wire (#16); persistence keeps using
/// them under these names.
pub use kodade_cli_proto::{PaneFile, SessionFile, TabFile, WorkspaceFile};

/// State directory for the current platform, honoring `XDG_STATE_HOME`.
pub fn state_dir() -> Option<PathBuf> {
    let xdg = env::var_os("XDG_STATE_HOME").map(PathBuf::from);
    state_dir_for(
        xdg.as_deref(),
        dirs::home_dir().as_deref(),
        cfg!(target_os = "macos"),
    )
}

/// Pure resolver behind [`state_dir`] so the platform rules can be unit-tested.
fn state_dir_for(xdg: Option<&Path>, home: Option<&Path>, is_macos: bool) -> Option<PathBuf> {
    if let Some(xdg) = xdg {
        return Some(xdg.join("kodade-cli"));
    }
    let home = home?;
    if is_macos {
        Some(home.join("Library/Application Support/kodade-cli"))
    } else {
        Some(home.join(".local/state/kodade-cli"))
    }
}

/// Path of a session's state file: `<state_dir>/sessions/<name>.json`.
pub fn session_file_path(name: &str) -> Option<PathBuf> {
    Some(state_dir()?.join("sessions").join(format!("{name}.json")))
}

/// Read and validate a session file. `Ok(None)` means "no file, start fresh";
/// `Err` means the file exists but is corrupt/foreign — the caller should move
/// it aside via [`quarantine`] and start clean.
pub fn read_session_file(path: &Path) -> Result<Option<SessionFile>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read session state file"),
    };
    let file: SessionFile = serde_json::from_str(&text).context("parse session state file")?;
    file.validate()?;
    Ok(Some(file))
}

/// Rename a bad state file to `<name>.json.broken` so a clean start does not
/// silently discard it. Best-effort: a failure here should never stop startup.
pub fn quarantine(path: &Path) {
    let broken = path.with_extension("json.broken");
    if let Err(error) = fs::rename(path, &broken) {
        eprintln!(
            "Ködade CLI could not set aside corrupt state file {}: {error:#}",
            path.display()
        );
    } else {
        eprintln!(
            "Ködade CLI moved corrupt state file to {}",
            broken.display()
        );
    }
}

/// Atomically write a session file: serialize to `<path>.tmp`, then rename over
/// the target so a reader never sees a half-written file.
pub fn write_session_file(path: &Path, file: &SessionFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("create session state directory")?;
    }
    let temp = path.with_extension("json.tmp");
    let mut bytes = serde_json::to_vec_pretty(file).context("serialize session state")?;
    bytes.push(b'\n');
    fs::write(&temp, &bytes).context("write session state temp file")?;
    fs::rename(&temp, path).context("commit session state file")?;
    Ok(())
}

/// Remove a session's state file (and any leftover temp), e.g. on an explicit
/// `kill-session` so the session does not resurrect on the next cold start.
pub fn remove_session_file(name: &str) {
    if let Some(path) = session_file_path(name) {
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(path.with_extension("json.tmp"));
    }
}

/// Daemon-side view of `~/.config/kodade-cli/config.toml`. Only `[session]
/// resume_agents` is read here; the client owns the full config (#20). Unknown
/// keys and tables are ignored, so the shared file loads for either side.
#[derive(Debug, Default, Deserialize)]
struct DaemonConfig {
    #[serde(default)]
    session: SessionConfig,
}

#[derive(Debug, Default, Deserialize)]
struct SessionConfig {
    #[serde(default)]
    resume_agents: bool,
}

/// Whether restored agent panes should re-run their resume command. Any read or
/// parse failure is treated as `false` (the safe default).
pub fn resume_agents_setting() -> bool {
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    let path = home.join(".config/kodade-cli/config.toml");
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    toml::from_str::<DaemonConfig>(&text)
        .map(|config| config.session.resume_agents)
        .unwrap_or(false)
}

/// Number of debounced writes produced by change events at the given instants.
/// The first change in a quiet period arms a `window` timer; every change that
/// lands before the timer expires collapses into that one write. Pins down the
/// coalescing contract the runtime persist loop implements (test-only).
#[cfg(test)]
fn debounced_writes(changes: &[Instant], window: Duration) -> usize {
    let mut writes = 0;
    let mut armed_until: Option<Instant> = None;
    for &change in changes {
        match armed_until {
            Some(deadline) if change < deadline => {} // absorbed into the pending write
            _ => {
                writes += 1;
                armed_until = Some(change + window);
            }
        }
    }
    writes
}

#[cfg(test)]
mod tests {
    use super::*;
    use kodade_cli_proto::{LayoutTree, PaneId, SplitAxis, SESSION_FILE_VERSION as VERSION};

    fn sample_file() -> SessionFile {
        // Two workspaces, three tabs, four panes: ws "one" has a split tab of
        // two panes plus a single-pane tab; ws "two" has one single-pane tab.
        SessionFile {
            version: VERSION,
            name: "demo".into(),
            active_workspace: 10,
            workspaces: vec![
                WorkspaceFile {
                    id: 10,
                    name: "one".into(),
                    root: Some(PathBuf::from("/tmp")),
                    color: Some("#e7a33b".into()),
                    active_tab: 20,
                    tabs: vec![
                        TabFile {
                            id: 20,
                            name: "agents".into(),
                            zoomed: false,
                            focused: 31,
                            tree: LayoutTree::Split {
                                axis: SplitAxis::Horizontal,
                                ratio: 0.5,
                                first: Box::new(LayoutTree::Leaf { pane: PaneId(30) }),
                                second: Box::new(LayoutTree::Leaf { pane: PaneId(31) }),
                            },
                            panes: vec![
                                PaneFile {
                                    id: 30,
                                    title: "codex".into(),
                                    cwd: Some(PathBuf::from("/tmp")),
                                    command: Some(vec!["codex".into()]),
                                },
                                PaneFile {
                                    id: 31,
                                    title: "shell".into(),
                                    cwd: Some(PathBuf::from("/")),
                                    command: None,
                                },
                            ],
                        },
                        TabFile {
                            id: 21,
                            name: "logs".into(),
                            zoomed: true,
                            focused: 32,
                            tree: LayoutTree::Leaf { pane: PaneId(32) },
                            panes: vec![PaneFile {
                                id: 32,
                                title: "tail".into(),
                                cwd: None,
                                command: None,
                            }],
                        },
                    ],
                },
                WorkspaceFile {
                    id: 11,
                    name: "two".into(),
                    root: None,
                    color: None,
                    active_tab: 22,
                    tabs: vec![TabFile {
                        id: 22,
                        name: "shell".into(),
                        zoomed: false,
                        focused: 33,
                        tree: LayoutTree::Leaf { pane: PaneId(33) },
                        panes: vec![PaneFile {
                            id: 33,
                            title: "shell".into(),
                            cwd: Some(PathBuf::from("/tmp")),
                            command: None,
                        }],
                    }],
                },
            ],
        }
    }

    #[test]
    fn session_file_round_trips_through_json() {
        let file = sample_file();
        let text = serde_json::to_string_pretty(&file).expect("serialize");
        let parsed: SessionFile = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(parsed, file);
        parsed.validate().expect("sample is valid");
    }

    #[test]
    fn validate_rejects_trees_naming_missing_panes() {
        let mut file = sample_file();
        // Point a leaf at a pane with no matching entry.
        file.workspaces[1].tabs[0].tree = LayoutTree::Leaf { pane: PaneId(999) };
        assert!(file.validate().is_err());
    }

    #[test]
    fn unknown_fields_are_ignored_on_read() {
        let json = r#"{
            "version": 1,
            "name": "demo",
            "active_workspace": 1,
            "future_key": "ignored",
            "workspaces": [
                { "id": 1, "name": "one", "active_tab": 2, "tabs": [
                    { "id": 2, "name": "shell", "focused": 3,
                      "tree": { "Leaf": { "pane": 3 } },
                      "panes": [ { "id": 3, "title": "shell" } ] }
                ] }
            ]
        }"#;
        let file: SessionFile = serde_json::from_str(json).expect("tolerant parse");
        file.validate().expect("valid despite extra key");
        assert_eq!(file.workspaces[0].tabs[0].panes[0].title, "shell");
    }

    #[test]
    fn corrupt_file_reads_as_error_and_quarantines() {
        let dir =
            std::env::temp_dir().join(format!("kodade-persist-corrupt-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.json");
        fs::write(&path, b"not json at all").unwrap();
        assert!(read_session_file(&path).is_err());
        quarantine(&path);
        assert!(!path.exists());
        assert!(path.with_extension("json.broken").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unknown_version_reads_as_error() {
        let dir =
            std::env::temp_dir().join(format!("kodade-persist-version-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("v99.json");
        let mut file = sample_file();
        file.version = 99;
        fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
        assert!(read_session_file(&path).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_file_reads_as_none() {
        let path = std::env::temp_dir().join("kodade-persist-does-not-exist.json");
        let _ = fs::remove_file(&path);
        assert!(read_session_file(&path).unwrap().is_none());
    }

    #[test]
    fn atomic_write_then_read_round_trips() {
        let dir = std::env::temp_dir().join(format!("kodade-persist-write-{}", std::process::id()));
        let path = dir.join("sessions").join("demo.json");
        write_session_file(&path, &sample_file()).expect("write");
        let read = read_session_file(&path).expect("read").expect("present");
        assert_eq!(read, sample_file());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn state_dir_follows_platform_rules() {
        assert_eq!(
            state_dir_for(Some(Path::new("/xdg/state")), None, true),
            Some(PathBuf::from("/xdg/state/kodade-cli"))
        );
        assert_eq!(
            state_dir_for(None, Some(Path::new("/Users/keith")), true),
            Some(PathBuf::from(
                "/Users/keith/Library/Application Support/kodade-cli"
            ))
        );
        assert_eq!(
            state_dir_for(None, Some(Path::new("/home/keith")), false),
            Some(PathBuf::from("/home/keith/.local/state/kodade-cli"))
        );
        assert_eq!(state_dir_for(None, None, false), None);
    }

    #[test]
    fn debounce_coalesces_rapid_changes_into_one_write() {
        let start = Instant::now();
        // Ten changes all within the 500 ms window collapse into one write.
        let rapid: Vec<Instant> = (0..10)
            .map(|n| start + Duration::from_millis(n * 5))
            .collect();
        assert_eq!(debounced_writes(&rapid, DEBOUNCE), 1);
        // A change past the window opens a second write.
        let spaced = [start, start + Duration::from_millis(600)];
        assert_eq!(debounced_writes(&spaced, DEBOUNCE), 2);
        assert_eq!(debounced_writes(&[], DEBOUNCE), 0);
    }
}
