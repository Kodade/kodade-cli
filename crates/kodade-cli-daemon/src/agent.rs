//! Pure state selection for one pane.

use std::time::Duration;

use kodade_cli_proto::AgentStateKind;

use crate::manifest::{matching_rule, Manifest};

pub const HOOK_TTL: Duration = Duration::from_secs(30);
pub const OUTPUT_WORKING_WINDOW: Duration = Duration::from_secs(2);
pub const SCREEN_LINES: usize = 8;

#[derive(Debug, Clone)]
pub struct HookState {
    pub state: AgentStateKind,
    pub source: String,
    pub age: Duration,
    /// True when the pane emitted PTY output after this hook was reported.
    /// A `done` report sticks until output appears (or the next report), rather
    /// than decaying on the 30 s TTL that other states use.
    pub output_since_report: bool,
}

/// A hook report is authoritative while this returns true. `done` has no TTL and
/// sticks until fresh PTY output; every other state uses `HOOK_TTL`.
fn hook_is_current(hook: &HookState) -> bool {
    if hook.state == AgentStateKind::Done {
        !hook.output_since_report
    } else {
        hook.age <= HOOK_TTL
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    pub agent: Option<String>,
    pub state: AgentStateKind,
    pub reason: String,
}

pub fn detect(
    manifests: &[Manifest],
    process: Option<&str>,
    title: &str,
    screen: &str,
    output_age: Duration,
    hook: Option<HookState>,
) -> Detection {
    let manifest = manifests
        .iter()
        .find(|manifest| manifest.identifies(process, title));
    let agent = manifest.map(|manifest| manifest.display.clone());
    if let Some(hook) = hook.filter(hook_is_current) {
        let sticky = if hook.state == AgentStateKind::Done {
            " sticky until output"
        } else {
            ""
        };
        return Detection {
            agent,
            state: hook.state,
            reason: format!(
                "hook report {}s ago ({}){sticky}",
                hook.age.as_secs(),
                hook.source
            ),
        };
    }
    let Some(manifest) = manifest else {
        let state = if process.map(is_shell).unwrap_or(true) {
            AgentStateKind::Idle
        } else {
            AgentStateKind::Unknown
        };
        return Detection {
            agent: None,
            state,
            reason: format!(
                "process {} is not a known agent",
                process.unwrap_or("shell")
            ),
        };
    };
    if let Some(rule) = matching_rule(manifest, screen, SCREEN_LINES) {
        let needle = rule
            .any
            .iter()
            .find(|needle| screen.contains(*needle))
            .unwrap_or(&rule.any[0]);
        return Detection {
            agent: Some(manifest.display.clone()),
            state: rule.state.into(),
            reason: format!("manifest rule '{needle}' matched"),
        };
    }
    let (state, reason) = if output_age < OUTPUT_WORKING_WINDOW {
        (AgentStateKind::Working, "recent output")
    } else {
        (AgentStateKind::Idle, "idle")
    };
    Detection {
        agent: Some(manifest.display.clone()),
        state,
        reason: format!(
            "process {} {reason} {}s",
            process.unwrap_or("title"),
            output_age.as_secs()
        ),
    }
}

fn is_shell(process: &str) -> bool {
    matches!(process, "sh" | "bash" | "zsh" | "fish" | "nu")
}

pub fn rollup(states: impl IntoIterator<Item = AgentStateKind>) -> AgentStateKind {
    states
        .into_iter()
        .min_by_key(urgency)
        .unwrap_or(AgentStateKind::Unknown)
}

fn urgency(state: &AgentStateKind) -> u8 {
    match state {
        AgentStateKind::Blocked => 0,
        AgentStateKind::Working => 1,
        AgentStateKind::Done => 2,
        AgentStateKind::Idle => 3,
        AgentStateKind::Unknown => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Manifest, ManifestState, Rule};

    fn manifest() -> Manifest {
        Manifest {
            name: "codex".into(),
            display: "Codex".into(),
            process: vec!["codex".into()],
            title: vec![],
            resume: Some("codex resume --last".into()),
            rules: vec![Rule {
                state: ManifestState::Blocked,
                any: vec!["y/n".into()],
            }],
        }
    }

    #[test]
    fn hook_has_precedence_and_working_expires() {
        let detected = detect(
            &[manifest()],
            Some("codex"),
            "",
            "y/n",
            Duration::from_secs(10),
            Some(HookState {
                state: AgentStateKind::Working,
                source: "claude".into(),
                age: Duration::from_secs(4),
                output_since_report: false,
            }),
        );
        assert_eq!(detected.state, AgentStateKind::Working);
        // A working/blocked/idle report older than the TTL decays to the screen rule.
        let expired = detect(
            &[manifest()],
            Some("codex"),
            "",
            "y/n",
            Duration::from_secs(10),
            Some(HookState {
                state: AgentStateKind::Working,
                source: "claude".into(),
                age: Duration::from_secs(31),
                output_since_report: false,
            }),
        );
        assert_eq!(expired.state, AgentStateKind::Blocked);
    }

    #[test]
    fn done_sticks_without_ttl_until_new_output() {
        // A very old `done` report still wins as long as no output has arrived.
        let sticky = detect(
            &[manifest()],
            Some("codex"),
            "",
            "y/n",
            Duration::from_secs(10),
            Some(HookState {
                state: AgentStateKind::Done,
                source: "claude".into(),
                age: Duration::from_secs(600),
                output_since_report: false,
            }),
        );
        assert_eq!(sticky.state, AgentStateKind::Done);
        // Once the pane produces output, `done` is released and detection falls
        // back to the screen rule / heuristic.
        let released = detect(
            &[manifest()],
            Some("codex"),
            "",
            "y/n",
            Duration::from_secs(10),
            Some(HookState {
                state: AgentStateKind::Done,
                source: "claude".into(),
                age: Duration::from_secs(2),
                output_since_report: true,
            }),
        );
        assert_eq!(released.state, AgentStateKind::Blocked);
    }

    #[test]
    fn rollup_prioritizes_attention() {
        assert_eq!(
            rollup([
                AgentStateKind::Idle,
                AgentStateKind::Done,
                AgentStateKind::Working,
                AgentStateKind::Blocked
            ]),
            AgentStateKind::Blocked
        );
    }

    fn parse(source: &str) -> Manifest {
        toml::from_str(source).expect("built-in manifest parses")
    }

    /// Every new v0.2 manifest ships identification only (process + title), so a
    /// benign screen must not report `blocked`; detection falls to the output
    /// heuristic. Each tuple is (manifest source, process name, title substring).
    #[test]
    fn new_manifests_identify_without_false_blocked() {
        let cases = [
            (
                include_str!("../manifests/cursor-agent.toml"),
                "cursor-agent",
                "Cursor Agent",
            ),
            (
                include_str!("../manifests/copilot.toml"),
                "copilot",
                "Copilot",
            ),
            (include_str!("../manifests/cline.toml"), "cline", "Cline"),
            (include_str!("../manifests/amp.toml"), "amp", "Amp"),
            (include_str!("../manifests/droid.toml"), "droid", "Droid"),
            (include_str!("../manifests/kimi.toml"), "kimi", "Kimi"),
            (include_str!("../manifests/qwen-code.toml"), "qwen", "Qwen"),
            (include_str!("../manifests/pi.toml"), "pi", "Pi"),
            (include_str!("../manifests/hermes.toml"), "hermes", "Hermes"),
        ];
        for (source, process, title) in cases {
            let manifest = parse(source);
            assert!(
                manifest.identifies(Some(process), ""),
                "{process} identified by process name"
            );
            assert!(
                manifest.identifies(None, title),
                "{process} identified by title substring"
            );
            // A sourced sample of routine agent output: no attention prompt present.
            let screen = "Working on your request...\nreading files\nediting src/main.rs\n";
            let detected = detect(
                std::slice::from_ref(&manifest),
                Some(process),
                title,
                screen,
                Duration::ZERO,
                None,
            );
            assert_eq!(
                detected.agent.as_deref(),
                Some(manifest.display.as_str()),
                "{process} resolves to its display name"
            );
            assert_ne!(
                detected.state,
                AgentStateKind::Blocked,
                "identification-only manifest must not false-positive blocked"
            );
        }
    }

    #[test]
    fn screen_rule_beats_output_heuristic_and_unknown_processes_stay_unknown() {
        let rule = detect(
            &[manifest()],
            Some("codex"),
            "",
            "continue? y/n",
            Duration::ZERO,
            None,
        );
        assert_eq!(rule.state, AgentStateKind::Blocked);
        let unknown = detect(
            &[manifest()],
            Some("vim"),
            "",
            "",
            Duration::from_secs(3),
            None,
        );
        assert_eq!(unknown.state, AgentStateKind::Unknown);
    }
}
