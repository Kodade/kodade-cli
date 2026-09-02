//! Client UI state persisted to `~/.config/kodade-cli/state` (#19).
//!
//! This is small, non-critical UI memory (collapsed sidebar workspaces, and the
//! `help_seen` flag #6 owns) — not session layout, which the daemon persists.
//! Every field is `#[serde(default)]` so the file merges cleanly across
//! features and a missing or malformed file just starts empty.

use std::{collections::HashMap, fs};

use serde::{Deserialize, Serialize};

use crate::config::state_path;

/// Persisted client UI state, stored in `~/.config/kodade-cli/state`. This is
/// the single loader for that file: the help overlay's `help_seen` flag (#6) and
/// the sidebar's collapsed workspaces (#19) share it, so writing one preserves
/// the other. New features add `#[serde(default)]` fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    /// Whether the help overlay has been shown once (#6).
    #[serde(default)]
    pub help_seen: bool,
    /// Collapsed sidebar workspaces, keyed by session, then workspace name (#19).
    #[serde(default)]
    pub collapsed: HashMap<String, Vec<String>>,
}

impl State {
    /// Load the state file, degrading to empty on any read/parse failure.
    pub fn load() -> Self {
        fs::read_to_string(state_path())
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Write the state file, best-effort (a failure is not fatal to the UI).
    pub fn save(&self) {
        let path = state_path();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(text) = toml::to_string(self) {
            let _ = fs::write(path, text);
        }
    }

    /// Collapsed workspace names for a session (empty when none recorded).
    pub fn collapsed_for(&self, session: &str) -> Vec<String> {
        self.collapsed.get(session).cloned().unwrap_or_default()
    }

    /// Record the collapsed workspace names for a session and persist.
    pub fn set_collapsed(&mut self, session: &str, names: Vec<String>) {
        if names.is_empty() {
            self.collapsed.remove(session);
        } else {
            self.collapsed.insert(session.to_string(), names);
        }
        self.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapsed_round_trips_per_session() {
        let mut state = State::default();
        state
            .collapsed
            .insert("work".into(), vec!["api".into(), "web".into()]);
        let text = toml::to_string(&state).expect("serialize");
        let back: State = toml::from_str(&text).expect("parse");
        assert_eq!(back.collapsed_for("work"), vec!["api", "web"]);
        assert!(back.collapsed_for("other").is_empty());
    }

    #[test]
    fn set_collapsed_clears_empty_sessions() {
        let mut state = State::default();
        state.collapsed.insert("work".into(), vec!["api".into()]);
        // Empty list removes the session key entirely.
        state.collapsed.insert("work".into(), Vec::new());
        state.collapsed.retain(|_, names| !names.is_empty());
        assert!(state.collapsed_for("work").is_empty());
    }

    #[test]
    fn missing_or_bad_state_starts_empty() {
        let bad: Result<State, _> = toml::from_str("not = [valid");
        assert!(bad.is_err());
        assert!(State::default().collapsed.is_empty());
        assert!(!State::default().help_seen);
    }
}
