use std::{collections::HashMap, fmt::Write as _, fs, io::Write, path::PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use kodade_cli_proto::{ClientMessage, Direction};
use ratatui::style::Color;
use serde::Deserialize;

const CONFIG_DIR: &str = ".config/kodade-cli";

#[derive(Debug, Clone)]
pub struct Config {
    pub theme: ThemeChoice,
    pub mouse: bool,
    pub sidebar: bool,
    /// `mouse.copy_on_select` — consumed by the drag-selection work (#12).
    pub copy_on_select: bool,
    /// `notify.enabled` — consumed by the notification work (#10).
    pub notify: bool,
    /// `paste.sanitize` — strip escape sequences and control bytes from pastes (#21).
    pub paste_sanitize: bool,
    pub prefix: KeyEvent,
    /// Right-side status bar widgets, in order (#11).
    pub status_right: Vec<StatusWidget>,
    /// Host terminal title template (#11). `{session}`/`{workspace}`/`{tab}`.
    pub window_title: String,
    /// Chords that fire after the prefix key.
    bindings: HashMap<KeyEvent, Action>,
    /// Chords that fire on their own (ctrl/alt chords without `prefix+`).
    globals: HashMap<KeyEvent, Action>,
    named_theme: Option<String>,
    /// Problems found while loading; printed by `load` and `config validate`.
    pub warnings: Vec<String>,
}

/// A right-side status bar widget (#11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusWidget {
    Zoom,
    Blocked,
    Hostname,
    Time,
}

impl StatusWidget {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "zoom" => Self::Zoom,
            "blocked" => Self::Blocked,
            "hostname" => Self::Hostname,
            "time" => Self::Time,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeChoice {
    Dark,
    Light,
    Auto,
    Named,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    SplitRight,
    SplitDown,
    ClosePane,
    NewTab,
    NextTab,
    PrevTab,
    Zoom,
    Detach,
    Rename,
    WorkspaceNext,
    NewWorkspace,
    SidebarToggle,
    FocusUp,
    FocusDown,
    FocusLeft,
    FocusRight,
    ResizeUp,
    ResizeDown,
    ResizeLeft,
    ResizeRight,
    Navigate,
    CopyMode,
    // Layout management (#14). The payload is the one-based tab position.
    SelectTabIndex(u8),
    CloseTab,
    RenameTab,
    RenameWorkspace,
    CloseWorkspace,
    SwapUp,
    SwapDown,
    SwapLeft,
    SwapRight,
    MoveTabLeft,
    MoveTabRight,
    NextPane,
    PrevPane,
    LastPane,
    WorkspacePrev,
    ResizeMode,
    BreakPane,
    LayoutEven,
    // Config (#20).
    ReloadConfig,
    Settings,
    /// Flash big pane ids for a second, tmux `display-panes` style (#11).
    DisplayPanes,
    // Paste (#21): re-paste the internal buffer.
    PasteBuffer,
}

/// Every remappable action and its config name. Single source of truth for
/// `Action::parse`, `config show`, the docs table, and the help overlay (#6).
const ACTIONS: &[(&str, Action)] = &[
    ("split_right", Action::SplitRight),
    ("split_down", Action::SplitDown),
    ("close_pane", Action::ClosePane),
    ("new_tab", Action::NewTab),
    ("next_tab", Action::NextTab),
    ("prev_tab", Action::PrevTab),
    ("zoom", Action::Zoom),
    ("detach", Action::Detach),
    ("rename", Action::Rename),
    ("workspace_next", Action::WorkspaceNext),
    ("new_workspace", Action::NewWorkspace),
    ("sidebar_toggle", Action::SidebarToggle),
    ("focus_up", Action::FocusUp),
    ("focus_down", Action::FocusDown),
    ("focus_left", Action::FocusLeft),
    ("focus_right", Action::FocusRight),
    ("resize_up", Action::ResizeUp),
    ("resize_down", Action::ResizeDown),
    ("resize_left", Action::ResizeLeft),
    ("resize_right", Action::ResizeRight),
    ("navigate", Action::Navigate),
    ("copy_mode", Action::CopyMode),
    ("select_tab_1", Action::SelectTabIndex(1)),
    ("select_tab_2", Action::SelectTabIndex(2)),
    ("select_tab_3", Action::SelectTabIndex(3)),
    ("select_tab_4", Action::SelectTabIndex(4)),
    ("select_tab_5", Action::SelectTabIndex(5)),
    ("select_tab_6", Action::SelectTabIndex(6)),
    ("select_tab_7", Action::SelectTabIndex(7)),
    ("select_tab_8", Action::SelectTabIndex(8)),
    ("select_tab_9", Action::SelectTabIndex(9)),
    ("close_tab", Action::CloseTab),
    ("rename_tab", Action::RenameTab),
    ("rename_workspace", Action::RenameWorkspace),
    ("close_workspace", Action::CloseWorkspace),
    ("swap_up", Action::SwapUp),
    ("swap_down", Action::SwapDown),
    ("swap_left", Action::SwapLeft),
    ("swap_right", Action::SwapRight),
    ("move_tab_left", Action::MoveTabLeft),
    ("move_tab_right", Action::MoveTabRight),
    ("next_pane", Action::NextPane),
    ("prev_pane", Action::PrevPane),
    ("last_pane", Action::LastPane),
    ("workspace_prev", Action::WorkspacePrev),
    ("resize_mode", Action::ResizeMode),
    ("break_pane", Action::BreakPane),
    ("layout_even", Action::LayoutEven),
    ("reload_config", Action::ReloadConfig),
    ("settings", Action::Settings),
    ("display_panes", Action::DisplayPanes),
    ("paste_buffer", Action::PasteBuffer),
];

impl Action {
    fn parse(name: &str) -> Option<Self> {
        ACTIONS
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .map(|(_, action)| *action)
    }

    /// Config name for this action, used in warnings and `config show`.
    pub fn name(self) -> &'static str {
        ACTIONS
            .iter()
            .find(|(_, candidate)| *candidate == self)
            .map(|(name, _)| *name)
            .unwrap_or("unknown")
    }

