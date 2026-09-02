//! The `prefix s` settings menu: a small overlay over the live config that
//! writes changes back to `config.toml`.
//!
//! Writes go through `toml_edit` so comments, formatting, and keys this menu
//! does not know about survive the round trip.

use std::{fs, path::Path};

use toml_edit::{value, DocumentMut, Item, Table, TableLike, Value};

use crate::{
    config::{theme_names, Config},
    overlay::{Overlay, OverlayRow, OverlayTarget},
};

/// One row of the settings menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    Theme,
    Mouse,
    Sidebar,
    CopyOnSelect,
    Notify,
}

/// Menu order.
pub const SETTINGS: &[Setting] = &[
    Setting::Theme,
    Setting::Mouse,
    Setting::Sidebar,
    Setting::CopyOnSelect,
    Setting::Notify,
];

impl Setting {
    fn label(self) -> &'static str {
        match self {
            Self::Theme => "theme",
            Self::Mouse => "mouse",
            Self::Sidebar => "sidebar",
            Self::CopyOnSelect => "copy on select",
            Self::Notify => "notifications",
        }
    }

    // Current value, shown right-aligned in the overlay.
    fn value(self, config: &Config) -> String {
        let onoff = |flag: bool| if flag { "on" } else { "off" }.to_string();
        match self {
            Self::Theme => config.theme_name().to_string(),
            Self::Mouse => onoff(config.mouse),
            Self::Sidebar => onoff(config.sidebar),
            Self::CopyOnSelect => onoff(config.copy_on_select),
            Self::Notify => onoff(config.notify),
        }
    }
}

/// Builds the settings overlay for the current config, keeping the selection.
pub fn overlay(config: &Config, selected: usize) -> Overlay {
    let rows = SETTINGS
        .iter()
        .enumerate()
        .map(|(index, setting)| {
            OverlayRow::new(
                format!(" {}", setting.label()),
                format!("{} ", setting.value(config)),
                OverlayTarget::Index(index),
            )
        })
        .collect();
    let mut overlay = Overlay::new("settings · enter toggles · esc closes", rows);
    overlay.selected = selected.min(SETTINGS.len().saturating_sub(1));
    overlay
}

/// Toggles a boolean setting or cycles to the next theme, in memory.
pub fn toggle(config: &mut Config, setting: Setting) {
    match setting {
        Setting::Theme => {
            let names = theme_names();
            let current = names
                .iter()
                .position(|name| name == config.theme_name())
                .unwrap_or(0);
            let next = &names[(current + 1) % names.len()];
            config.set_theme(next);
        }
        Setting::Mouse => config.mouse = !config.mouse,
        Setting::Sidebar => config.sidebar = !config.sidebar,
        Setting::CopyOnSelect => config.copy_on_select = !config.copy_on_select,
        Setting::Notify => config.notify = !config.notify,
    }
}

