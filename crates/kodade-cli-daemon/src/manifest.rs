//! Agent detection manifest parsing and conservative screen matching.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
};

use anyhow::{bail, Context, Result};
use kodade_cli_proto::AgentStateKind;
use serde::Deserialize;

/// Where a loaded manifest came from. User overrides replace a bundled manifest
/// with the same name, but never mutate the copy compiled into the binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ManifestSource {
    #[default]
    Builtin,
    UserOverride,
}

impl ManifestSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::UserOverride => "user override",
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub display: String,
    #[serde(default)]
    pub process: Vec<String>,
    #[serde(default)]
    pub title: Vec<String>,
    /// Metadata command that opens an agent's resume surface, e.g. `codex resume`.
    #[serde(default)]
    pub resume: Option<String>,
    #[serde(default, rename = "rule")]
    pub rules: Vec<Rule>,
    #[serde(skip)]
    pub source: ManifestSource,
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

/// Reject rules which parse as TOML but can never safely identify an agent.
/// This is shared by startup, reload, and downloaded-manifest validation.
pub fn validate(manifest: &Manifest) -> Result<()> {
    if manifest.name.trim().is_empty() || manifest.display.trim().is_empty() {
        bail!("agent manifest needs a name and display label");
    }
    if manifest
        .rules
        .iter()
        .any(|rule| rule.any.is_empty() || rule.any.iter().any(|needle| needle.trim().is_empty()))
    {
        bail!("agent manifest rules need nonempty match text");
    }
    Ok(())
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
    let directory = dirs::home_dir().map(|home| home.join(".config/kodade-cli/agent-detection"));
    load_from(directory.as_deref())
}

/// Load bundled manifests plus an optional directory of user overrides. This
/// seam makes reload all-or-nothing and lets tests prove overrides without
/// changing the process home directory.
pub fn load_from(directory: Option<&Path>) -> Result<Vec<Manifest>> {
    let mut manifests = builtin()?;
    if let Some(directory) = directory {
        if directory.exists() {
            let mut overrides = HashSet::new();
            for entry in fs::read_dir(directory).context("read agent detection directory")? {
                let path = entry?.path();
                if path.extension().and_then(|item| item.to_str()) != Some("toml") {
                    continue;
                }
                let mut manifest = parse_file(&path)?;
                manifest.source = ManifestSource::UserOverride;
                if !overrides.insert(manifest.name.clone()) {
                    bail!("duplicate agent manifest override name: {}", manifest.name);
                }
                manifests.insert(manifest.name.clone(), manifest);
            }
        }
    }
    let mut manifests: Vec<_> = manifests.into_values().collect();
    manifests.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(manifests)
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
    .map(|result| {
        result.and_then(|mut manifest| {
            validate(&manifest)?;
            manifest.source = ManifestSource::Builtin;
            Ok((manifest.name.clone(), manifest))
        })
    })
    .collect()
}

fn parse_file(path: &Path) -> Result<Manifest> {
    let manifest: Manifest = toml::from_str(&fs::read_to_string(path)?)
        .with_context(|| format!("parse {}", path.display()))?;
    validate(&manifest).with_context(|| format!("validate {}", path.display()))?;
    Ok(manifest)
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

    #[test]
    fn user_override_replaces_builtin_and_invalid_reload_is_rejected() {
        let directory = std::env::temp_dir().join(format!(
            "kodade-cli-manifests-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("codex.toml"),
            "name = 'codex'\ndisplay = 'Custom Codex'\nprocess = ['custom-codex']\n",
        )
        .unwrap();
        let manifests = load_from(Some(&directory)).unwrap();
        let codex = manifests
            .iter()
            .find(|manifest| manifest.name == "codex")
            .unwrap();
        assert_eq!(codex.display, "Custom Codex");
        assert_eq!(codex.source, ManifestSource::UserOverride);

        fs::write(directory.join("broken.toml"), "not valid = [").unwrap();
        assert!(load_from(Some(&directory)).is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn duplicate_override_names_are_rejected_before_replacing_the_live_set() {
        let directory = std::env::temp_dir().join(format!(
            "kodade-cli-duplicate-manifests-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let contents = "name = 'same'\ndisplay = 'Same'\nprocess = ['same']\n";
        fs::write(directory.join("one.toml"), contents).unwrap();
        fs::write(directory.join("two.toml"), contents).unwrap();
        assert!(load_from(Some(&directory)).is_err());
        fs::remove_dir_all(directory).unwrap();
    }
}