    pub fn message(self) -> Option<ClientMessage> {
        Some(match self {
            Self::SplitRight => ClientMessage::SplitRight,
            Self::SplitDown => ClientMessage::SplitDown,
            Self::ClosePane => ClientMessage::ClosePane,
            Self::NewTab => ClientMessage::NewTab,
            Self::NextTab => ClientMessage::NextTab,
            Self::PrevTab => ClientMessage::PrevTab,
            Self::Zoom => ClientMessage::ZoomPane,
            // `prefix W` opens a name/path prompt in the client instead of
            // firing this directly, but keep a sensible default for completeness.
            Self::NewWorkspace => ClientMessage::NewWorkspace {
                name: "workspace".into(),
                root: None,
            },
            Self::FocusUp => ClientMessage::FocusPane {
                direction: Direction::Up,
            },
            Self::FocusDown => ClientMessage::FocusPane {
                direction: Direction::Down,
            },
            Self::FocusLeft => ClientMessage::FocusPane {
                direction: Direction::Left,
            },
            Self::FocusRight => ClientMessage::FocusPane {
                direction: Direction::Right,
            },
            Self::ResizeUp => ClientMessage::ResizePane {
                direction: Direction::Up,
                cells: 2,
            },
            Self::ResizeDown => ClientMessage::ResizePane {
                direction: Direction::Down,
                cells: 2,
            },
            Self::ResizeLeft => ClientMessage::ResizePane {
                direction: Direction::Left,
                cells: 2,
            },
            Self::ResizeRight => ClientMessage::ResizePane {
                direction: Direction::Right,
                cells: 2,
            },
            Self::WorkspaceNext => ClientMessage::SelectWorkspaceDelta { delta: 1 },
            Self::WorkspacePrev => ClientMessage::SelectWorkspaceDelta { delta: -1 },
            Self::SelectTabIndex(index) => ClientMessage::SelectTabIndex { index },
            Self::SwapUp => ClientMessage::SwapPane {
                direction: Direction::Up,
            },
            Self::SwapDown => ClientMessage::SwapPane {
                direction: Direction::Down,
            },
            Self::SwapLeft => ClientMessage::SwapPane {
                direction: Direction::Left,
            },
            Self::SwapRight => ClientMessage::SwapPane {
                direction: Direction::Right,
            },
            Self::MoveTabLeft => ClientMessage::MoveTab { delta: -1 },
            Self::MoveTabRight => ClientMessage::MoveTab { delta: 1 },
            Self::NextPane => ClientMessage::FocusPaneCycle { forward: true },
            Self::PrevPane => ClientMessage::FocusPaneCycle { forward: false },
            Self::BreakPane => ClientMessage::BreakPane,
            Self::LayoutEven => ClientMessage::EqualizeLayout,
            Self::Detach | Self::Rename | Self::SidebarToggle => return None,
            // M3b reserves these names without introducing their modes early.
            Self::Navigate | Self::CopyMode => return None,
            // Handled in `App`: these need snapshot context or a prompt.
            Self::CloseTab
            | Self::CloseWorkspace
            | Self::RenameTab
            | Self::RenameWorkspace
            | Self::LastPane
            | Self::ResizeMode
            | Self::DisplayPanes => return None,
            // Client-side only: they never reach the daemon.
            Self::ReloadConfig | Self::Settings | Self::PasteBuffer => return None,
        })
    }
}

#[derive(Debug, Deserialize, Default)]
struct FileConfig {
    theme: Option<String>,
    mouse: Option<Section<MouseTable>>,
    sidebar: Option<bool>,
    notify: Option<Section<NotifyTable>>,
    paste: Option<Section<PasteTable>>,
    keys: Option<HashMap<String, Chords>>,
    status: Option<StatusFile>,
    ui: Option<UiFile>,
    /// Anything this version does not know: reported as a warning so typos
    /// like `sidbar = true` do not silently do nothing.
    #[serde(flatten)]
    extra: HashMap<String, toml::Value>,
}

#[derive(Debug, Deserialize, Default)]
struct StatusFile {
    right: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Default)]
struct UiFile {
    window_title: Option<String>,
}

/// A setting that accepts either a bare `key = true` boolean (the pre-0.2
/// shape) or a `[key]` table. Both stay valid forever.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Section<T> {
    Enabled(bool),
    Table(T),
}

#[derive(Debug, Deserialize, Default)]
struct MouseTable {
    enabled: Option<bool>,
    copy_on_select: Option<bool>,
    #[serde(flatten)]
    extra: HashMap<String, toml::Value>,
}

#[derive(Debug, Deserialize, Default)]
struct NotifyTable {
    enabled: Option<bool>,
    #[serde(flatten)]
    extra: HashMap<String, toml::Value>,
}

#[derive(Debug, Deserialize, Default)]
struct PasteTable {
    sanitize: Option<bool>,
    #[serde(flatten)]
    extra: HashMap<String, toml::Value>,
}

/// A binding value: one chord or a list of chords.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Chords {
    One(String),
    Many(Vec<String>),
}

// Test configs are written as plain strings.
#[cfg(test)]
impl From<&str> for Chords {
    fn from(value: &str) -> Self {
        Self::One(value.to_string())
    }
}

