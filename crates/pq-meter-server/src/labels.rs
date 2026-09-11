//! User labels and the learned identities they belong to, saved atomically on rename.

use std::{collections::BTreeMap, path::PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::decision::StoredAppliance;

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct DeviceLabels {
    pub names: BTreeMap<String, String>,
    pub appliances: Vec<StoredAppliance>,
    #[serde(skip)]
    path: Option<PathBuf>,
}

impl DeviceLabels {
    pub fn load(path: PathBuf) -> anyhow::Result<Self> {
        let mut labels = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("invalid device labels file")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => return Err(error).context("reading device labels"),
        };
        labels.path = Some(path);
        Ok(labels)
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = self
            .path
            .as_ref()
            .context("device label storage is not configured")?;
        let parent = path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        std::fs::create_dir_all(parent).context("creating device labels directory")?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer_pretty(&mut file, self)?;
        file.as_file().sync_all()?;
        file.persist(path).context("saving device labels")?;
        Ok(())
    }
}

#[derive(Debug)]
pub enum RenameError {
    InvalidName,
    NotFound,
    Unavailable,
}
