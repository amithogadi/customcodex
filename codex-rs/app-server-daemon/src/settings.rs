use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Map;
use serde_json::Value;
use tokio::fs;

pub(crate) const DEFAULT_SHUTDOWN_GRACE_SECONDS: u32 = 60;
pub(crate) const MAX_SHUTDOWN_GRACE_SECONDS: u32 = 5 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DaemonSettings {
    pub(crate) feature_overrides: BTreeMap<String, bool>,
    pub(crate) shutdown_grace_seconds: u32,
}

impl Default for DaemonSettings {
    fn default() -> Self {
        Self {
            feature_overrides: BTreeMap::new(),
            shutdown_grace_seconds: DEFAULT_SHUTDOWN_GRACE_SECONDS,
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StopSettings {
    shutdown_grace_seconds: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredSettings {
    #[serde(default)]
    feature_overrides: BTreeMap<String, bool>,
    #[serde(default = "default_shutdown_grace_seconds")]
    shutdown_grace_seconds: u32,
}

impl Default for StoredSettings {
    fn default() -> Self {
        Self {
            feature_overrides: BTreeMap::new(),
            shutdown_grace_seconds: DEFAULT_SHUTDOWN_GRACE_SECONDS,
        }
    }
}

fn default_shutdown_grace_seconds() -> u32 {
    DEFAULT_SHUTDOWN_GRACE_SECONDS
}

fn validate_shutdown_grace(seconds: u32) -> Result<()> {
    ensure!(
        seconds <= MAX_SHUTDOWN_GRACE_SECONDS,
        "shutdown grace must be between 0 and {MAX_SHUTDOWN_GRACE_SECONDS} seconds"
    );
    Ok(())
}

impl DaemonSettings {
    pub(crate) async fn load(path: &Path) -> Result<Self> {
        let settings: StoredSettings = read_settings(path).await?;
        validate_shutdown_grace(settings.shutdown_grace_seconds)?;
        Ok(Self {
            feature_overrides: settings.feature_overrides,
            shutdown_grace_seconds: settings.shutdown_grace_seconds,
        })
    }

    pub(crate) async fn load_for_stop(path: &Path) -> Self {
        // Stop must work even when settings are unreadable or partially edited.
        let shutdown_grace_seconds = read_settings::<StopSettings>(path)
            .await
            .ok()
            .and_then(|settings| settings.shutdown_grace_seconds)
            .filter(|&seconds| seconds <= MAX_SHUTDOWN_GRACE_SECONDS)
            .unwrap_or(DEFAULT_SHUTDOWN_GRACE_SECONDS);
        Self {
            shutdown_grace_seconds,
            ..Self::default()
        }
    }

    pub(crate) async fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await.with_context(|| {
                format!(
                    "failed to create daemon settings directory {}",
                    parent.display()
                )
            })?;
        }
        let mut settings: Map<String, Value> = read_settings(path).await?;
        settings.remove("remoteControlEnabled");
        settings.remove("updater");
        if self.feature_overrides.is_empty() {
            settings.remove("featureOverrides");
        } else {
            settings.insert(
                "featureOverrides".to_string(),
                serde_json::to_value(&self.feature_overrides)?,
            );
        }
        let contents =
            serde_json::to_vec_pretty(&settings).context("failed to serialize settings")?;
        let temporary_path = path.with_extension("tmp");
        fs::write(&temporary_path, contents)
            .await
            .with_context(|| {
                format!(
                    "failed to write daemon settings {}",
                    temporary_path.display()
                )
            })?;
        fs::rename(&temporary_path, path)
            .await
            .with_context(|| format!("failed to replace daemon settings {}", path.display()))
    }
}

async fn read_settings<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    let contents = match fs::read_to_string(path).await {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(T::default()),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to read settings {}", path.display()));
        }
    };
    serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse settings {}", path.display()))
}

#[cfg(test)]
#[path = "settings_tests.rs"]
mod tests;