/// Writes the menu-owned settings back to `path`, creating the file when
/// missing and leaving every other key, comment, and blank line untouched.
pub fn write(path: &Path, config: &Config) -> Result<(), String> {
    let source = fs::read_to_string(path).unwrap_or_default();
    let mut doc = source
        .parse::<DocumentMut>()
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let root = doc.as_table_mut();
    set(root, "theme", config.theme_name().into());
    // Keep whatever shape the file uses: a `[sidebar]` table stores `show`,
    // otherwise a bare `sidebar = true` boolean (the 0.1 alias).
    if doc.get("sidebar").is_some_and(Item::is_table_like) {
        set(
            table_mut(&mut doc, "sidebar"),
            "show",
            config.sidebar.into(),
        );
    } else {
        set(doc.as_table_mut(), "sidebar", config.sidebar.into());
    }
    // Keep whatever shape the file already uses: a bare `mouse = true` stays a
    // boolean unless copy_on_select needs the table form.
    let mouse_is_table = doc.get("mouse").is_some_and(Item::is_table_like);
    if mouse_is_table || !config.copy_on_select {
        if !mouse_is_table {
            doc.remove("mouse");
        }
        let mouse = table_mut(&mut doc, "mouse");
        set(mouse, "enabled", config.mouse.into());
        set(mouse, "copy_on_select", config.copy_on_select.into());
    } else {
        set(doc.as_table_mut(), "mouse", config.mouse.into());
    }
    // Same rule for notify: a bare boolean unless the file already uses a table.
    if doc.get("notify").is_some_and(Item::is_table_like) {
        set(
            table_mut(&mut doc, "notify"),
            "enabled",
            config.notify.into(),
        );
    } else {
        set(doc.as_table_mut(), "notify", config.notify.into());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    fs::write(path, doc.to_string()).map_err(|error| format!("{}: {error}", path.display()))
}

// Sets `key`, reusing the existing value's decor so surrounding comments and
// spacing survive; a missing key is appended.
fn set(table: &mut dyn TableLike, key: &str, new: Value) {
    if let Some(existing) = table.get_mut(key).and_then(Item::as_value_mut) {
        let decor = existing.decor().clone();
        *existing = new;
        *existing.decor_mut() = decor;
        return;
    }
    table.insert(key, value(new));
}

// Returns `key` as a table, creating a real `[key]` table when it is missing
// and leaving an existing inline table alone.
fn table_mut<'a>(doc: &'a mut DocumentMut, key: &str) -> &'a mut dyn TableLike {
    if !doc.get(key).is_some_and(Item::is_table_like) {
        doc.insert(key, Item::Table(Table::new()));
    }
    doc[key].as_table_like_mut().expect("table inserted")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kodade-cli-settings-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn overlay_lists_current_values() {
        let config = Config::default();
        let overlay = overlay(&config, 0);
        assert_eq!(overlay.rows.len(), SETTINGS.len());
        assert_eq!(overlay.rows[0].hint.trim(), "auto");
        assert_eq!(overlay.rows[1].hint.trim(), "on");
    }

    #[test]
    fn toggling_flips_flags_and_cycles_themes() {
        let mut config = Config::default();
        toggle(&mut config, Setting::Mouse);
        assert!(!config.mouse);
        toggle(&mut config, Setting::Notify);
        assert!(!config.notify);
        toggle(&mut config, Setting::CopyOnSelect);
        assert!(!config.copy_on_select);
        // `auto` is first in the list, so one cycle lands on kodade-dark.
        toggle(&mut config, Setting::Theme);
        assert_eq!(config.theme_name(), "kodade-dark");
    }

    #[test]
    fn write_preserves_comments_and_unknown_keys() {
        let dir = temp_dir("preserve");
        let path = dir.join("config.toml");
        fs::write(
            &path,
            "# my config\ntheme = \"tokyo-night\"\nfuture_key = 7\n\n[keys]\nsplit_right = \"s\"\n",
        )
        .expect("seed config");
        let mut config = Config::default();
        config.set_theme("kodade-light");
        config.mouse = false;
        write(&path, &config).expect("write succeeds");
        let written = fs::read_to_string(&path).expect("read back");
        assert!(written.starts_with("# my config"));
        assert!(written.contains("theme = \"kodade-light\""));
        assert!(written.contains("future_key = 7"));
        assert!(written.contains("split_right = \"s\""));
        assert!(written.contains("mouse = false"));
        assert!(written.contains("notify = true"));
        // Round-trips as valid TOML.
        written.parse::<DocumentMut>().expect("valid toml");
    }

    #[test]
    fn write_keeps_an_existing_notify_table() {
        let dir = temp_dir("notify-table");
        let path = dir.join("config.toml");
        fs::write(&path, "[notify]\n# keep me\nenabled = true\n").expect("seed config");
        let mut config = Config::default();
        config.notify = false;
        write(&path, &config).expect("write succeeds");
        let written = fs::read_to_string(&path).expect("read back");
        assert!(written.contains("[notify]"));
        assert!(written.contains("# keep me"));
        assert!(written.contains("enabled = false"));
    }

    #[test]
    fn write_creates_a_missing_file_and_uses_the_table_form_when_needed() {
        let dir = temp_dir("create");
        let path = dir.join("nested").join("config.toml");
        let mut config = Config::default();
        config.copy_on_select = false;
        write(&path, &config).expect("write succeeds");
        let written = fs::read_to_string(&path).expect("read back");
        assert!(written.contains("[mouse]"));
        assert!(written.contains("copy_on_select = false"));
        assert!(written.contains("enabled = true"));
        // notify has no extra keys, so it stays a plain boolean.
        assert!(written.contains("notify = true"));
    }
}
