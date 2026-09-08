//! The command center: one fuzzy, executable list of everyday Ködade actions
//! and launchable agent CLIs.  It deliberately knows no TUI state beyond the
//! live binding table, keeping filtering and activation easy to verify.

use crate::{
    config::{Action, Config},
    overlay::{Overlay, OverlayRow, OverlayTarget},
    picker::fuzzy_score,
};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteTarget {
    Action(Action),
    Agent {
        name: String,
        command: Vec<String>,
    },
    Shell,
    PluginAction {
        plugin: String,
        directory: PathBuf,
        action: String,
        command: String,
        pane: bool,
    },
    PluginUnavailable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Item {
    label: String,
    hint: String,
    search: String,
    target: PaletteTarget,
}

/// A filtered command center.  The client owns presentation and invokes the
/// selected target; the daemon remains the authority that starts the pane.
pub struct Palette {
    all: Vec<Item>,
    visible: Vec<Item>,
    pub overlay: Overlay,
}

impl Palette {
    #[cfg(test)]
    pub fn new(config: &Config) -> Self {
        Self::with_plugin_context(config, &crate::plugins::InvocationContext::default())
    }

    pub fn with_plugin_context(
        config: &Config,
        context: &crate::plugins::InvocationContext,
    ) -> Self {
        let mut all = Vec::new();
        // These are the reliable, documented entry commands for the most-used
        // bundled manifests. We send them to the attached daemon rather than
        // probing this client PATH, which keeps remote sessions honest.
        for (name, command) in [
            ("Codex", "codex"),
            ("Claude Code", "claude"),
            ("Gemini CLI", "gemini"),
            ("OpenCode", "opencode"),
            ("Pi", "pi"),
        ] {
            all.push(Item {
                label: format!("agent · {name}"),
                hint: "start in new tab".into(),
                search: format!("agent {name} {command} launch start"),
                target: PaletteTarget::Agent {
                    name: name.into(),
                    command: vec![command.into()],
                },
            });
        }
        all.push(Item {
            label: "agent · shell".into(),
            hint: "new tab".into(),
            search: "terminal shell new pane tab".into(),
            target: PaletteTarget::Shell,
        });
        match crate::plugins::palette_actions_for(context) {
            Ok(actions) => {
                for item in actions {
                    all.push(Item {
                        label: format!("plugin · {} · {}", item.plugin, item.action.name),
                        hint: if item.action.pane { "open pane" } else { "run" }.into(),
                        search: format!(
                            "plugin {} {} {}",
                            item.plugin, item.action.id, item.action.description
                        ),
                        target: PaletteTarget::PluginAction {
                            plugin: item.plugin,
                            directory: item.directory,
                            action: item.action.id,
                            command: item.action.command,
                            pane: item.action.pane,
                        },
                    });
                }
            }
            Err(error) => all.push(Item {
                label: "plugin registry unavailable".into(),
                hint: error.to_string(),
                search: "plugin registry error".into(),
                target: PaletteTarget::PluginUnavailable(error.to_string()),
            }),
        }
        for (name, action) in Config::actions() {
            let hint = config
                .chords_for(*action)
                .into_iter()
                .next()
                .unwrap_or_default();
            let label = format!("action · {}", action_label(*action));
            all.push(Item {
                search: format!("{name} {label} {hint}"),
                label,
                hint,
                target: PaletteTarget::Action(*action),
            });
        }
        let visible = filter(&all, "");
        let rows = visible.iter().map(row).collect();
        let mut overlay = Overlay::new("command center · type to filter · esc closes", rows);
        overlay.filter = Some(String::new());
        Self {
            all,
            visible,
            overlay,
        }
    }

    pub fn apply_filter(&mut self) {
        let query = self.overlay.filter.as_deref().unwrap_or_default();
        self.visible = filter(&self.all, query);
        self.overlay.rows = self.visible.iter().map(row).collect();
        self.overlay.selected = self
            .overlay
            .selected
            .min(self.overlay.rows.len().saturating_sub(1));
    }

    pub fn current_target(&self) -> Option<PaletteTarget> {
        self.visible
            .get(self.overlay.selected)
            .map(|item| item.target.clone())
    }
}

fn row(item: &Item) -> OverlayRow {
    OverlayRow::new(item.label.clone(), item.hint.clone(), OverlayTarget::None)
}

fn filter(items: &[Item], query: &str) -> Vec<Item> {
    let mut matched: Vec<(usize, u32, &Item)> = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            fuzzy_score(query, &item.search).map(|score| (index, score, item))
        })
        .collect();
    matched.sort_by(|(ia, sa, _), (ib, sb, _)| sb.cmp(sa).then(ia.cmp(ib)));
    matched
        .into_iter()
        .map(|(_, _, item)| item.clone())
        .collect()
}

fn action_label(action: Action) -> String {
    action.name().replace('_', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_center_finds_agent_by_command_name() {
        let mut palette = Palette::new(&Config::default());
        palette.overlay.filter = Some("codex".into());
        palette.apply_filter();
        assert!(
            matches!(palette.current_target(), Some(PaletteTarget::Agent { command, .. }) if command == vec!["codex"])
        );
    }

    #[test]
    fn command_center_keeps_live_action_bindings_discoverable() {
        let config = Config::default();
        // A default binding is enough to prove the public palette exposes it.
        let palette = Palette::new(&config);
        assert!(palette
            .overlay
            .rows
            .iter()
            .any(|row| row.label.contains("split right") && !row.hint.is_empty()));
    }

    #[test]
    fn command_center_includes_actions_that_end_the_client_loop() {
        let palette = Palette::new(&Config::default());
        assert!(palette
            .overlay
            .rows
            .iter()
            .any(|row| row.label.contains("detach")));
        assert!(palette.overlay.rows[0].label.contains("Codex"));
    }

    #[test]
    fn command_center_renders_in_narrow_and_wide_terminals() {
        use ratatui::{backend::TestBackend, Terminal};
        let palette = Palette::new(&Config::default());
        for (width, height) in [(20, 8), (100, 24)] {
            let mut terminal =
                Terminal::new(TestBackend::new(width, height)).expect("test terminal");
            terminal
                .draw(|frame| {
                    crate::overlay::render_overlay(
                        frame,
                        frame.area(),
                        &palette.overlay,
                        &crate::config::Theme::kodade_dark(),
                    )
                })
                .expect("palette renders");
            let buffer = terminal.backend().buffer();
            let rendered = (0..height)
                .map(|row| {
                    (0..width)
                        .map(|column| buffer[(column, row)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(rendered.contains("agent"), "{rendered}");
            if width > 20 {
                assert!(rendered.contains("command center"));
                assert!(rendered.contains("agent · Codex"));
            }
        }
    }
}
