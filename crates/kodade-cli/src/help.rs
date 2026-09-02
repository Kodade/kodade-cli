//! The binding table rendered four ways.
//!
//! `help_table(config)` is the single source of truth for the help overlay
//! (`prefix ?`), the status-bar prefix hint, the `kodade-cli keys` command, and
//! the `docs/CONFIG.md` key table. Everything here is pure so it can be
//! unit-tested without a terminal.

use std::fmt::Write as _;

use crate::{
    config::{Action, Config},
    overlay::{Overlay, OverlayRow, OverlayTarget},
};

/// A named section of the binding table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Panes,
    Tabs,
    Workspaces,
    Modes,
    Other,
}

impl Group {
    /// Display order and membership; every action lands in exactly one group.
    pub const ORDER: &'static [Group] = &[
        Group::Panes,
        Group::Tabs,
        Group::Workspaces,
        Group::Modes,
        Group::Other,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Group::Panes => "panes",
            Group::Tabs => "tabs",
            Group::Workspaces => "workspaces",
            Group::Modes => "modes",
            Group::Other => "other",
        }
    }
}

/// One action row: its config name, a human label, the chords bound to it now,
/// and whether any of those chords fire without the prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpRow {
    pub action: &'static str,
    pub label: String,
    pub chords: Vec<String>,
    pub global: bool,
}

/// A group with its rows, in display order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpGroup {
    pub group: Group,
    pub rows: Vec<HelpRow>,
}

/// The whole binding table for `config`, grouped and ordered. Single source for
/// the overlay, the status hint, the `keys` command, and the docs table.
pub fn help_table(config: &Config) -> Vec<HelpGroup> {
    Group::ORDER
        .iter()
        .map(|&group| {
            let rows = Config::actions()
                .iter()
                .filter(|(_, action)| group_of(*action) == group)
                .map(|(name, action)| {
                    let chords = config.chords_for(*action);
                    let global = chords.iter().any(|chord| is_global(chord));
                    HelpRow {
                        action: name,
                        label: label_of(*action),
                        chords,
                        global,
                    }
                })
                .collect();
            HelpGroup { group, rows }
        })
        .collect()
}

/// A chord rendered by `chords_for` fires on its own when it carries a modifier
/// and is not written `prefix+…`. Prefixed chords are either bare (`%`) or
/// carry the `prefix+` marker, so this never mistakes one for a global chord.
fn is_global(chord: &str) -> bool {
    !chord.starts_with("prefix+") && (chord.contains("ctrl+") || chord.contains("alt+"))
}