impl Chords {
    fn list(&self) -> Vec<&str> {
        match self {
            Self::One(chord) => vec![chord.as_str()],
            Self::Many(chords) => chords.iter().map(String::as_str).collect(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        let mut bindings = HashMap::new();
        for (binding, action) in [
            ("%", Action::SplitRight),
            ("\"", Action::SplitDown),
            ("x", Action::ClosePane),
            ("c", Action::NewTab),
            ("n", Action::Navigate),
            ("tab", Action::NextTab),
            ("p", Action::PrevTab),
            ("z", Action::Zoom),
            ("d", Action::Detach),
            ("r", Action::Rename),
            ("w", Action::WorkspaceNext),
            ("W", Action::NewWorkspace),
            ("b", Action::SidebarToggle),
            ("[", Action::CopyMode),
            // #14 took `R` for rename_workspace, so reload is prefix+ctrl+r.
            ("prefix+ctrl+r", Action::ReloadConfig),
            ("s", Action::Settings),
            ("up", Action::FocusUp),
            ("down", Action::FocusDown),
            ("left", Action::FocusLeft),
            ("right", Action::FocusRight),
            ("k", Action::FocusUp),
            ("j", Action::FocusDown),
            ("h", Action::FocusLeft),
            ("l", Action::FocusRight),
            ("K", Action::ResizeUp),
            ("J", Action::ResizeDown),
            ("H", Action::ResizeLeft),
            ("L", Action::ResizeRight),
            // Layout management (#14). move_tab_* and workspace_prev ship unbound.
            ("X", Action::CloseTab),
            ("T", Action::RenameTab),
            ("R", Action::RenameWorkspace),
            ("D", Action::CloseWorkspace),
            ("alt+k", Action::SwapUp),
            ("alt+j", Action::SwapDown),
            ("alt+h", Action::SwapLeft),
            ("alt+l", Action::SwapRight),
            ("o", Action::NextPane),
            ("O", Action::PrevPane),
            (";", Action::LastPane),
            ("alt+r", Action::ResizeMode),
            ("!", Action::BreakPane),
            ("=", Action::LayoutEven),
            ("q", Action::DisplayPanes),
            ("]", Action::PasteBuffer),
        ] {
            bindings.insert(
                parse_key_chord(binding).expect("built-in key is valid"),
                action,
            );
        }
        // Digits 1–9 jump straight to that tab position.
        for index in 1..=9_u8 {
            bindings.insert(
                parse_key_chord(&index.to_string()).expect("digit key is valid"),
                Action::SelectTabIndex(index),
            );
        }
        Self {
            theme: ThemeChoice::Auto,
            mouse: true,
            sidebar: true,
            copy_on_select: true,
            notify: true,
            paste_sanitize: true,
            prefix: parse_key_chord("ctrl+b").expect("built-in prefix is valid"),
            status_right: vec![StatusWidget::Zoom, StatusWidget::Blocked],
            window_title: "Ködade · {workspace} · {tab}".into(),
            bindings,
            // Defaults are all prefixed; global chords are opt-in per config.
            globals: HashMap::new(),
            named_theme: None,
            warnings: Vec::new(),
        }
    }
}

impl Config {
    /// Loads the config, printing any warnings. Never fails: a broken file
    /// degrades to the defaults.
    pub fn load() -> Self {
        let config = match Self::load_checked() {
            Ok(config) => config,
            Err(error) => {
                eprintln!("kodade-cli: warning: {error}, using defaults");
                Self::default()
            }
        };
        for warning in &config.warnings {
            eprintln!("kodade-cli: warning: {warning}");
        }
        config
    }

    /// Loads the config, reporting a missing-or-invalid TOML file as an error
    /// so live reload (`prefix R`) can keep the previous config.
    pub fn load_checked() -> Result<Self, String> {
        let path = config_path();
        let Ok(source) = fs::read_to_string(&path) else {
            return Ok(Self::default());
        };
        let file = toml::from_str::<FileConfig>(&source)
            .map_err(|error| format!("invalid config {}: {error}", path.display()))?;
        Ok(Self::from_file(file))
    }

    fn from_file(file: FileConfig) -> Self {
        let mut config = Self::default();
        if let Some(theme) = file.theme {
            config.set_theme(&theme);
        }
        config.warn_unknown("", &file.extra);
        match file.mouse {
            Some(Section::Enabled(enabled)) => config.mouse = enabled,
            Some(Section::Table(table)) => {
                config.mouse = table.enabled.unwrap_or(config.mouse);
                config.copy_on_select = table.copy_on_select.unwrap_or(config.copy_on_select);
                config.warn_unknown("mouse.", &table.extra);
            }
            None => {}
        }
        match file.notify {
            Some(Section::Enabled(enabled)) => config.notify = enabled,
            Some(Section::Table(table)) => {
                config.notify = table.enabled.unwrap_or(config.notify);
                config.warn_unknown("notify.", &table.extra);
            }
            None => {}
        }
        match file.paste {
            Some(Section::Enabled(enabled)) => config.paste_sanitize = enabled,
            Some(Section::Table(table)) => {
                config.paste_sanitize = table.sanitize.unwrap_or(config.paste_sanitize);
                config.warn_unknown("paste.", &table.extra);
            }
            None => {}
        }
        config.sidebar = file.sidebar.unwrap_or(config.sidebar);
        if let Some(right) = file.status.and_then(|status| status.right) {
            // Unknown widget names warn and are skipped, keeping the rest.
            let mut widgets = Vec::new();
            for name in &right {
                match StatusWidget::parse(name) {
                    Some(widget) => widgets.push(widget),
                    None => config
                        .warnings
                        .push(format!("unknown status widget {name}")),
                }
            }
            config.status_right = widgets;
        }
        if let Some(title) = file.ui.and_then(|ui| ui.window_title) {
            config.window_title = title;
        }
        if let Some(keys) = file.keys {
            // Sorted so warnings and overrides are deterministic.
            let mut entries = keys.iter().collect::<Vec<_>>();
            entries.sort_by_key(|(left, _)| *left);
            for (name, chords) in entries {
                config.apply_binding(name, chords);
            }
        }
        config
    }

    // Applies one `[keys]` entry: `prefix` or an action with one or more chords.
    fn apply_binding(&mut self, name: &str, chords: &Chords) {
        if name == "prefix" {
            match parse_binding(chords.list().first().copied().unwrap_or("")) {
                Ok(binding) => self.prefix = binding.key,
                Err(error) => self.warnings.push(format!("invalid key prefix: {error}")),
            }
            return;
        }
        let Some(action) = Action::parse(name) else {
            self.warnings.push(format!("unknown key action {name}"));
            return;
        };
        let listed = chords.list();
        // An empty array unbinds the action.
        if listed.is_empty() {
            self.bindings.retain(|_, mapped| *mapped != action);
            self.globals.retain(|_, mapped| *mapped != action);
            return;
        }
        let parsed = listed
            .into_iter()
            .filter_map(|chord| match parse_binding(chord) {
                Ok(binding) => Some(binding),
                Err(error) => {
                    self.warnings.push(format!("invalid key {name}: {error}"));
                    None
                }
            })
            .collect::<Vec<_>>();
        // Every chord was rejected: keep the defaults rather than unbind.
        if parsed.is_empty() {
            return;
        }
        // An override replaces the action's defaults, aliases included.
        self.bindings.retain(|_, mapped| *mapped != action);
        self.globals.retain(|_, mapped| *mapped != action);
        for binding in parsed {
            let map = if binding.global {
                &mut self.globals
            } else {
                &mut self.bindings
            };
            // Taking a chord away from another action is easy to do by accident.
            if let Some(displaced) = map.insert(binding.key, action) {
                if displaced != action {
                    let chord = render_chord(binding.key);
                    self.warnings.push(format!(
                        "key {chord} for {name} replaces {}",
                        displaced.name()
                    ));
                }
            }
        }
    }

    // Reports every setting this version does not recognize.
    fn warn_unknown(&mut self, prefix: &str, extra: &HashMap<String, toml::Value>) {
        let mut names = extra.keys().cloned().collect::<Vec<_>>();
        names.sort();
        for name in names {
            self.warnings
                .push(format!("unknown setting {prefix}{name}"));
        }
    }

    /// Applies a theme name (`auto`, an alias, a built-in, or a user theme).
    pub fn set_theme(&mut self, name: &str) {
        self.theme = match name {
            "dark" => ThemeChoice::Dark,
            "light" => ThemeChoice::Light,
            "auto" => ThemeChoice::Auto,
            _ => ThemeChoice::Named,
        };
        self.named_theme = Some(name.to_string());
    }

    /// The configured theme name, as it appears in config.toml.
    pub fn theme_name(&self) -> &str {
        self.named_theme.as_deref().unwrap_or("auto")
    }

    /// Every remappable action and its config name.
    pub fn actions() -> &'static [(&'static str, Action)] {
        ACTIONS
    }

    /// Chords bound to an action, rendered back to config text. Prefixed
    /// chords render bare (`%`) unless they carry a modifier (`prefix+ctrl+x`);
    /// global chords render as themselves (`ctrl+alt+v`).
    pub fn chords_for(&self, action: Action) -> Vec<String> {
        let mut chords = self
            .bindings
            .iter()
            .filter(|(_, mapped)| **mapped == action)
            .map(|(key, _)| {
                let text = render_chord(*key);
                if key.modifiers.is_empty() {
                    text
                } else {
                    format!("prefix+{text}")
                }
            })
            .chain(
                self.globals
                    .iter()
                    .filter(|(_, mapped)| **mapped == action)
                    .map(|(key, _)| render_chord(*key)),
            )
            .collect::<Vec<_>>();
        chords.sort();
        chords
    }

