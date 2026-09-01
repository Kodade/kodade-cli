use std::{collections::HashMap, fs, io::Write, path::PathBuf};

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
    pub prefix: KeyEvent,
    bindings: HashMap<KeyEvent, Action>,
    named_theme: Option<String>,
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
}

impl Action {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "split_right" => Self::SplitRight,
            "split_down" => Self::SplitDown,
            "close_pane" => Self::ClosePane,
            "new_tab" => Self::NewTab,
            "next_tab" => Self::NextTab,
            "prev_tab" => Self::PrevTab,
            "zoom" => Self::Zoom,
            "detach" => Self::Detach,
            "rename" => Self::Rename,
            "workspace_next" => Self::WorkspaceNext,
            "new_workspace" => Self::NewWorkspace,
            "sidebar_toggle" => Self::SidebarToggle,
            "focus_up" => Self::FocusUp,
            "focus_down" => Self::FocusDown,
            "focus_left" => Self::FocusLeft,
            "focus_right" => Self::FocusRight,
            "resize_up" => Self::ResizeUp,
            "resize_down" => Self::ResizeDown,
            "resize_left" => Self::ResizeLeft,
            "resize_right" => Self::ResizeRight,
            "navigate" => Self::Navigate,
            "copy_mode" => Self::CopyMode,
            _ => return None,
        })
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
            Self::NewWorkspace => ClientMessage::NewWorkspace {
                name: "workspace".into(),
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
            Self::Detach | Self::Rename | Self::WorkspaceNext | Self::SidebarToggle => return None,
            // M3b reserves these names without introducing their modes early.
            Self::Navigate | Self::CopyMode => return None,
        })
    }
}

#[derive(Debug, Deserialize, Default)]
struct FileConfig {
    theme: Option<String>,
    mouse: Option<bool>,
    sidebar: Option<bool>,
    keys: Option<HashMap<String, String>>,
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
            ("p", Action::PrevTab),
            ("z", Action::Zoom),
            ("d", Action::Detach),
            ("r", Action::Rename),
            ("w", Action::WorkspaceNext),
            ("W", Action::NewWorkspace),
            ("b", Action::SidebarToggle),
            ("[", Action::CopyMode),
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
        ] {
            bindings.insert(
                parse_key_chord(binding).expect("built-in key is valid"),
                action,
            );
        }
        Self {
            theme: ThemeChoice::Auto,
            mouse: true,
            sidebar: true,
            prefix: parse_key_chord("ctrl+b").expect("built-in prefix is valid"),
            bindings,
            named_theme: None,
        }
    }
}

impl Config {
    pub fn load() -> Self {
        let path = config_dir().join("config.toml");
        let Ok(source) = fs::read_to_string(&path) else {
            return Self::default();
        };
        let Ok(file) = toml::from_str::<FileConfig>(&source) else {
            eprintln!(
                "kodade-cli: warning: invalid config {}, using defaults",
                path.display()
            );
            return Self::default();
        };
        Self::from_file(file)
    }

    fn from_file(file: FileConfig) -> Self {
        let mut config = Self::default();
        if let Some(theme) = file.theme {
            config.theme = match theme.as_str() {
                "dark" => ThemeChoice::Dark,
                "light" => ThemeChoice::Light,
                "auto" => ThemeChoice::Auto,
                _ => ThemeChoice::Named,
            };
            config.named_theme = Some(theme);
        }
        config.mouse = file.mouse.unwrap_or(config.mouse);
        config.sidebar = file.sidebar.unwrap_or(config.sidebar);
        if let Some(keys) = file.keys {
            for (name, binding) in keys {
                if name == "prefix" {
                    match parse_key_chord(&binding) {
                        Ok(key) => config.prefix = key,
                        Err(error) => eprintln!("kodade-cli: warning: invalid key prefix: {error}"),
                    }
                    continue;
                }
                let Some(action) = Action::parse(&name) else {
                    eprintln!("kodade-cli: warning: unknown key action {name}");
                    continue;
                };
                match parse_key_chord(&binding) {
                    Ok(key) => {
                        config.bindings.retain(|_, mapped| *mapped != action);
                        config.bindings.insert(key, action);
                    }
                    Err(error) => eprintln!("kodade-cli: warning: invalid key {name}: {error}"),
                }
            }
        }
        config
    }

