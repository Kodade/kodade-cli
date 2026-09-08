//! Opt-in, bounded private cold-restart terminal replay.
//!
//! The public interface is intentionally small: callers provide the layout
//! fingerprint and pane screens, and this module owns validation, limits and
//! atomic on-disk publication. It never writes unless the caller opted in.

use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use kodade_cli_proto::{Screen, SessionFile};
use serde::{Deserialize, Serialize};
use unicode_width::UnicodeWidthStr;

const VERSION: u32 = 1;
pub const MAX_PANE_TEXT: usize = 64 * 1024;
const MAX_PANE_BYTES: usize = 64 * 1024;
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
    let old_path = path_for(&old_state);
    let new_path = path_for(&new_state);
    let Ok(Some(mut history)) = read_unchecked(&old_path) else {
        return;
    };
    history.session.name = new.to_owned();
    let Ok(mut bytes) = serde_json::to_vec(&history) else {
        return;
    };
    bytes.push(b'\n');
    if crate::persist::write_private_file(&new_path, &bytes).is_ok() {
        let _ = fs::remove_file(old_path);
    }
}

pub fn read(path: &Path, expected: &SessionFile) -> Result<Option<Vec<PaneHistory>>> {
    let Some(history) = read_unchecked(path)? else {
        return Ok(None);
    };
    if history.version != VERSION || &history.session != expected || !valid(&history) {
        return Ok(None);
    }
    Ok(Some(history.panes))
}

pub fn write(path: &Path, session: SessionFile, panes: Vec<PaneHistory>) -> Result<()> {
    let mut panes = panes;
    panes.sort_by_key(|pane| pane.pane);
    let mut history = HistoryFile {
        version: VERSION,
        session,
        panes: Vec::new(),
    };
    let mut used = serde_json::to_vec(&history)?.len();
    for mut pane in panes {
        if !valid_screen(&pane.screen) {
            continue;
        }
        // Preserve the active screen and as much recent text as its record
        // budget allows. A long scrollback must not discard a normal screen.
        pane.text = crate::utf8_tail(&pane.text, MAX_PANE_TEXT);
        let mut encoded = serde_json::to_vec(&pane)?;
        while encoded.len() > MAX_PANE_BYTES && !pane.text.is_empty() {
            let keep = pane
                .text
                .len()
                .saturating_sub(encoded.len() - MAX_PANE_BYTES);
            pane.text = crate::utf8_tail(&pane.text, keep);
            encoded = serde_json::to_vec(&pane)?;
        }
        if encoded.len() <= MAX_PANE_BYTES && used + encoded.len() < MAX_TOTAL_BYTES {
            used += encoded.len() + 1;
            history.panes.push(pane);
        }
    }
    if !valid(&history) {
        return Ok(());
    }
    let mut bytes = serde_json::to_vec(&history).context("serialize pane history")?;
    bytes.push(b'\n');
    crate::persist::write_private_file(path, &bytes)
}

fn valid(history: &HistoryFile) -> bool {
    let expected = history_pane_ids(&history.session);
    let mut seen = std::collections::HashSet::new();
    if history.panes.iter().any(|pane| {
        pane.text.len() > MAX_PANE_TEXT
            || pane
                .text
                .chars()
                .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\t'))
            || !expected.contains(&pane.pane)
            || !seen.insert(pane.pane)
            || !valid_screen(&pane.screen)
            || serde_json::to_vec(pane).map_or(true, |bytes| bytes.len() > MAX_PANE_BYTES)
    }) {
        return false;
    }
    serde_json::to_vec(history).is_ok_and(|bytes| bytes.len() <= MAX_TOTAL_BYTES)
}

fn valid_screen(screen: &Screen) -> bool {
    dimensions(screen).is_some()
}

/// Saved frames include trailing blank cells, so their row widths recover the
/// original terminal size before a newly attached client resizes the PTY.
pub fn dimensions(screen: &Screen) -> Option<(u16, u16)> {
    let mut cols = usize::from(screen.cursor_col) + 1;
    let rows = screen.rows.len().max(usize::from(screen.cursor_row) + 1);
    for row in &screen.rows {
        if row.len() > MAX_DIMENSION || row.iter().any(|run| run.text.chars().any(char::is_control))
        {
            return None;
        }
        cols = cols.max(row.iter().map(|run| run.text.width()).sum());
    }
    (rows <= MAX_DIMENSION && cols <= MAX_DIMENSION).then_some((cols as u16, rows as u16))
}

fn history_pane_ids(session: &SessionFile) -> std::collections::HashSet<u64> {
    session
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.tabs)
        .flat_map(|tab| &tab.panes)
        .map(|pane| pane.id)
        .collect()
}

/// Open the target once, reject non-regular inputs, then cap the actual bytes
/// read. Metadata before a fresh open is not a safe size or type authority.
fn read_unchecked(path: &Path) -> Result<Option<HistoryFile>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("open pane history"),
    };
    if !file
        .metadata()
        .context("inspect pane history")?
        .file_type()
        .is_file()
    {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    file.take(MAX_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("read pane history")?;
    if bytes.len() as u64 > MAX_INPUT_BYTES {
        return Ok(None);
    }
    Ok(serde_json::from_slice(&bytes).ok())
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
                        native_session: None,
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

    #[test]
    fn long_unicode_history_keeps_recent_text_and_the_active_screen() {
        let dir = std::env::temp_dir().join(format!("kodade-history-tail-{}", std::process::id()));
        let path = dir.join("history.json");
        let mut entry = pane();
        entry.text = format!("{}\nnewest marker", "界\"".repeat(MAX_PANE_TEXT));
        entry.screen.contents = "active marker".into();
        write(&path, session(), vec![entry]).unwrap();
        let restored = read(&path, &session()).unwrap().unwrap();
        assert_eq!(restored.len(), 1);
        assert!(restored[0].text.ends_with("newest marker"));
        assert_eq!(restored[0].screen.contents, "active marker");
        assert!(serde_json::to_vec(&restored[0]).unwrap().len() <= MAX_PANE_BYTES);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn screen_bounds_count_display_cells_and_reject_terminal_controls() {
        use kodade_cli_proto::{CellColor, Run};
        let mut screen = Screen {
            rows: vec![vec![Run {
                text: "界".repeat(257),
                fg: CellColor::Default,
                bg: CellColor::Default,
                attrs: 0,
            }]],
            ..Default::default()
        };
        assert!(dimensions(&screen).is_none());
        screen.rows[0][0].text = "\x1b]52;c;payload\x07".into();
        assert!(dimensions(&screen).is_none());
        screen.rows[0][0].text = "界".repeat(60);
        assert_eq!(dimensions(&screen), Some((120, 1)));
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