    /// The effective config as TOML, for `kodade-cli config show`.
    pub fn to_toml(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "theme = {}", toml_string(self.theme_name()));
        let _ = writeln!(out, "sidebar = {}", self.sidebar);
        let _ = writeln!(out, "\n[mouse]");
        let _ = writeln!(out, "enabled = {}", self.mouse);
        let _ = writeln!(out, "copy_on_select = {}", self.copy_on_select);
        let _ = writeln!(out, "\n[notify]");
        let _ = writeln!(out, "enabled = {}", self.notify);
        let _ = writeln!(out, "\n[paste]");
        let _ = writeln!(out, "sanitize = {}", self.paste_sanitize);
        let _ = writeln!(out, "\n[keys]");
        let _ = writeln!(out, "prefix = {}", toml_string(&render_chord(self.prefix)));
        for (name, action) in Self::actions() {
            let chords = self
                .chords_for(*action)
                .into_iter()
                .map(|chord| toml_string(&chord))
                .collect::<Vec<_>>();
            let _ = writeln!(out, "{name} = [{}]", chords.join(", "));
        }
        out
    }

    /// Action bound to a key pressed after the prefix.
    pub fn action(&self, key: KeyEvent) -> Option<Action> {
        self.bindings.get(&normalize_key(key)).copied()
    }

    /// Action bound to an unprefixed global chord.
    pub fn global_action(&self, key: KeyEvent) -> Option<Action> {
        self.globals.get(&normalize_key(key)).copied()
    }

    pub fn resolve_theme(&self) -> Theme {
        match self.theme {
            // `dark` / `light` alias to the Ködade built-ins.
            ThemeChoice::Dark => Theme::kodade_dark(),
            ThemeChoice::Light => Theme::kodade_light(),
            // OSC 11 (terminal_background) runs only on `auto` — see #24. Every
            // other arm resolves a fixed theme and never queries the terminal.
            ThemeChoice::Auto => terminal_background()
                .as_deref()
                .map(Theme::from_background)
                .unwrap_or_else(Theme::kodade_dark),
            ThemeChoice::Named => self
                .named_theme
                .as_deref()
                // Built-in names resolve before the user themes directory.
                .and_then(|name| builtin_theme(name).or_else(|| load_user_theme(name)))
                .unwrap_or_else(|| {
                    eprintln!("kodade-cli: warning: theme not found or invalid, using kodade-dark");
                    Theme::kodade_dark()
                }),
        }
    }
}

/// Resolve one of the built-in theme names, ahead of the user themes dir.
fn builtin_theme(name: &str) -> Option<Theme> {
    match name {
        "kodade-dark" => Some(Theme::kodade_dark()),
        "kodade-light" => Some(Theme::kodade_light()),
        "tokyo-night" => Some(Theme::tokyo_night()),
        _ => None,
    }
}

/// A parsed binding: the chord plus whether it fires without the prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub key: KeyEvent,
    pub global: bool,
}

/// Parses `[prefix+][ctrl+][alt+][shift+]key` in any modifier order. A chord
/// with ctrl or alt and no explicit `prefix+` is global.
pub fn parse_binding(value: &str) -> Result<Binding, String> {
    let whole = value.trim();
    let mut rest = whole;
    let mut modifiers = KeyModifiers::NONE;
    let mut prefixed = false;
    loop {
        let lower = rest.to_ascii_lowercase();
        let eaten = if lower.starts_with("ctrl+") {
            modifiers |= KeyModifiers::CONTROL;
            5
        } else if lower.starts_with("alt+") {
            modifiers |= KeyModifiers::ALT;
            4
        } else if lower.starts_with("shift+") {
            modifiers |= KeyModifiers::SHIFT;
            6
        } else if lower.starts_with("prefix+") {
            prefixed = true;
            7
        } else {
            break;
        };
        rest = &rest[eaten..];
    }
    if rest.is_empty() {
        return Err(format!("unsupported key chord {whole}"));
    }
    let key = normalize_key(KeyEvent::new(key_code(rest, whole)?, modifiers));
    let global = !prefixed
        && key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
    Ok(Binding { key, global })
}

/// Parses a chord that is always prefix-relative (the prefix key itself, and
/// the built-in defaults).
pub fn parse_key_chord(value: &str) -> Result<KeyEvent, String> {
    parse_binding(value).map(|binding| binding.key)
}

// Named keys, F1-F12, or a single character.
fn key_code(key: &str, whole: &str) -> Result<KeyCode, String> {
    let lower = key.to_ascii_lowercase();
    Ok(match lower.as_str() {
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "tab" => KeyCode::Tab,
        "enter" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        "backspace" => KeyCode::Backspace,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "delete" | "del" => KeyCode::Delete,
        _ => {
            if let Some(digits) = lower.strip_prefix('f') {
                if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                    return match digits.parse::<u8>() {
                        Ok(number @ 1..=12) => Ok(KeyCode::F(number)),
                        _ => Err(format!("unsupported key chord {whole}")),
                    };
                }
            }
            match key.chars().count() {
                1 => KeyCode::Char(key.chars().next().expect("one char")),
                _ => return Err(format!("unsupported key chord {whole}")),
            }
        }
    })
}

/// `shift+x` and `X` are the same binding: letters normalize to the uppercase
/// character without SHIFT, so `l` and `L` stay distinct. Non-letters keep the
/// SHIFT modifier because their unshifted character is a different key.
pub fn normalize_key(key: KeyEvent) -> KeyEvent {
    match key.code {
        KeyCode::Char(c) if c.is_alphabetic() && key.modifiers.contains(KeyModifiers::SHIFT) => {
            let upper = c.to_uppercase().next().unwrap_or(c);
            KeyEvent::new(KeyCode::Char(upper), key.modifiers - KeyModifiers::SHIFT)
        }
        _ => key,
    }
}

// Quotes a value for `config show`; chords include `"` and `\\`.
fn toml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Renders a chord back to config text (`ctrl+alt+v`, `F5`, `%`).
pub fn render_chord(key: KeyEvent) -> String {
    let mut text = String::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        text.push_str("ctrl+");
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        text.push_str("alt+");
    }
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        text.push_str("shift+");
    }
    match key.code {
        KeyCode::Char(' ') => text.push_str("space"),
        KeyCode::Char(c) => text.push(c),
        KeyCode::Up => text.push_str("up"),
        KeyCode::Down => text.push_str("down"),
        KeyCode::Left => text.push_str("left"),
        KeyCode::Right => text.push_str("right"),
        KeyCode::Tab => text.push_str("tab"),
        KeyCode::Enter => text.push_str("enter"),
        KeyCode::Esc => text.push_str("esc"),
        KeyCode::Backspace => text.push_str("backspace"),
        KeyCode::Home => text.push_str("home"),
        KeyCode::End => text.push_str("end"),
        KeyCode::PageUp => text.push_str("pageup"),
        KeyCode::PageDown => text.push_str("pagedown"),
        KeyCode::Delete => text.push_str("delete"),
        KeyCode::F(number) => {
            let _ = write!(text, "F{number}");
        }
        other => {
            let _ = write!(text, "{other:?}");
        }
    }
    text
}

#[derive(Debug, Clone, Deserialize)]
struct ThemeFile {
    name: String,
    accent: String,
    border: String,
    text: String,
    dim: String,
    blocked: String,
    working: String,
    done: String,
    idle: String,
    tabbar_bg: String,
    status_bg: String,
    // Expanded schema (#8). All optional so 10-field user themes keep loading;
    // each falls back to an existing field (see `parse`).
    bg: Option<String>,
    surface: Option<String>,
    selection: Option<String>,
    cursor: Option<String>,
    menu_bg: Option<String>,
    menu_fg: Option<String>,
    tab_active_fg: Option<String>,
    tab_active_bg: Option<String>,
    sidebar_bg: Option<String>,
    ansi: Option<AnsiFile>,
}

