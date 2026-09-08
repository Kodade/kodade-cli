//! Saved SSH machine profiles. The catalog is local client state only: OpenSSH
//! remains responsible for credentials, host keys, and connection multiplexing.

use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::config;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MachineProfile {
    pub id: String,
    pub label: String,
    pub target: String,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default = "enabled")]
    pub enabled: bool,
}

fn enabled() -> bool {
    true
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Catalog {
    #[serde(default)]
    pub machines: Vec<MachineProfile>,
}

pub fn path() -> PathBuf {
    config::state_path().with_file_name("machines.toml")
}

pub fn load() -> Result<Catalog> {
    match fs::read_to_string(path()) {
        Ok(text) => Ok(toml::from_str(&text)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Catalog::default()),
        Err(error) => Err(error.into()),
    }
}

pub fn save(catalog: &Catalog) -> Result<()> {
    let path = path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&temporary, toml::to_string_pretty(catalog)?)?;
    fs::rename(temporary, path)?;
    Ok(())
}

impl Catalog {
    pub fn add(
        &mut self,
        label: String,
        target: String,
        session: Option<String>,
    ) -> Result<&MachineProfile> {
        if label.trim().is_empty()
            || target.trim().is_empty()
            || target.starts_with('-')
            || target.contains(char::is_whitespace)
        {
            bail!("machine label and SSH target must be non-empty, safe words")
        }
        if self.machines.iter().any(|item| item.label == label) {
            bail!("machine label '{label}' already exists")
        }
        let id = format!(
            "m-{:x}",
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        );
        self.machines.push(MachineProfile {
            id,
            label,
            target,
            session,
            enabled: true,
        });
        Ok(self.machines.last().expect("just pushed"))
    }
    pub fn get_mut(&mut self, id: &str) -> Result<&mut MachineProfile> {
        self.machines
            .iter_mut()
            .find(|item| item.id == id)
            .ok_or_else(|| anyhow::anyhow!("machine '{id}' not found"))
    }
    pub fn remove(&mut self, id: &str) -> Result<MachineProfile> {
        let index = self
            .machines
            .iter()
            .position(|item| item.id == id)
            .ok_or_else(|| anyhow::anyhow!("machine '{id}' not found"))?;
        Ok(self.machines.remove(index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_crud_keeps_remote_session_and_enabled_state() {
        let mut c = Catalog::default();
        let id = c
            .add("Build".into(), "buildbox".into(), Some("agents".into()))
            .unwrap()
            .id
            .clone();
        c.get_mut(&id).unwrap().enabled = false;
        assert_eq!(c.remove(&id).unwrap().session.as_deref(), Some("agents"));
    }
    #[test]
    fn unsafe_targets_are_rejected() {
        assert!(Catalog::default()
            .add("x".into(), "-o bad".into(), None)
            .is_err());
    }
}