    pub fn action(&self, key: KeyEvent) -> Option<Action> {
        self.bindings.get(&key).copied()
    }

    pub fn resolve_theme(&self) -> Theme {
        match self.theme {
            ThemeChoice::Dark => Theme::dark(),
            ThemeChoice::Light => Theme::light(),
            ThemeChoice::Auto => terminal_background()
                .as_deref()
                .map(Theme::from_background)
                .unwrap_or_else(Theme::dark),
            ThemeChoice::Named => self
                .named_theme
                .as_deref()
                .and_then(load_user_theme)
                .unwrap_or_else(|| {
                    eprintln!("kodade-cli: warning: theme not found or invalid, using dark");
                    Theme::dark()
                }),
        }
    }
}

pub fn parse_key_chord(value: &str) -> Result<KeyEvent, String> {
    let value = value.trim();
    let (modifier, key) = if let Some(key) = value.strip_prefix("ctrl+") {
        (KeyModifiers::CONTROL, key)
    } else if let Some(key) = value.strip_prefix("alt+") {
        (KeyModifiers::ALT, key)
    } else {
        (KeyModifiers::NONE, value)
    };
    let code = match key {
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        _ if key.len() == 2 && key.starts_with('F') => key[1..]
            .parse::<u8>()
            .map(KeyCode::F)
            .map_err(|_| format!("unsupported key chord {value}"))?,
        _ if key.chars().count() == 1 => KeyCode::Char(key.chars().next().unwrap()),
        _ => return Err(format!("unsupported key chord {value}")),
    };
    Ok(KeyEvent::new(code, modifier))
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
}

impl Theme {
    fn dark() -> Self {
        Self::parse(include_str!("../themes/dark.toml")).expect("built-in dark theme is valid")
    }

    fn light() -> Self {
        Self::parse(include_str!("../themes/light.toml")).expect("built-in light theme is valid")
    }

    fn parse(source: &str) -> Result<Self, String> {
        let raw = toml::from_str::<ThemeFile>(source).map_err(|error| error.to_string())?;
        Ok(Self {
            name: raw.name,
            accent: parse_hex_color(&raw.accent)?,
            border: parse_hex_color(&raw.border)?,
            text: parse_hex_color(&raw.text)?,
            dim: parse_hex_color(&raw.dim)?,
            blocked: parse_hex_color(&raw.blocked)?,
            working: parse_hex_color(&raw.working)?,
            done: parse_hex_color(&raw.done)?,
            idle: parse_hex_color(&raw.idle)?,
            tabbar_bg: parse_hex_color(&raw.tabbar_bg)?,
            status_bg: parse_hex_color(&raw.status_bg)?,
        })
    }

    fn from_background(background: &str) -> Self {
        let Ok(Color::Rgb(red, green, blue)) = parse_rgb_color(background) else {
            return Self::dark();
        };
        if u16::from(red) + u16::from(green) + u16::from(blue) > 382 {
            Self::light()
        } else {
            Self::dark()
        }
    }
}

fn config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(CONFIG_DIR)
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
        assert_eq!(Theme::dark().name, "dark");
    }

    #[test]
    fn parses_key_chords() {
        assert_eq!(
            parse_key_chord("ctrl+b"),
            Ok(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL))
        );
        assert_eq!(
            parse_key_chord("alt+x"),
            Ok(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT))
        );
        assert_eq!(
            parse_key_chord("F1"),
            Ok(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE))
        );
        assert!(parse_key_chord("shift+x").is_err());
    }

    #[test]
    fn bindings_keep_defaults_and_replace_overrides() {
        let defaults = Config::default();
        assert_eq!(
            defaults.action(parse_key_chord("%").unwrap()),
            Some(Action::SplitRight)
        );
        let config = Config::from_file(FileConfig {
            keys: Some(HashMap::from([("split_right".into(), "s".into())])),
            ..FileConfig::default()
        });
        assert_eq!(config.action(parse_key_chord("%").unwrap()), None);
        assert_eq!(
            config.action(parse_key_chord("s").unwrap()),
            Some(Action::SplitRight)
        );
    }
}