/// The 16-entry `[ansi]` palette table (#8). Missing table or keys fall back to
/// the standard xterm colors.
#[derive(Debug, Clone, Deserialize, Default)]
struct AnsiFile {
    black: Option<String>,
    red: Option<String>,
    green: Option<String>,
    yellow: Option<String>,
    blue: Option<String>,
    magenta: Option<String>,
    cyan: Option<String>,
    white: Option<String>,
    bright_black: Option<String>,
    bright_red: Option<String>,
    bright_green: Option<String>,
    bright_yellow: Option<String>,
    bright_blue: Option<String>,
    bright_magenta: Option<String>,
    bright_cyan: Option<String>,
    bright_white: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Theme {
    /// Kept for diagnostics and future theme listings.
    #[allow(dead_code)]
    pub name: String,
    pub accent: Color,
    pub border: Color,
    pub text: Color,
    pub dim: Color,
    pub blocked: Color,
    pub working: Color,
    pub done: Color,
    pub idle: Color,
    pub tabbar_bg: Color,
    pub status_bg: Color,
    // Expanded schema (#8).
    pub bg: Color,
    /// Consumed by the colored-pane renderer (#7) / selection (#12); parsed now.
    #[allow(dead_code)]
    pub surface: Color,
    #[allow(dead_code)]
    pub selection: Color,
    #[allow(dead_code)]
    pub cursor: Color,
    pub menu_bg: Color,
    pub menu_fg: Color,
    pub tab_active_fg: Color,
    pub tab_active_bg: Color,
    pub sidebar_bg: Color,
    /// Indexed ANSI palette, order: black..white then bright_black..bright_white.
    /// Consumed by the colored-pane renderer (#7); parsed now so themes carry it.
    #[allow(dead_code)]
    pub ansi: [Color; 16],
}

impl Theme {
    pub(crate) fn kodade_dark() -> Self {
        Self::parse(include_str!("../themes/kodade-dark.toml"))
            .expect("built-in kodade-dark theme is valid")
    }

    fn kodade_light() -> Self {
        Self::parse(include_str!("../themes/kodade-light.toml"))
            .expect("built-in kodade-light theme is valid")
    }

    fn tokyo_night() -> Self {
        Self::parse(include_str!("../themes/tokyo-night.toml"))
            .expect("built-in tokyo-night theme is valid")
    }

    fn parse(source: &str) -> Result<Self, String> {
        let raw = toml::from_str::<ThemeFile>(source).map_err(|error| error.to_string())?;
        let accent = parse_hex_color(&raw.accent)?;
        let border = parse_hex_color(&raw.border)?;
        let text = parse_hex_color(&raw.text)?;
        let tabbar_bg = parse_hex_color(&raw.tabbar_bg)?;
        let status_bg = parse_hex_color(&raw.status_bg)?;
        // Optional field → parse if present, else the given fallback.
        let opt = |value: &Option<String>, fallback: Color| -> Result<Color, String> {
            match value {
                Some(hex) => parse_hex_color(hex),
                None => Ok(fallback),
            }
        };
        Ok(Self {
            name: raw.name,
            accent,
            border,
            text,
            dim: parse_hex_color(&raw.dim)?,
            blocked: parse_hex_color(&raw.blocked)?,
            working: parse_hex_color(&raw.working)?,
            done: parse_hex_color(&raw.done)?,
            idle: parse_hex_color(&raw.idle)?,
            tabbar_bg,
            status_bg,
            bg: opt(&raw.bg, tabbar_bg)?,
            surface: opt(&raw.surface, status_bg)?,
            selection: opt(&raw.selection, border)?,
            cursor: opt(&raw.cursor, accent)?,
            menu_bg: opt(&raw.menu_bg, status_bg)?,
            menu_fg: opt(&raw.menu_fg, text)?,
            tab_active_fg: opt(&raw.tab_active_fg, accent)?,
            tab_active_bg: opt(&raw.tab_active_bg, tabbar_bg)?,
            sidebar_bg: opt(&raw.sidebar_bg, tabbar_bg)?,
            ansi: parse_ansi(raw.ansi.as_ref())?,
        })
    }

    fn from_background(background: &str) -> Self {
        let Ok(Color::Rgb(red, green, blue)) = parse_rgb_color(background) else {
            return Self::kodade_dark();
        };
        if u16::from(red) + u16::from(green) + u16::from(blue) > 382 {
            Self::kodade_light()
        } else {
            Self::kodade_dark()
        }
    }
}

/// Standard xterm 16-color palette, used when a theme omits `[ansi]` keys.
const XTERM_16: [(u8, u8, u8); 16] = [
    (0x00, 0x00, 0x00),
    (0x80, 0x00, 0x00),
    (0x00, 0x80, 0x00),
    (0x80, 0x80, 0x00),
    (0x00, 0x00, 0x80),
    (0x80, 0x00, 0x80),
    (0x00, 0x80, 0x80),
    (0xc0, 0xc0, 0xc0),
    (0x80, 0x80, 0x80),
    (0xff, 0x00, 0x00),
    (0x00, 0xff, 0x00),
    (0xff, 0xff, 0x00),
    (0x00, 0x00, 0xff),
    (0xff, 0x00, 0xff),
    (0x00, 0xff, 0xff),
    (0xff, 0xff, 0xff),
];

fn parse_ansi(file: Option<&AnsiFile>) -> Result<[Color; 16], String> {
    let mut palette = XTERM_16.map(|(r, g, b)| Color::Rgb(r, g, b));
    if let Some(ansi) = file {
        let keys = [
            &ansi.black,
            &ansi.red,
            &ansi.green,
            &ansi.yellow,
            &ansi.blue,
            &ansi.magenta,
            &ansi.cyan,
            &ansi.white,
            &ansi.bright_black,
            &ansi.bright_red,
            &ansi.bright_green,
            &ansi.bright_yellow,
            &ansi.bright_blue,
            &ansi.bright_magenta,
            &ansi.bright_cyan,
            &ansi.bright_white,
        ];
        for (slot, value) in palette.iter_mut().zip(keys) {
            if let Some(hex) = value {
                *slot = parse_hex_color(hex)?;
            }
        }
    }
    Ok(palette)
}

fn config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(CONFIG_DIR)
}

/// The config file the CLI reads and the settings overlay writes back to.
pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// Theme names offered by the settings overlay: `auto`, the built-ins, then
/// every `*.toml` in the user themes directory.
pub fn theme_names() -> Vec<String> {
    let mut names = vec![
        "auto".to_string(),
        "kodade-dark".to_string(),
        "kodade-light".to_string(),
        "tokyo-night".to_string(),
    ];
    if let Ok(entries) = fs::read_dir(config_dir().join("themes")) {
        let mut user = entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                (path.extension()? == "toml")
                    .then(|| path.file_stem()?.to_str().map(str::to_owned))?
            })
            .filter(|name| !names.contains(name))
            .collect::<Vec<_>>();
        user.sort();
        names.extend(user);
    }
    names
}

fn load_user_theme(name: &str) -> Option<Theme> {
    let path = config_dir().join("themes").join(format!("{name}.toml"));
    fs::read_to_string(path)
        .ok()
        .and_then(|source| Theme::parse(&source).ok())
}

