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
    /// Display identity from a recognized Ködade lifecycle adapter.
    pub agent: Option<String>,
    /// Foreground process evidence captured when the hook arrived. Hook
    /// identity cannot survive a process replacement.
    pub process_pid: Option<i32>,
    pub process_name: Option<String>,
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
    /// True when a live hook report drove the state. Notifications (#10) treat a
    /// hook as proof an agent is present even without a manifest match.
    pub from_hook: bool,
    /// True only when the hook, rather than a manifest, supplied the identity.
    pub identity_from_hook: bool,
}

pub fn detect(
    manifests: &[Manifest],
    process: Option<&str>,
    process_pid: Option<i32>,
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
        // Adapter callbacks commonly run under Node/Python instead of the
        // agent executable. Accept their identity only for the same observed
        // process, never for a later arbitrary foreground program.
        let hook_agent = (agent.is_none() && hook_matches_wrapper(process, process_pid, &hook))
            .then(|| hook.agent.clone())
            .flatten();
        let identity_from_hook = hook_agent.is_some();
        let agent = agent.or(hook_agent);
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
            from_hook: true,
            identity_from_hook,
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
            from_hook: false,
            identity_from_hook: false,
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
            from_hook: false,
            identity_from_hook: false,
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
        from_hook: false,
        identity_from_hook: false,
    }
}

pub(crate) fn is_shell(process: &str) -> bool {
    matches!(process, "sh" | "bash" | "zsh" | "fish" | "nu")
}

fn hook_matches_wrapper(process: Option<&str>, process_pid: Option<i32>, hook: &HookState) -> bool {
    let Some(process) = process else {
        return false;
    };
    // Missing process evidence must fail closed. In particular, never let the
    // spawn command stand in for a later foreground process.
    let (Some(process_pid), Some(hook_pid)) = (process_pid, hook.process_pid) else {
        return false;
    };
    matches!(process, "node" | "nodejs" | "python" | "python3")
        && hook.process_name.as_deref() == Some(process)
        && hook_pid == process_pid
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
            resume: Some("codex resume".into()),
            rules: vec![Rule {
                state: ManifestState::Blocked,
                any: vec!["y/n".into()],
            }],
            source: crate::manifest::ManifestSource::Builtin,
        }
    }

    #[test]
    fn hook_has_precedence_and_working_expires() {
        let detected = detect(
            &[manifest()],
            Some("codex"),
            None,
            "",
            "y/n",
            Duration::from_secs(10),
            Some(HookState {
                state: AgentStateKind::Working,
                source: "claude".into(),
                agent: None,
                process_pid: None,
                process_name: None,
                age: Duration::from_secs(4),
                output_since_report: false,
            }),
        );
        assert_eq!(detected.state, AgentStateKind::Working);
        // A working/blocked/idle report older than the TTL decays to the screen rule.
        let expired = detect(
            &[manifest()],
            Some("codex"),
            None,
            "",
            "y/n",
            Duration::from_secs(10),
            Some(HookState {
                state: AgentStateKind::Working,
                source: "claude".into(),
                agent: None,
                process_pid: None,
                process_name: None,
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
            None,
            "",
            "y/n",
            Duration::from_secs(10),
            Some(HookState {
                state: AgentStateKind::Done,
                source: "claude".into(),
                agent: None,
                process_pid: None,
                process_name: None,
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
            None,
            "",
            "y/n",
            Duration::from_secs(10),
            Some(HookState {
                state: AgentStateKind::Done,
                source: "claude".into(),
                agent: None,
                process_pid: None,
                process_name: None,
                age: Duration::from_secs(2),
                output_since_report: true,
            }),
        );
        assert_eq!(released.state, AgentStateKind::Blocked);
    }

    #[test]
    fn recognized_hook_identifies_a_wrapper_but_not_its_exit_shell() {
        let hook = HookState {
            state: AgentStateKind::Working,
            source: "kodade:pi".into(),
            agent: Some("Pi".into()),
            process_pid: Some(42),
            process_name: Some("node".into()),
            age: Duration::ZERO,
            output_since_report: false,
        };
        assert_eq!(
            detect(
                &[],
                Some("node"),
                Some(42),
                "",
                "",
                Duration::ZERO,
                Some(hook.clone())
            )
            .agent
            .as_deref(),
            Some("Pi")
        );
        assert_eq!(
            detect(
                &[],
                Some("sh"),
                Some(42),
                "",
                "",
                Duration::ZERO,
                Some(hook)
            )
            .agent,
            None
        );
    }

    #[test]
    fn hook_identity_rejects_a_different_or_unrecognized_process() {
        let hook = HookState {
            state: AgentStateKind::Done,
            source: "kodade:pi".into(),
            agent: Some("Pi".into()),
            process_pid: Some(42),
            process_name: Some("node".into()),
            age: Duration::ZERO,
            output_since_report: false,
        };
        assert!(detect(
            &[],
            Some("sleep"),
            Some(99),
            "",
            "",
            Duration::ZERO,
            Some(hook.clone())
        )
        .agent
        .is_none());
        assert!(detect(
            &[],
            Some("node"),
            None,
            "",
            "",
            Duration::ZERO,
            Some(hook.clone())
        )
        .agent
        .is_none());
        // A replacement Node process is still unrelated: the PID captured by
        // the adapter callback is part of the hook identity.
        assert!(detect(
            &[],
            Some("node"),
            Some(43),
            "",
            "",
            Duration::ZERO,
            Some(hook.clone())
        )
        .agent
        .is_none());
        let mut replacement = hook;
        replacement.process_name = Some("nodejs".into());
        assert!(detect(
            &[],
            Some("node"),
            Some(42),
            "",
            "",
            Duration::ZERO,
            Some(replacement)
        )
        .agent
        .is_none());
    }

    #[test]
    fn title_identifies_an_agent_started_through_a_shell() {
        let mut titled = manifest();
        titled.title = vec!["Codex".into()];
        assert_eq!(
            detect(
                &[titled],
                Some("sh"),
                None,
                "Codex",
                "",
                Duration::ZERO,
                None,
            )
            .agent,
            Some("Codex".into())
        );
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
            // Some manifests are process-only: a short title substring (< 5 chars,
            // e.g. "Amp"/"Pi") would false-positive, so title is intentionally absent.
            if !manifest.title.is_empty() {
                assert!(
                    manifest.identifies(None, title),
                    "{process} identified by title substring"
                );
            }
            // A sourced sample of routine agent output: no attention prompt present.
            let screen = "Working on your request...\nreading files\nediting src/main.rs\n";
            let detected = detect(
                std::slice::from_ref(&manifest),
                Some(process),
                None,
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
            None,
            "",
            "continue? y/n",
            Duration::ZERO,
            None,
        );
        assert_eq!(rule.state, AgentStateKind::Blocked);
        let unknown = detect(
            &[manifest()],
            Some("vim"),
            None,
            "",
            "",
            Duration::from_secs(3),
            None,
        );
        assert_eq!(unknown.state, AgentStateKind::Unknown);
    }
}
