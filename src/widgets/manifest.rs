// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Extension package manifest format (`beyrix-ext.toml`).
//!
//! Supports Widget, Background, Effect, Provider, Theme, and Bundle packages
//! with SDK versioning, dependency declarations, and capability requests.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::layout::WidgetPlacementConfig;
use super::permissions::Capability;

/// Current BeyrixWidgets Public SDK version.
pub const SDK_VERSION: &str = "1.0.0";
pub const SDK_MAJOR: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExtensionKind {
    Widget,
    Background,
    Effect,
    Provider,
    Theme,
    Bundle,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionDependency {
    pub id: String,
    pub version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ExtensionManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub kind: ExtensionKind,
    pub sdk_version: String,
    pub entry: String,
    pub author: String,
    pub description: String,
    pub license: String,
    pub official: bool,
    pub permissions: Vec<Capability>,
    pub dependencies: Vec<ExtensionDependency>,
    pub settings: BTreeMap<String, toml::Value>,
    pub placement: WidgetPlacementConfig,
}

impl Default for ExtensionManifest {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            version: "0.1.0".to_string(),
            kind: ExtensionKind::Widget,
            sdk_version: SDK_VERSION.to_string(),
            entry: "index.js".to_string(),
            author: String::new(),
            description: String::new(),
            license: "MPL-2.0".to_string(),
            official: false,
            permissions: vec![Capability::Ui, Capability::Theme],
            dependencies: Vec::new(),
            settings: BTreeMap::new(),
            placement: WidgetPlacementConfig::default(),
        }
    }
}

impl ExtensionManifest {
    pub fn load_from_dir(dir: &Path) -> Result<LoadedExtension, String> {
        let toml_path = dir.join("beyrix-ext.toml");
        if !toml_path.is_file() {
            return Err(format!("missing {}", toml_path.display()));
        }
        let text = std::fs::read_to_string(&toml_path)
            .map_err(|e| format!("read {}: {e}", toml_path.display()))?;
        let manifest: Self =
            toml::from_str(&text).map_err(|e| format!("parse {}: {e}", toml_path.display()))?;
        manifest.validate(dir)?;
        let entry_path = dir.join(&manifest.entry);
        Ok(LoadedExtension {
            root: dir.to_path_buf(),
            entry_path,
            manifest,
        })
    }

    pub fn validate(&self, dir: &Path) -> Result<(), String> {
        if self.id.trim().is_empty()
            || !self
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        {
            return Err(format!("invalid extension id {:?}", self.id));
        }
        if self.name.trim().is_empty() {
            return Err("extension name cannot be empty".to_string());
        }
        if !is_sdk_compatible(&self.sdk_version) {
            return Err(format!(
                "incompatible sdk_version {:?}; host supports {}.x",
                self.sdk_version, SDK_MAJOR
            ));
        }
        let entry = Path::new(&self.entry);
        if entry.is_absolute()
            || entry
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(format!("unsafe entry path {:?}", self.entry));
        }
        let resolved = dir.join(entry);
        if !resolved.is_file() {
            return Err(format!("entry file not found: {}", resolved.display()));
        }
        Ok(())
    }
}

pub fn is_sdk_compatible(requested: &str) -> bool {
    let trimmed = requested
        .trim()
        .trim_start_matches('^')
        .trim_start_matches('~');
    let Some(major_str) = trimmed.split('.').next() else {
        return false;
    };
    let Ok(major) = major_str.parse::<u32>() else {
        return false;
    };
    major == SDK_MAJOR
}

#[derive(Clone, Debug)]
pub struct LoadedExtension {
    pub root: PathBuf,
    pub entry_path: PathBuf,
    pub manifest: ExtensionManifest,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_manifest_and_rejects_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.js"), "export default {}").unwrap();
        std::fs::write(
            dir.path().join("beyrix-ext.toml"),
            r#"
id = "org.example.clock"
name = "Clock"
version = "1.0.0"
kind = "widget"
sdk_version = "1.0.0"
entry = "index.js"
permissions = ["ui", "theme"]
"#,
        )
        .unwrap();
        let loaded = ExtensionManifest::load_from_dir(dir.path()).unwrap();
        assert_eq!(loaded.manifest.id, "org.example.clock");
        assert_eq!(loaded.manifest.kind, ExtensionKind::Widget);

        let mut bad = loaded.manifest.clone();
        bad.entry = "../escape.js".to_string();
        assert!(bad.validate(dir.path()).is_err());

        let mut incompatible = loaded.manifest;
        incompatible.sdk_version = "2.0.0".to_string();
        assert!(incompatible.validate(dir.path()).is_err());
    }
}
