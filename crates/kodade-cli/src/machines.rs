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
use std::collections::HashSet;

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
        Ok(text) => {
            let catalog: Catalog = toml::from_str(&text)?;
            catalog.validate()?;
            Ok(catalog)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Catalog::default()),
        Err(error) => Err(error.into()),
    }
}

pub fn save(catalog: &Catalog) -> Result<()> {
    catalog.validate()?;
    let path = path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    crate::atomic_file::write(&path, toml::to_string_pretty(catalog)?.as_bytes())
}

impl Catalog {
    pub fn validate(&self) -> Result<()> {
        let mut ids = HashSet::new();
        let mut labels = HashSet::new();
        for profile in &self.machines {
            if profile.id.is_empty()
                || profile.id.chars().any(char::is_control)
                || !ids.insert(&profile.id)
                || !labels.insert(&profile.label)
            {
                bail!("machine profiles need unique ids and labels");
            }
            validate_profile(&profile.label, &profile.target, profile.session.as_deref())?;
        }
        Ok(())
    }

    pub fn add(
        &mut self,
        label: String,
        target: String,
        session: Option<String>,
    ) -> Result<&MachineProfile> {
        validate_profile(&label, &target, session.as_deref())?;
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
            .find(|item| item.id == id || item.label == id)
            .ok_or_else(|| anyhow::anyhow!("machine '{id}' not found"))
    }
    pub fn remove(&mut self, id: &str) -> Result<MachineProfile> {
        let index = self
            .machines
            .iter()
            .position(|item| item.id == id || item.label == id)
            .ok_or_else(|| anyhow::anyhow!("machine '{id}' not found"))?;
        Ok(self.machines.remove(index))
    }
}

fn validate_profile(label: &str, target: &str, session: Option<&str>) -> Result<()> {
    if label.trim().is_empty() || label.chars().any(char::is_control) {
        bail!("machine label must be non-empty and contain no control characters");
    }
    crate::remote::validate_host(target)?;
    if let Some(session) = session {
        kodade_cli_daemon::validate_session_name(session)?;
    }
    Ok(())
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