/// Chords joined for display; global chords are marked so the prefix hint and
/// the overlay make clear they fire on their own.
fn joined(row: &HelpRow) -> String {
    if row.chords.is_empty() {
        return "unbound".to_string();
    }
    row.chords
        .iter()
        .map(|chord| {
            if is_global(chord) {
                format!("{chord} (global)")
            } else {
                chord.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

// --- overlay ---------------------------------------------------------------

/// The help overlay plus the full row set, so filtering can rebuild the view
/// without re-reading the config.
pub struct HelpOverlay {
    /// Every row including group headers, shown when the filter is empty.
    full: Vec<OverlayRow>,
    /// Action rows only, paired with a lowercase search key.
    entries: Vec<(String, OverlayRow)>,
    pub overlay: Overlay,
}

impl HelpOverlay {
    /// Rebuilds the visible rows from the current filter text and keeps the
    /// selection in range.
    pub fn apply_filter(&mut self) {
        let query = self.overlay.filter.clone().unwrap_or_default();
        self.overlay.rows = if query.is_empty() {
            self.full.clone()
        } else {
            let needle = query.to_lowercase();
            self.entries
                .iter()
                .filter(|(key, _)| key.contains(&needle))
                .map(|(_, row)| row.clone())
                .collect()
        };
        let last = self.overlay.rows.len().saturating_sub(1);
        if self.overlay.selected > last {
            self.overlay.selected = last;
        }
    }
}

/// Builds the help overlay for `config`: a filter line, one dim header per
/// group, and an action row per binding.
pub fn overlay(config: &Config) -> HelpOverlay {
    let mut full = Vec::new();
    let mut entries = Vec::new();
    for group in help_table(config) {
        if group.rows.is_empty() {
            continue;
        }
        full.push(OverlayRow::new(
            format!("— {} —", group.group.label()),
            String::new(),
            OverlayTarget::None,
        ));
        for row in &group.rows {
            let display = OverlayRow::new(
                format!(" {}", row.label),
                format!("{} ", joined(row)),
                OverlayTarget::None,
            );
            let key = format!("{} {} {}", row.action, row.label, joined(row)).to_lowercase();
            full.push(display.clone());
            entries.push((key, display));
        }
    }
    let mut overlay = Overlay::new("help · type to filter · esc closes", full.clone());
    overlay.filter = Some(String::new());
    HelpOverlay {
        full,
        entries,
        overlay,
    }
}

// --- status hint -----------------------------------------------------------

/// Actions the prefix hint advertises, in the order they appear. The hint shows
/// the first chord of each, up to twelve, so the status bar stays one line.
const HINT_ACTIONS: &[Action] = &[
    Action::SplitRight,
    Action::SplitDown,
    Action::NewTab,
    Action::ClosePane,
    Action::Zoom,
    Action::Navigate,
    Action::NextPane,
    Action::WorkspacePicker,
    Action::NewWorkspace,
    Action::Rename,
    Action::Detach,
    Action::Settings,
    Action::Help,
];

/// The prefix hint chords, generated from the live bindings (`% " c x z …`).
pub fn prefix_hint(config: &Config) -> String {
    HINT_ACTIONS
        .iter()
        .filter_map(|action| {
            config
                .chords_for(*action)
                .into_iter()
                .next()
                .map(|chord| chord.trim_start_matches("prefix+").to_string())
        })
        .take(12)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The first-attach nudge (`ctrl+b ? for help`), rendered from the actual
/// prefix and the current help chord so a remap shows through.
pub fn attach_hint(config: &Config) -> String {
    let prefix = crate::config::render_chord(config.prefix);
    let help_chord = config
        .chords_for(Action::Help)
        .into_iter()
        .next()
        .map(|chord| chord.trim_start_matches("prefix+").to_string())
        .unwrap_or_else(|| "?".to_string());
    format!("{prefix} {help_chord} for help")
}

// --- keys command ----------------------------------------------------------

/// The binding table as aligned text for `kodade-cli keys`.
pub fn keys_text(config: &Config) -> String {
    let table = help_table(config);
    let width = table
        .iter()
        .flat_map(|group| group.rows.iter())
        .map(|row| row.label.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for group in table {
        if group.rows.is_empty() {
            continue;
        }
        let _ = writeln!(out, "{}", group.group.label().to_uppercase());
        for row in group.rows {
            let _ = writeln!(out, "  {:<width$}  {}", row.label, joined(&row));
        }
        out.push('\n');
    }
    // Drop the trailing blank line.
    while out.ends_with('\n') {
        out.pop();
    }
    out.push('\n');
    out
}

/// The binding table as a JSON array for `kodade-cli keys --json`.
pub fn keys_json(config: &Config) -> String {
    let rows = help_table(config)
        .into_iter()
        .flat_map(|group| {
            let name = group.group.label();
            group.rows.into_iter().map(move |row| KeyEntry {
                group: name,
                action: row.action,
                label: row.label,
                chords: row.chords,
                global: row.global,
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_string_pretty(&rows).expect("key entries serialize")
}

#[derive(serde::Serialize)]
struct KeyEntry {
    group: &'static str,
    action: &'static str,
    label: String,
    chords: Vec<String>,
    global: bool,
}

// --- docs table ------------------------------------------------------------

/// The Markdown key table embedded in `docs/CONFIG.md` between the
/// `<!-- keys:start -->` / `<!-- keys:end -->` markers. Generated from
/// `help_table` so the docs never drift from the code; a test asserts the two
/// stay in sync.
#[cfg(test)]
pub fn markdown_table(config: &Config) -> String {
    let mut out = String::from("| Action name | Default |\n|---|---|\n");
    for group in help_table(config) {
        for row in group.rows {
            let chords = if row.chords.is_empty() {
                "unbound".to_string()
            } else {
                row.chords
                    .iter()
                    .map(|chord| format!("`{chord}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let _ = writeln!(out, "| `{}` | {} |", row.action, chords);
        }
    }
    out
}

// --- first-attach state ----------------------------------------------------

/// Whether the help overlay has been opened before, per the state file. A
/// missing or invalid file reads as "not seen" so the hint shows once.
pub fn state_seen() -> bool {
    seen_at(&crate::config::state_path())
}

fn seen_at(path: &std::path::Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|source| source.parse::<toml::Table>().ok())
        .and_then(|table| table.get("help_seen").and_then(toml::Value::as_bool))
        .unwrap_or(false)
}

/// Records that help has been opened. Best-effort: a write failure is ignored,
/// so the worst case is the hint showing again next time.
pub fn mark_seen() {
    write_seen(&crate::config::state_path());
}

fn write_seen(path: &std::path::Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Merge into the existing state file so sidebar collapse state (#19) and any
    // other keys survive; only `help_seen` is touched here.
    let mut table = std::fs::read_to_string(path)
        .ok()
        .and_then(|source| source.parse::<toml::Table>().ok())
        .unwrap_or_default();
    table.insert("help_seen".into(), toml::Value::Boolean(true));
    if let Ok(text) = toml::to_string(&table) {
        let _ = std::fs::write(path, text);
    }
}

// --- action metadata -------------------------------------------------------

// The group an action belongs to in the help table.
fn group_of(action: Action) -> Group {
    use Action::*;
    match action {
        SplitRight | SplitDown | ClosePane | Zoom | Rename | FocusUp | FocusDown | FocusLeft
        | FocusRight | ResizeUp | ResizeDown | ResizeLeft | ResizeRight | SwapUp | SwapDown
        | SwapLeft | SwapRight | NextPane | PrevPane | LastPane | BreakPane | LayoutEven
        | DisplayPanes => Group::Panes,
        NewTab | NextTab | PrevTab | SelectTabIndex(_) | CloseTab | RenameTab | MoveTabLeft
        | MoveTabRight => Group::Tabs,
        WorkspaceNext | WorkspacePrev | WorkspacePicker | NewWorkspace | WorktreeNew
        | RenameWorkspace | CloseWorkspace => Group::Workspaces,
        Navigate | Goto | CopyMode | ResizeMode | Settings | Help => Group::Modes,
        Detach | SidebarToggle | ReloadConfig | PasteBuffer | MouseToggle | NotificationJump => {
            Group::Other
        }
    }
}

// A human-readable label for an action, shown in the overlay and `keys`.
fn label_of(action: Action) -> String {
    use Action::*;
    match action {
        SplitRight => "split right".into(),
        SplitDown => "split down".into(),
        ClosePane => "close pane".into(),
        NewTab => "new tab".into(),
        NextTab => "next tab".into(),
        PrevTab => "previous tab".into(),
        Zoom => "zoom pane".into(),
        Detach => "detach".into(),
        Rename => "rename pane".into(),
        WorkspaceNext => "next workspace".into(),
        WorkspacePicker => "workspace picker".into(),
        Goto => "go to".into(),
        NewWorkspace => "new workspace".into(),
        WorktreeNew => "new worktree workspace".into(),
        SidebarToggle => "toggle sidebar".into(),
        FocusUp => "focus up".into(),
        FocusDown => "focus down".into(),
        FocusLeft => "focus left".into(),
        FocusRight => "focus right".into(),
        ResizeUp => "resize up".into(),
        ResizeDown => "resize down".into(),
        ResizeLeft => "resize left".into(),
        ResizeRight => "resize right".into(),
        Navigate => "navigate".into(),
        CopyMode => "copy mode".into(),
        SelectTabIndex(index) => format!("select tab {index}"),
        CloseTab => "close tab".into(),
        RenameTab => "rename tab".into(),
        RenameWorkspace => "rename workspace".into(),
        CloseWorkspace => "close workspace".into(),
        SwapUp => "swap up".into(),
        SwapDown => "swap down".into(),
        SwapLeft => "swap left".into(),
        SwapRight => "swap right".into(),
        MoveTabLeft => "move tab left".into(),
        MoveTabRight => "move tab right".into(),
        NextPane => "next pane".into(),
        PrevPane => "previous pane".into(),
        LastPane => "last pane".into(),
        WorkspacePrev => "previous workspace".into(),
        ResizeMode => "resize mode".into(),
        BreakPane => "break pane to tab".into(),
        LayoutEven => "even layout".into(),
        ReloadConfig => "reload config".into(),
        Settings => "settings".into(),
        Help => "help".into(),
        DisplayPanes => "show pane ids".into(),
        PasteBuffer => "paste buffer".into(),
        MouseToggle => "toggle mouse capture".into(),
        NotificationJump => "jump to notification".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn remapped() -> Config {
        // A config that overrides split_right to a global chord and remaps help.
        let toml = r#"
[keys]
split_right = ["ctrl+alt+v"]
help = ["F1"]
"#;
        Config::from_toml_str(toml)
    }

    #[test]
    fn table_reflects_remapped_chords_not_defaults() {
        let config = remapped();
        let split = help_table(&config)
            .into_iter()
            .flat_map(|group| group.rows)
            .find(|row| row.action == "split_right")
            .expect("split_right present");
        assert_eq!(split.chords, vec!["ctrl+alt+v".to_string()]);
        assert!(split.global);
        // The default `%` is gone.
        assert!(!split.chords.iter().any(|chord| chord == "%"));
    }

    #[test]
    fn attach_hint_tracks_the_prefix() {
        assert_eq!(attach_hint(&Config::default()), "ctrl+b ? for help");
        // Remapping the prefix changes the first-attach hint.
        let config = Config::from_toml_str("[keys]\nprefix = \"ctrl+space\"\n");
        assert_eq!(attach_hint(&config), "ctrl+space ? for help");
    }

    #[test]
    fn prefix_hint_tracks_the_bindings() {
        let default = prefix_hint(&Config::default());
        assert!(default.starts_with("% \" c x z"));
        // Remapping split_right to a global chord changes the hint.
        let hint = prefix_hint(&remapped());
        assert!(hint.starts_with("ctrl+alt+v"));
        // At most twelve chords.
        assert!(default.split(' ').count() <= 12);
    }

    #[test]
    fn overlay_has_a_header_per_group_plus_a_row_per_action() {
        let config = Config::default();
        let state = overlay(&config);
        let headers = Group::ORDER.len();
        let actions = Config::actions().len();
        assert_eq!(state.overlay.rows.len(), headers + actions);
        assert_eq!(state.entries.len(), actions);
        // Filtering to a needle drops the headers and keeps only matches.
        let mut state = state;
        state.overlay.filter = Some("workspace".into());
        state.apply_filter();
        assert!(state.overlay.rows.iter().all(|row| row
            .label
            .to_lowercase()
            .contains("workspace")
            || row.hint.to_lowercase().contains("workspace")));
        assert!(!state.overlay.rows.is_empty());
    }

    #[test]
    fn keys_text_groups_and_aligns() {
        let text = keys_text(&Config::default());
        assert!(text.starts_with("PANES\n"));
        assert!(text.contains("split right"));
        assert!(text.contains("MODES\n"));
        assert!(text.contains("help"));
    }

    #[test]
    fn keys_json_is_an_array_of_entries() {
        let json = keys_json(&Config::default());
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        let array = value.as_array().expect("array");
        assert_eq!(array.len(), Config::actions().len());
        assert!(array
            .iter()
            .any(|entry| entry["action"] == "split_right" && entry["group"] == "panes"));
    }

    #[test]
    fn state_file_round_trips_and_tolerates_junk() {
        let dir = std::env::temp_dir().join("kodade-cli-help-state");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("state");
        // Missing file reads as not seen.
        assert!(!seen_at(&path));
        write_seen(&path);
        assert!(seen_at(&path));
        // Invalid TOML also reads as not seen instead of erroring.
        std::fs::write(&path, "not = valid = toml").expect("write junk");
        assert!(!seen_at(&path));
    }

    #[test]
    fn docs_table_matches_the_embedded_table() {
        let docs = include_str!("../../../docs/CONFIG.md");
        let start = docs
            .find("<!-- keys:start -->")
            .expect("keys:start marker present");
        let end = docs
            .find("<!-- keys:end -->")
            .expect("keys:end marker present");
        let embedded = docs[start + "<!-- keys:start -->".len()..end].trim();
        let generated = markdown_table(&Config::default());
        assert_eq!(embedded, generated.trim());
    }
}