pub fn parse_hex_color(value: &str) -> Result<Color, String> {
    let value = value.strip_prefix('#').ok_or("color must begin with #")?;
    if value.len() != 6 {
        return Err("color must have six hex digits".into());
    }
    let byte = |range: std::ops::Range<usize>| -> Result<u8, String> {
        u8::from_str_radix(&value[range], 16).map_err(|_| "invalid hex color".into())
    };
    Ok(Color::Rgb(byte(0..2)?, byte(2..4)?, byte(4..6)?))
}

fn parse_rgb_color(value: &str) -> Result<Color, String> {
    let mut values = value.split('/').map(|part| u16::from_str_radix(part, 16));
    let scale = |value: u16| ((u32::from(value) * 255) / 65535) as u8;
    match (values.next(), values.next(), values.next(), values.next()) {
        (Some(Ok(red)), Some(Ok(green)), Some(Ok(blue)), None) => {
            Ok(Color::Rgb(scale(red), scale(green), scale(blue)))
        }
        _ => Err("invalid terminal color response".into()),
    }
}

fn terminal_background() -> Option<String> {
    // OSC 11 replies with `ESC ] 11 ; rgb:rrrr/gggg/bbbb BEL`. Query before the TUI enters raw mode.
    use std::{io::Read, os::fd::AsRawFd};

    crossterm::terminal::enable_raw_mode().ok()?;
    let mut stdout = std::io::stdout();
    let result = (|| {
        stdout.write_all(b"\x1b]11;?\x07").ok()?;
        stdout.flush().ok()?;
        let stdin = std::io::stdin();
        let mut pollfd = libc::pollfd {
            fd: stdin.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut pollfd, 1, 100) } <= 0 {
            return None;
        }
        let mut response = [0_u8; 64];
        let count = stdin.lock().read(&mut response).ok()?;
        let response = std::str::from_utf8(&response[..count]).ok()?;
        response
            .strip_prefix("\x1b]11;rgb:")?
            .trim_end_matches(['\x07', '\\'])
            .split_once("\x1b")
            .map(|(color, _)| color)
            .or_else(|| {
                response
                    .strip_prefix("\x1b]11;rgb:")
                    .map(|color| color.trim_end_matches(['\x07', '\\']))
            })
            .map(str::to_owned)
    })();
    let _ = crossterm::terminal::disable_raw_mode();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_colors_and_theme_toml() {
        assert_eq!(parse_hex_color("#7aa2f7"), Ok(Color::Rgb(122, 162, 247)));
        assert!(parse_hex_color("#xyz").is_err());
        assert_eq!(Theme::kodade_dark().name, "kodade-dark");
    }

    #[test]
    fn kodade_dark_populates_expanded_schema() {
        let theme = Theme::kodade_dark();
        assert_eq!(theme.accent, Color::Rgb(0xE7, 0xA3, 0x3B));
        assert_eq!(theme.bg, Color::Rgb(0x2a, 0x28, 0x25));
        assert_eq!(theme.cursor, Color::Rgb(0xe2, 0xb8, 0x6e));
        assert_eq!(theme.tab_active_bg, Color::Rgb(0x38, 0x35, 0x2f));
        // ANSI table parsed, purple-free magenta slot.
        assert_eq!(theme.ansi[2], Color::Rgb(0xa8, 0xc8, 0x7f)); // green
        assert_eq!(theme.ansi[5], Color::Rgb(0xd9, 0x8a, 0x5b)); // "magenta"
    }

    #[test]
    fn ten_field_theme_falls_back_for_optional_fields() {
        // A legacy user theme with only the original 10 fields must still load.
        let source = "\
name = \"legacy\"
accent = \"#112233\"
border = \"#445566\"
text = \"#778899\"
dim = \"#010203\"
blocked = \"#111111\"
working = \"#222222\"
done = \"#333333\"
idle = \"#444444\"
tabbar_bg = \"#0a0b0c\"
status_bg = \"#0d0e0f\"
";
        let theme = Theme::parse(source).expect("legacy theme loads");
        // Fallbacks: bg→tabbar_bg, surface→status_bg, selection→border,
        // cursor→accent, menu_bg→status_bg, menu_fg→text, tab_active_fg→accent,
        // tab_active_bg→tabbar_bg, sidebar_bg→tabbar_bg.
        assert_eq!(theme.bg, theme.tabbar_bg);
        assert_eq!(theme.surface, theme.status_bg);
        assert_eq!(theme.selection, theme.border);
        assert_eq!(theme.cursor, theme.accent);
        assert_eq!(theme.menu_bg, theme.status_bg);
        assert_eq!(theme.menu_fg, theme.text);
        assert_eq!(theme.tab_active_fg, theme.accent);
        assert_eq!(theme.tab_active_bg, theme.tabbar_bg);
        assert_eq!(theme.sidebar_bg, theme.tabbar_bg);
        // Missing [ansi] → standard xterm palette.
        assert_eq!(theme.ansi[1], Color::Rgb(0x80, 0x00, 0x00));
        assert_eq!(theme.ansi[9], Color::Rgb(0xff, 0x00, 0x00));
    }

    #[test]
    fn partial_ansi_table_overrides_only_listed_keys() {
        let source = "\
name = \"partial\"
accent = \"#112233\"
border = \"#445566\"
text = \"#778899\"
dim = \"#010203\"
blocked = \"#111111\"
working = \"#222222\"
done = \"#333333\"
idle = \"#444444\"
tabbar_bg = \"#0a0b0c\"
status_bg = \"#0d0e0f\"

[ansi]
red = \"#abcdef\"
";
        let theme = Theme::parse(source).expect("partial ansi loads");
        assert_eq!(theme.ansi[1], Color::Rgb(0xab, 0xcd, 0xef)); // overridden
        assert_eq!(theme.ansi[2], Color::Rgb(0x00, 0x80, 0x00)); // xterm default
    }

    #[test]
    fn theme_names_alias_and_resolve_built_ins_first() {
        // `dark`/`light` alias to the Ködade built-ins.
        let dark = Config::from_file(FileConfig {
            theme: Some("dark".into()),
            ..FileConfig::default()
        });
        assert_eq!(dark.theme, ThemeChoice::Dark);
        assert_eq!(dark.resolve_theme().name, "kodade-dark");
        let light = Config::from_file(FileConfig {
            theme: Some("light".into()),
            ..FileConfig::default()
        });
        assert_eq!(light.resolve_theme().name, "kodade-light");
        // Explicit built-in names resolve without touching the user dir.
        assert_eq!(builtin_theme("kodade-light").unwrap().name, "kodade-light");
        assert_eq!(builtin_theme("tokyo-night").unwrap().name, "tokyo-night");
        assert!(builtin_theme("no-such-theme").is_none());
        let tokyo = Config::from_file(FileConfig {
            theme: Some("tokyo-night".into()),
            ..FileConfig::default()
        });
        assert_eq!(tokyo.theme, ThemeChoice::Named);
        assert_eq!(tokyo.resolve_theme().name, "tokyo-night");
    }

    #[test]
    fn osc11_query_guarded_to_auto_only() {
        // #24: terminal_background() (OSC 11) must only run for `auto`. Named,
        // dark, and light arms resolve fixed built-ins and never query.
        assert_eq!(Config::default().theme, ThemeChoice::Auto);
        for name in ["dark", "light", "kodade-dark", "tokyo-night"] {
            let config = Config::from_file(FileConfig {
                theme: Some(name.into()),
                ..FileConfig::default()
            });
            assert_ne!(config.theme, ThemeChoice::Auto);
        }
    }

    #[test]
    fn parses_key_chords_in_any_modifier_order() {
        assert_eq!(
            parse_key_chord("ctrl+b"),
            Ok(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL))
        );
        assert_eq!(
            parse_key_chord("alt+x"),
            Ok(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT))
        );
        assert_eq!(parse_key_chord("alt+ctrl+v"), parse_key_chord("ctrl+alt+v"));
        assert_eq!(
            parse_key_chord("F12"),
            Ok(KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE))
        );
        for (text, code) in [
            ("enter", KeyCode::Enter),
            ("esc", KeyCode::Esc),
            ("space", KeyCode::Char(' ')),
            ("backspace", KeyCode::Backspace),
            ("tab", KeyCode::Tab),
            ("home", KeyCode::Home),
            ("end", KeyCode::End),
            ("pageup", KeyCode::PageUp),
            ("pagedown", KeyCode::PageDown),
            ("delete", KeyCode::Delete),
            ("up", KeyCode::Up),
            ("down", KeyCode::Down),
            ("left", KeyCode::Left),
            ("right", KeyCode::Right),
        ] {
            assert_eq!(
                parse_key_chord(text),
                Ok(KeyEvent::new(code, KeyModifiers::NONE)),
                "{text}"
            );
        }
        assert!(parse_key_chord("ctrl+nope").is_err());
        assert!(parse_key_chord("F13").is_err());
        assert!(parse_key_chord("ctrl+").is_err());
    }

    #[test]
    fn shift_normalizes_letters_but_not_other_keys() {
        // `shift+x` is the same binding as `X`, and neither carries SHIFT.
        assert_eq!(parse_key_chord("shift+x"), parse_key_chord("X"));
        assert_eq!(
            parse_key_chord("shift+x"),
            Ok(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE))
        );
        assert_eq!(
            parse_key_chord("ctrl+shift+l"),
            Ok(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::CONTROL))
        );
        // Non-letters keep SHIFT: their unshifted form is a different key.
        assert_eq!(
            parse_key_chord("shift+tab"),
            Ok(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT))
        );
        // A terminal that reports SHIFT with an uppercase letter still matches.
        let defaults = Config::default();
        assert_eq!(
            defaults.action(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT)),
            defaults.action(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::NONE))
        );
    }

    #[test]
    fn chords_render_back_to_config_text() {
        assert_eq!(
            render_chord(parse_key_chord("ctrl+alt+v").unwrap()),
            "ctrl+alt+v"
        );
        assert_eq!(render_chord(parse_key_chord("F5").unwrap()), "F5");
        assert_eq!(render_chord(parse_key_chord("space").unwrap()), "space");
        assert_eq!(render_chord(parse_key_chord("%").unwrap()), "%");
        assert_eq!(
            render_chord(parse_key_chord("shift+tab").unwrap()),
            "shift+tab"
        );
    }

    #[test]
    fn ctrl_and_alt_chords_are_global_unless_prefix_qualified() {
        assert!(parse_binding("ctrl+alt+v").unwrap().global);
        assert!(parse_binding("alt+h").unwrap().global);
        assert!(!parse_binding("prefix+ctrl+alt+v").unwrap().global);
        assert!(!parse_binding("%").unwrap().global);
        assert!(!parse_binding("X").unwrap().global);
        // Defaults are all prefixed; nothing fires without the prefix.
        assert_eq!(
            Config::default().global_action(parse_key_chord("ctrl+alt+v").unwrap()),
            None
        );
    }

    #[test]
    fn default_bindings_are_unique() {
        // Catches two actions claiming the same default chord after a merge.
        let defaults = Config::default();
        let mut seen = Vec::new();
        for action in Config::actions().iter().map(|(_, action)| *action) {
            for chord in defaults.chords_for(action) {
                assert!(!seen.contains(&chord), "duplicate default binding {chord}");
                seen.push(chord);
            }
        }
    }

    fn keys(entries: Vec<(&str, Chords)>) -> FileConfig {
        FileConfig {
            keys: Some(
                entries
                    .into_iter()
                    .map(|(name, chords)| (name.to_string(), chords))
                    .collect(),
            ),
            ..FileConfig::default()
        }
    }

    #[test]
    fn layout_management_actions_have_default_chords() {
        let config = Config::default();
        for (chord, action) in [
            ("1", Action::SelectTabIndex(1)),
            ("9", Action::SelectTabIndex(9)),
            ("X", Action::CloseTab),
            ("T", Action::RenameTab),
            ("R", Action::RenameWorkspace),
            ("D", Action::CloseWorkspace),
            ("alt+h", Action::SwapLeft),
            ("alt+j", Action::SwapDown),
            ("alt+k", Action::SwapUp),
            ("alt+l", Action::SwapRight),
            ("o", Action::NextPane),
            ("O", Action::PrevPane),
            (";", Action::LastPane),
            ("alt+r", Action::ResizeMode),
            ("!", Action::BreakPane),
            ("=", Action::LayoutEven),
        ] {
            assert_eq!(
                config.action(parse_key_chord(chord).unwrap()),
                Some(action),
                "{chord}"
            );
        }
        // move_tab_* and workspace_prev ship unbound but remain remappable.
        assert!(!config.bindings.values().any(|action| matches!(
            action,
            Action::MoveTabLeft | Action::MoveTabRight | Action::WorkspacePrev
        )));
        let remapped = Config::from_file(FileConfig {
            keys: Some(HashMap::from([
                ("move_tab_left".into(), "<".into()),
                ("workspace_prev".into(), "alt+w".into()),
                ("select_tab_3".into(), "F3".into()),
            ])),
            ..FileConfig::default()
        });
        assert_eq!(
            remapped.action(parse_key_chord("<").unwrap()),
            Some(Action::MoveTabLeft)
        );
        // #20 grammar: a bare alt chord is global, so it fires without the prefix.
        assert_eq!(
            remapped.global_action(parse_key_chord("alt+w").unwrap()),
            Some(Action::WorkspacePrev)
        );
        assert_eq!(
            remapped.action(parse_key_chord("F3").unwrap()),
            Some(Action::SelectTabIndex(3))
        );
        // The replaced default chord is gone; the other digits stay.
        assert_eq!(remapped.action(parse_key_chord("3").unwrap()), None);
        assert_eq!(
            remapped.action(parse_key_chord("4").unwrap()),
            Some(Action::SelectTabIndex(4))
        );
        assert!(Action::parse("select_tab_0").is_none());
    }

    #[test]
    fn layout_actions_map_to_their_messages() {
        assert_eq!(
            Action::SelectTabIndex(7).message(),
            Some(ClientMessage::SelectTabIndex { index: 7 })
        );
        assert_eq!(
            Action::SwapLeft.message(),
            Some(ClientMessage::SwapPane {
                direction: Direction::Left
            })
        );
        assert_eq!(
            Action::MoveTabRight.message(),
            Some(ClientMessage::MoveTab { delta: 1 })
        );
        assert_eq!(
            Action::PrevPane.message(),
            Some(ClientMessage::FocusPaneCycle { forward: false })
        );
        assert_eq!(
            Action::WorkspacePrev.message(),
            Some(ClientMessage::SelectWorkspaceDelta { delta: -1 })
        );
        assert_eq!(Action::BreakPane.message(), Some(ClientMessage::BreakPane));
        assert_eq!(
            Action::LayoutEven.message(),
            Some(ClientMessage::EqualizeLayout)
        );
        // Prompt- and snapshot-driven actions are resolved by `App`.
        assert!(Action::CloseTab.message().is_none());
        assert!(Action::ResizeMode.message().is_none());
    }

    #[test]
    fn status_and_window_title_parse_with_defaults() {
        let defaults = Config::default();
        assert_eq!(
            defaults.status_right,
            vec![StatusWidget::Zoom, StatusWidget::Blocked]
        );
        assert_eq!(defaults.window_title, "Ködade · {workspace} · {tab}");
        assert_eq!(
            defaults.action(parse_key_chord("q").unwrap()),
            Some(Action::DisplayPanes)
        );
        let file = Config::from_file(FileConfig {
            status: Some(StatusFile {
                right: Some(vec![
                    "time".into(),
                    "hostname".into(),
                    "nope".into(),
                    "zoom".into(),
                ]),
            }),
            ui: Some(UiFile {
                window_title: Some("{session}:{tab}".into()),
            }),
            ..FileConfig::default()
        });
        // Unknown widget names drop out; the rest keep their order.
        assert_eq!(
            file.status_right,
            vec![
                StatusWidget::Time,
                StatusWidget::Hostname,
                StatusWidget::Zoom
            ]
        );
        assert_eq!(file.window_title, "{session}:{tab}");
    }

    #[test]
    fn bindings_keep_defaults_and_replace_overrides() {
        let defaults = Config::default();
        assert_eq!(
            defaults.action(parse_key_chord("%").unwrap()),
            Some(Action::SplitRight)
        );
        let config = Config::from_file(keys(vec![("split_right", "s".into())]));
        assert_eq!(config.action(parse_key_chord("%").unwrap()), None);
        assert_eq!(
            config.action(parse_key_chord("s").unwrap()),
            Some(Action::SplitRight)
        );
    }

    #[test]
    fn binding_arrays_bind_prefixed_and_global_chords() {
        // The issue's acceptance case: both fire, the second without the prefix.
        let config = Config::from_file(keys(vec![(
            "split_right",
            Chords::Many(vec!["%".into(), "ctrl+alt+v".into()]),
        )]));
        assert_eq!(
            config.action(parse_key_chord("%").unwrap()),
            Some(Action::SplitRight)
        );
        assert_eq!(
            config.global_action(parse_key_chord("ctrl+alt+v").unwrap()),
            Some(Action::SplitRight)
        );
        // Not reachable through the prefix, and the prefixed chord is not global.
        assert_eq!(config.action(parse_key_chord("ctrl+alt+v").unwrap()), None);
        assert_eq!(config.global_action(parse_key_chord("%").unwrap()), None);
        assert_eq!(
            config.chords_for(Action::SplitRight),
            vec!["%".to_string(), "ctrl+alt+v".to_string()]
        );
    }

    #[test]
    fn invalid_and_unknown_keys_become_warnings() {
        let config = Config::from_file(keys(vec![
            ("zoom", "nope".into()),
            ("not_an_action", "z".into()),
        ]));
        assert_eq!(config.warnings.len(), 2);
        // A rejected override leaves the default in place.
        assert_eq!(
            config.action(parse_key_chord("z").unwrap()),
            Some(Action::Zoom)
        );
    }

    #[test]
    fn mouse_accepts_a_boolean_or_a_table() {
        // Pre-0.2 shape.
        let legacy = toml::from_str::<FileConfig>("mouse = false").expect("legacy config parses");
        let legacy = Config::from_file(legacy);
        assert!(!legacy.mouse);
        assert!(legacy.copy_on_select);
        // Table shape with the new keys.
        let table = toml::from_str::<FileConfig>(
            "[mouse]\nenabled = true\ncopy_on_select = false\n\n[notify]\nenabled = false\n",
        )
        .expect("table config parses");
        let table = Config::from_file(table);
        assert!(table.mouse);
        assert!(!table.copy_on_select);
        assert!(!table.notify);
        // `notify = true` scalar also works.
        let scalar = Config::from_file(
            toml::from_str::<FileConfig>("notify = false").expect("scalar notify parses"),
        );
        assert!(!scalar.notify);
    }

    #[test]
    fn an_empty_array_unbinds_an_action() {
        let config = Config::from_file(keys(vec![("zoom", Chords::Many(Vec::new()))]));
        assert_eq!(config.action(parse_key_chord("z").unwrap()), None);
        assert!(config.chords_for(Action::Zoom).is_empty());
        assert!(config.warnings.is_empty());
        // All-invalid values are a typo, not an unbind: the default survives.
        let broken = Config::from_file(keys(vec![("zoom", "nope".into())]));
        assert_eq!(
            broken.action(parse_key_chord("z").unwrap()),
            Some(Action::Zoom)
        );
    }

    #[test]
    fn taking_a_chord_from_another_action_warns() {
        // The v0.1 docs example now displaces the settings menu.
        let config = Config::from_file(keys(vec![("split_right", "s".into())]));
        assert_eq!(
            config.warnings,
            vec!["key s for split_right replaces settings".to_string()]
        );
        assert_eq!(
            config.action(parse_key_chord("s").unwrap()),
            Some(Action::SplitRight)
        );
    }

    #[test]
    fn unknown_settings_are_reported() {
        let file = toml::from_str::<FileConfig>(
            "sidbar = true\n\n[mouse]\ncopy_on_selct = false\n\n[notify]\nenabld = true\n",
        )
        .expect("typo config still parses");
        let config = Config::from_file(file);
        assert_eq!(
            config.warnings,
            vec![
                "unknown setting sidbar".to_string(),
                "unknown setting mouse.copy_on_selct".to_string(),
                "unknown setting notify.enabld".to_string(),
            ]
        );
        // The known settings keep their defaults.
        assert!(config.sidebar);
        assert!(config.copy_on_select);
    }

    #[test]
    fn effective_config_prints_as_toml() {
        let toml_text = Config::default().to_toml();
        assert!(toml_text.contains("theme = \"auto\""));
        assert!(toml_text.contains("split_down = [\"\\\"\"]"));
        assert!(toml_text.contains("[mouse]"));
        assert!(toml_text.contains("copy_on_select = true"));
        assert!(toml_text.contains("[notify]"));
        assert!(toml_text.contains("prefix = \"ctrl+b\""));
        assert!(toml_text.contains("split_right = [\"%\"]"));
        // Round-trips exactly: loading the printed config prints the same text.
        let file = toml::from_str::<FileConfig>(&toml_text).expect("effective config parses");
        let reloaded = Config::from_file(file);
        assert!(reloaded.warnings.is_empty(), "{:?}", reloaded.warnings);
        assert_eq!(reloaded.to_toml(), toml_text);
    }
}
