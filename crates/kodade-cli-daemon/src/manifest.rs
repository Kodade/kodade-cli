//! Agent detection manifest parsing and conservative screen matching.

use std::{collections::HashMap, fs, path::Path};

use anyhow::{Context, Result};
use kodade_cli_proto::AgentStateKind;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub display: String,
    #[serde(default)]
    pub process: Vec<String>,
    #[serde(default)]
    pub title: Vec<String>,
    /// Command that resumes the agent's last session, e.g. `codex resume --last`.
    #[serde(default)]
    pub resume: Option<String>,
    #[serde(default, rename = "rule")]
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Rule {
    pub state: ManifestState,
    pub any: Vec<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ManifestState {
    Blocked,
    Working,
    Done,
}

impl From<ManifestState> for AgentStateKind {
    fn from(state: ManifestState) -> Self {
        match state {
            ManifestState::Blocked => Self::Blocked,
            ManifestState::Working => Self::Working,
            ManifestState::Done => Self::Done,
        }
    }
}

impl Manifest {
    pub fn identifies(&self, process: Option<&str>, title: &str) -> bool {
        process.is_some_and(|name| self.process.iter().any(|item| item == name))
            || self.title.iter().any(|item| title.contains(item))
    }
}

pub fn matching_rule<'a>(manifest: &'a Manifest, screen: &str, lines: usize) -> Option<&'a Rule> {
    let bottom = screen
        .lines()
        .rev()
        .take(lines)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    manifest
        .rules
        .iter()
        .find(|rule| rule.any.iter().any(|needle| bottom.contains(needle)))
}

pub fn load() -> Result<Vec<Manifest>> {
    let mut manifests = builtin()?;
    if let Some(home) = dirs::home_dir() {
        let directory = home.join(".config/kodade-cli/agent-detection");
        if directory.exists() {
            for entry in fs::read_dir(&directory).context("read agent detection directory")? {
                let path = entry?.path();
                if path.extension().and_then(|item| item.to_str()) != Some("toml") {
                    continue;
                }
                let manifest = parse_file(&path)?;
                manifests.insert(manifest.name.clone(), manifest);
            }
        }
    }
    Ok(manifests.into_values().collect())
}

fn builtin() -> Result<HashMap<String, Manifest>> {
    [
        include_str!("../manifests/claude-code.toml"),
        include_str!("../manifests/codex.toml"),
        include_str!("../manifests/grok.toml"),
        include_str!("../manifests/opencode.toml"),
        include_str!("../manifests/gemini-cli.toml"),
        include_str!("../manifests/aider.toml"),
        include_str!("../manifests/cursor-agent.toml"),
        include_str!("../manifests/copilot.toml"),
        include_str!("../manifests/cline.toml"),
        include_str!("../manifests/amp.toml"),
        include_str!("../manifests/droid.toml"),
        include_str!("../manifests/kimi.toml"),
        include_str!("../manifests/qwen-code.toml"),
        include_str!("../manifests/pi.toml"),
        include_str!("../manifests/hermes.toml"),
    ]
    .into_iter()
    .map(|contents| toml::from_str::<Manifest>(contents).context("parse built-in agent manifest"))
    .map(|result| result.map(|manifest| (manifest.name.clone(), manifest)))
    .collect()
}

fn parse_file(path: &Path) -> Result<Manifest> {
    toml::from_str(&fs::read_to_string(path)?).with_context(|| format!("parse {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_matches_only_bottom_screen_lines() {
        let manifest: Manifest = toml::from_str(
            r#"name = "codex"
display = "Codex"
process = ["codex"]
[[rule]]
state = "blocked"
any = ["y/n"]
"#,
        )
        .unwrap();
        assert!(manifest.identifies(Some("codex"), ""));
        assert!(matching_rule(&manifest, "y/n\nold\nold", 2).is_none());
        assert_eq!(
            AgentStateKind::from(matching_rule(&manifest, "old\ny/n", 2).unwrap().state),
            AgentStateKind::Blocked
        );
    }
}
