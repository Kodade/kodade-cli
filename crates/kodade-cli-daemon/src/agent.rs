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
    if let Some(hook) = hook.filter(|hook| hook.age <= HOOK_TTL) {
        return Detection {
            agent,
            state: hook.state,
            reason: format!("hook report {}s ago ({})", hook.age.as_secs(), hook.source),
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
            rules: vec![Rule {
                state: ManifestState::Blocked,
                any: vec!["y/n".into()],
            }],
        }
    }

    #[test]
    fn hook_has_precedence_and_expires() {
        let detected = detect(
            &[manifest()],
            Some("codex"),
            "",
            "y/n",
            Duration::from_secs(10),
            Some(HookState {
                state: AgentStateKind::Done,
                source: "claude".into(),
                age: Duration::from_secs(4),
            }),
        );
        assert_eq!(detected.state, AgentStateKind::Done);
        let expired = detect(
            &[manifest()],
            Some("codex"),
            "",
            "y/n",
            Duration::from_secs(10),
            Some(HookState {
                state: AgentStateKind::Done,
                source: "claude".into(),
                age: Duration::from_secs(31),
            }),
        );
        assert_eq!(expired.state, AgentStateKind::Blocked);
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
