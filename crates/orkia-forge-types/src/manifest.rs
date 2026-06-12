// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// V0 ships api_version = 1. Bumped only on breaking manifest changes.
pub const fn default_api_version() -> u32 {
    1
}

fn default_version_str() -> String {
    "0.1.0".into()
}

fn default_width() -> u32 {
    480
}

fn default_height() -> u32 {
    320
}

const fn default_true() -> bool {
    true
}

fn default_icon() -> String {
    "default".into()
}

/// Top-level shape of `manifest.toml`: a single `[forge]` table. We keep the
/// outer struct rather than collapsing it because future versions may add
/// sibling tables (`[forge.agent]`, `[forge.network]`, …).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForgeManifest {
    pub forge: ForgeConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForgeConfig {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_version_str")]
    pub version: String,
    #[serde(default = "default_api_version")]
    pub api_version: u32,
    pub rfc_id: String,
    pub rfc_hash: String,
    pub created_at: DateTime<Utc>,
    #[serde(default = "default_icon")]
    pub icon: String,
    pub window: WindowConfig,
    #[serde(default)]
    pub permissions: Permissions,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WindowConfig {
    pub title: String,
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    #[serde(default = "default_true")]
    pub resizable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Permissions {
    #[serde(default = "default_true")]
    pub storage: bool,
    #[serde(default)]
    pub agent: bool,
    #[serde(default)]
    pub network: Vec<String>,
    #[serde(default)]
    pub notification: bool,
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            storage: true,
            agent: false,
            network: Vec::new(),
            notification: false,
        }
    }
}

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("manifest io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("manifest toml parse: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("manifest toml serialize: {0}")]
    Serialize(#[from] toml::ser::Error),
}

impl ForgeManifest {
    pub fn load(path: &Path) -> Result<Self, ManifestError> {
        let raw = std::fs::read_to_string(path)?;
        let parsed = toml::from_str(&raw)?;
        Ok(parsed)
    }

    pub fn save(&self, path: &Path) -> Result<(), ManifestError> {
        let toml = toml::to_string_pretty(self)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, toml)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn fixture() -> ForgeManifest {
        ForgeManifest {
            forge: ForgeConfig {
                name: "hello-orkia".into(),
                description: "demo".into(),
                version: "0.1.0".into(),
                api_version: 1,
                rfc_id: "hello-orkia".into(),
                rfc_hash: "sha256:abc".into(),
                created_at: Utc.with_ymd_and_hms(2026, 5, 22, 14, 0, 0).unwrap(),
                icon: "default".into(),
                window: WindowConfig {
                    title: "Hello".into(),
                    width: 480,
                    height: 320,
                    resizable: true,
                },
                permissions: Permissions::default(),
            },
        }
    }

    #[test]
    fn round_trip_toml() {
        let m = fixture();
        let s = toml::to_string_pretty(&m).unwrap();
        let parsed: ForgeManifest = toml::from_str(&s).unwrap();
        assert_eq!(m, parsed);
    }

    #[test]
    fn permission_defaults() {
        let p = Permissions::default();
        assert!(p.storage);
        assert!(!p.agent);
        assert!(p.network.is_empty());
        assert!(!p.notification);
    }

    #[test]
    fn window_defaults_apply_on_missing_fields() {
        let s = r#"
            title = "x"
        "#;
        let w: WindowConfig = toml::from_str(s).unwrap();
        assert_eq!(w.width, 480);
        assert_eq!(w.height, 320);
        assert!(w.resizable);
    }
}
