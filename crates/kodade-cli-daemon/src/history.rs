//! Opt-in, bounded private cold-restart terminal replay.
//!
//! The public interface is intentionally small: callers provide the layout
//! fingerprint and pane screens, and this module owns validation, limits and
//! atomic on-disk publication. It never writes unless the caller opted in.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use kodade_cli_proto::{Screen, SessionFile};
use serde::{Deserialize, Serialize};

const VERSION: u32 = 1;
pub const MAX_PANE_TEXT: usize = 64 * 1024;
pub const MAX_TOTAL_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_INPUT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_DIMENSION: usize = 512;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryFile {
    version: u32,
    /// A complete layout identity prevents replay against a copied/stale layout
    /// where fresh panes happen to receive the same numeric ids.
    session: SessionFile,
    panes: Vec<PaneHistory>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneHistory {
    pub pane: u64,
    pub text: String,
    pub screen: Screen,
}

pub fn path_for(session_state: &Path) -> PathBuf {
    session_state.with_extension("history.json")
}

pub fn remove_for_session(name: &str) {
    if let Some(state) = crate::persist::session_file_path(name) {
        let _ = fs::remove_file(path_for(&state));
    }
}

pub fn rename_for_session(old: &str, new: &str) {
    let (Some(old_state), Some(new_state)) = (
        crate::persist::session_file_path(old),
        crate::persist::session_file_path(new),
    ) else {
        return;
    };
    let old = path_for(&old_state);
    let new = path_for(&new_state);
    let _ = fs::rename(old, new);
}

pub fn read(path: &Path, expected: &SessionFile) -> Result<Option<Vec<PaneHistory>>> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inspect pane history"),
    };
    if metadata.len() > MAX_INPUT_BYTES {
        return Ok(None);
    }
    let bytes = fs::read(path).context("read pane history")?;
    let history: HistoryFile = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    if history.version != VERSION || &history.session != expected || !valid(&history) {
        return Ok(None);
    }
    Ok(Some(history.panes))
}

pub fn write(path: &Path, session: SessionFile, panes: Vec<PaneHistory>) -> Result<()> {
    let mut kept = Vec::new();
    for pane in panes {
        let mut candidate = kept.clone();
        candidate.push(pane);
        let history = HistoryFile {
            version: VERSION,
            session: session.clone(),
            panes: candidate,
        };
        if valid(&history) {
            kept = history.panes;
        }
    }
    let history = HistoryFile {
        version: VERSION,
        session,
        panes: kept,
    };
    let mut bytes = serde_json::to_vec(&history).context("serialize pane history")?;
    bytes.push(b'\n');
    crate::persist::write_private_file(path, &bytes)
}

fn valid(history: &HistoryFile) -> bool {
    if history
        .panes
        .iter()
        .any(|pane| pane.text.len() > MAX_PANE_TEXT || !valid_screen(&pane.screen))
    {
        return false;
    }
    serde_json::to_vec(history).is_ok_and(|bytes| bytes.len() <= MAX_TOTAL_BYTES)
}

fn valid_screen(screen: &Screen) -> bool {
    screen.rows.len() <= MAX_DIMENSION
        && screen.rows.iter().all(|row| row.len() <= MAX_DIMENSION)
        && screen.cursor_row as usize <= MAX_DIMENSION
        && screen.cursor_col as usize <= MAX_DIMENSION
}

#[cfg(test)]
mod tests {
    use super::*;
    use kodade_cli_proto::{
        LayoutTree, PaneFile, PaneId, TabFile, WorkspaceFile, SESSION_FILE_VERSION,
    };

    fn session() -> SessionFile {
        SessionFile {
            version: SESSION_FILE_VERSION,
            name: "test".into(),
            active_workspace: 1,
            workspaces: vec![WorkspaceFile {
                id: 1,
                name: "test".into(),
                root: None,
                color: None,
                active_tab: 2,
                tabs: vec![TabFile {
                    id: 2,
                    name: "shell".into(),
                    zoomed: false,
                    focused: 3,
                    tree: LayoutTree::Leaf { pane: PaneId(3) },
                    panes: vec![PaneFile {
                        id: 3,
                        title: "shell".into(),
                        cwd: None,
                        command: None,
                    }],
                }],
            }],
        }
    }
    fn pane() -> PaneHistory {
        PaneHistory {
            pane: 3,
            text: "ready".into(),
            screen: Screen::default(),
        }
    }
    #[test]
    fn rejects_stale_or_oversized_history_without_panicking() {
        let dir = std::env::temp_dir().join(format!("kodade-history-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.history.json");
        write(&path, session(), vec![pane()]).unwrap();
        assert_eq!(read(&path, &session()).unwrap().unwrap()[0].text, "ready");
        let mut other = session();
        other.name = "other".into();
        assert!(read(&path, &other).unwrap().is_none());
        fs::write(&path, vec![b'x'; MAX_INPUT_BYTES as usize + 1]).unwrap();
        assert!(read(&path, &session()).unwrap().is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn writes_owner_private_history() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("kodade-history-mode-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("sessions/test.history.json");
        write(&path, session(), vec![pane()]).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let _ = fs::remove_dir_all(dir);
    }
}
