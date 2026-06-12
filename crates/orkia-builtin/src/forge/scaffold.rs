// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;
use orkia_forge_types::{ForgeConfig, ForgeManifest};
use orkia_rfc_core::{RfcRecord, RfcStore};
use orkia_shell_types::{BuildOutcome, BuilderError, ForgeBuilder, UsageReport};
use sha2::{Digest, Sha256};

use super::placeholders;
use super::validate::{self, ValidatedForge};

/// The V0 scaffolder: validates the RFC, writes manifest + placeholder files.
///
/// V1: the trait is async to accommodate `RemoteBuilder`, but the scaffolder
/// itself stays synchronous internally (pure filesystem work). The async
/// shim is essentially zero-cost — `async fn` of a sync body becomes a
/// `Ready` future that resolves immediately.
pub struct ScaffoldBuilder;

impl ScaffoldBuilder {
    pub const VERSION: &'static str = "scaffold-0.1.0";
}

#[async_trait]
impl ForgeBuilder for ScaffoldBuilder {
    async fn build(
        &self,
        rfc: &RfcRecord,
        target_dir: &Path,
    ) -> Result<BuildOutcome, BuilderError> {
        let started = Instant::now();
        let validated = validate::validate(rfc)?;
        let raw_for_hash = render_rfc_for_hash(rfc);
        let rfc_hash = sha256_hex(raw_for_hash.as_bytes());

        let manifest = ForgeManifest {
            forge: ForgeConfig {
                name: validated.name.clone(),
                description: validated.description.clone(),
                version: "0.1.0".into(),
                api_version: orkia_forge_types::default_api_version(),
                rfc_id: rfc.fm.id.as_str().to_string(),
                rfc_hash: format!("sha256:{rfc_hash}"),
                created_at: Utc::now(),
                icon: validated.icon.clone(),
                window: validated.window.clone(),
                permissions: validated.permissions.clone(),
            },
        };

        let mut files = write_scaffold(&validated, target_dir, &manifest)?;
        if let Some(agent) = rfc.fm.forge.as_ref().and_then(|f| f.agent.as_ref()) {
            let agent_files = write_agent_scaffold(target_dir, agent)?;
            files.extend(agent_files);
        }

        Ok(BuildOutcome {
            manifest,
            files_written: files,
            duration: started.elapsed(),
            builder_version: Self::VERSION.into(),
        })
    }

    async fn usage(&self) -> Result<UsageReport, BuilderError> {
        Err(BuilderError::Unavailable {
            reason: "Offline scaffold builder does not track usage.".into(),
        })
    }
}

/// `~/.orkia/forge` — the root for every app's directory.
pub fn default_app_root() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".orkia").join("forge")
}

pub fn scaffold_dir_for(root: &Path, app_name: &str) -> PathBuf {
    root.join(app_name)
}

/// Higher-level entry that the `rfc forge` CLI uses: looks up the RFC by id
/// inside the given project store and builds into `<root>/<forge.name>/`.
///
/// V1 made this async to match the trait. The scaffolder is sync internally;
/// the await on `ScaffoldBuilder.build` is essentially free.
pub async fn build_from_path(
    project_dir: &Path,
    rfc_id: &str,
    root: &Path,
    force: bool,
) -> Result<(PathBuf, BuildOutcome), BuilderError> {
    let store = RfcStore::new(project_dir);
    let rfc_id_typed = orkia_rfc_core::RfcId::new(rfc_id);
    let rfc = store
        .load(&rfc_id_typed)
        .map_err(|e| BuilderError::InvalidRfc(format!("RFC `{rfc_id}` not found: {e}")))?;

    // Peek at the forge name before building so we can apply --force.
    // Validate BEFORE any filesystem operation (SEC-028): reject traversal
    // names (`../foo`, `/etc/...`) before joining or removing a directory.
    let validated_name = rfc
        .fm
        .forge
        .as_ref()
        .map(|f| f.name.clone())
        .ok_or_else(|| BuilderError::InvalidRfc("[forge] block missing".into()))?;
    orkia_forge_types::validate_app_name(&validated_name)
        .map_err(|e| BuilderError::InvalidRfc(format!("forge.name invalid: {e}")))?;

    let app_dir = scaffold_dir_for(root, &validated_name);
    if app_dir.exists() {
        if !force {
            return Err(BuilderError::AppExists {
                name: validated_name,
            });
        }
        fs::remove_dir_all(&app_dir).map_err(BuilderError::Io)?;
    }

    let outcome = ScaffoldBuilder.build(&rfc, &app_dir).await?;
    Ok((app_dir, outcome))
}

fn write_scaffold(
    v: &ValidatedForge,
    dir: &Path,
    manifest: &ForgeManifest,
) -> Result<Vec<PathBuf>, BuilderError> {
    fs::create_dir_all(dir).map_err(BuilderError::Io)?;
    fs::create_dir_all(dir.join("data")).map_err(BuilderError::Io)?;
    fs::create_dir_all(dir.join("seal")).map_err(BuilderError::Io)?;

    let manifest_path = dir.join("manifest.toml");
    manifest
        .save(&manifest_path)
        .map_err(|e| BuilderError::Manifest(e.to_string()))?;

    let html_path = dir.join("app.html");
    fs::write(&html_path, placeholders::app_html(v)).map_err(BuilderError::Io)?;
    let css_path = dir.join("app.css");
    fs::write(&css_path, placeholders::app_css()).map_err(BuilderError::Io)?;
    let js_path = dir.join("app.js");
    fs::write(&js_path, placeholders::app_js()).map_err(BuilderError::Io)?;
    let icon_path = dir.join("icon.png");
    fs::write(&icon_path, placeholders::default_icon_png()).map_err(BuilderError::Io)?;
    let seal_log = dir.join("seal").join("events.jsonl");
    fs::write(&seal_log, b"").map_err(BuilderError::Io)?;

    Ok(vec![
        manifest_path,
        html_path,
        css_path,
        js_path,
        icon_path,
        seal_log,
    ])
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

///  - `<app-dir>/agent/archetype.toml` — the agent block, copied verbatim.
///  - `<app-dir>/agent/system-prompt.md` — the system prompt as a
///    separate file (easier to edit later).
///  - `<app-dir>/agent/memory.md` — V2 stub (empty).
fn write_agent_scaffold(
    dir: &Path,
    agent: &orkia_rfc_core::frontmatter::ForgeAgentBlock,
) -> Result<Vec<PathBuf>, BuilderError> {
    let agent_dir = dir.join("agent");
    fs::create_dir_all(&agent_dir).map_err(BuilderError::Io)?;

    let arch_path = agent_dir.join("archetype.toml");
    // Wrap the block in `[agent]` for canonical TOML shape.
    #[derive(serde::Serialize)]
    struct Wrap<'a> {
        agent: &'a orkia_rfc_core::frontmatter::ForgeAgentBlock,
    }
    let toml_body = toml::to_string_pretty(&Wrap { agent })
        .map_err(|e| BuilderError::Manifest(format!("agent toml: {e}")))?;
    fs::write(&arch_path, toml_body).map_err(BuilderError::Io)?;

    let prompt_path = agent_dir.join("system-prompt.md");
    fs::write(&prompt_path, &agent.system_prompt).map_err(BuilderError::Io)?;

    let memory_path = agent_dir.join("memory.md");
    fs::write(&memory_path, b"").map_err(BuilderError::Io)?;

    Ok(vec![arch_path, prompt_path, memory_path])
}

/// Stable representation of the RFC used for content hashing. The on-disk
/// file would include mutable fields like `updated_at` that change on every
/// edit, which defeats the purpose of "lock the app to an RFC version" — so
/// we hash only the body + the structural frontmatter pieces.
fn render_rfc_for_hash(rfc: &RfcRecord) -> String {
    let mut s = String::new();
    s.push_str("id=");
    s.push_str(rfc.fm.id.as_str());
    s.push('\n');
    s.push_str("version=");
    s.push_str(&rfc.fm.version.to_string());
    s.push('\n');
    if let Some(kind) = &rfc.fm.kind {
        s.push_str("kind=");
        s.push_str(kind);
        s.push('\n');
    }
    if let Some(forge) = &rfc.fm.forge {
        // toml::to_string is deterministic for this shape.
        if let Ok(t) = toml::to_string(forge) {
            s.push_str("[forge]\n");
            s.push_str(&t);
        }
    }
    s.push_str("---body---\n");
    s.push_str(&rfc.body);
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;
    use chrono::TimeZone;
    use orkia_rfc_core::frontmatter::{ForgeFrontmatterBlock, ForgePermissions, ForgeWindow};
    use orkia_rfc_core::{ContentHash, RfcFrontmatter, RfcId, RfcState};
    use tempfile::TempDir;

    fn make_rfc() -> RfcRecord {
        let ts = FixedOffset::east_opt(0)
            .unwrap()
            .with_ymd_and_hms(2026, 5, 22, 14, 0, 0)
            .unwrap();
        RfcRecord {
            fm: RfcFrontmatter {
                id: RfcId::new("hello-orkia"),
                state: RfcState::DraftEmpty,
                version: 1,
                created_at: ts,
                updated_at: ts,
                content_hash: ContentHash("sha256:0".into()),
                agents: vec![],
                locked_by: None,
                locked_at: None,
                title: Some("Hello".into()),
                status: None,
                assigned: None,
                kind: Some("forge-app".into()),
                forge: Some(ForgeFrontmatterBlock {
                    name: "hello-orkia".into(),
                    description: "demo".into(),
                    icon: None,
                    window: ForgeWindow {
                        title: "Hello".into(),
                        width: 480,
                        height: 320,
                        resizable: true,
                    },
                    permissions: ForgePermissions::default(),
                    agent: None,
                }),
                scope: None,
                operator: None,
            },
            body: "# Hello\n".into(),
        }
    }

    #[tokio::test]
    async fn builds_scaffold_with_expected_files() {
        let tmp = TempDir::new().unwrap();
        let rfc = make_rfc();
        let out = ScaffoldBuilder
            .build(&rfc, tmp.path())
            .await
            .expect("build");
        assert_eq!(out.builder_version, "scaffold-0.1.0");
        for p in ["manifest.toml", "app.html", "app.css", "app.js", "icon.png"] {
            assert!(tmp.path().join(p).exists(), "missing {p}");
        }
        assert!(tmp.path().join("data").is_dir());
        assert!(tmp.path().join("seal").is_dir());
        assert!(tmp.path().join("seal").join("events.jsonl").exists());

        let loaded = ForgeManifest::load(&tmp.path().join("manifest.toml")).unwrap();
        assert_eq!(loaded.forge.name, "hello-orkia");
        assert!(loaded.forge.rfc_hash.starts_with("sha256:"));
        assert_eq!(loaded.forge.api_version, 1);
    }

    #[tokio::test]
    async fn build_rejects_non_forge_kind() {
        let tmp = TempDir::new().unwrap();
        let mut rfc = make_rfc();
        rfc.fm.kind = Some("task".into());
        let err = ScaffoldBuilder.build(&rfc, tmp.path()).await.unwrap_err();
        assert!(matches!(err, BuilderError::InvalidRfc(_)));
        // Scaffolder must not leak files on failure.
        assert!(!tmp.path().join("manifest.toml").exists());
    }

    #[test]
    fn hash_stable_across_updated_at() {
        let mut rfc = make_rfc();
        let h1 = sha256_hex(render_rfc_for_hash(&rfc).as_bytes());
        rfc.fm.updated_at += chrono::Duration::hours(1);
        let h2 = sha256_hex(render_rfc_for_hash(&rfc).as_bytes());
        assert_eq!(h1, h2, "hash must ignore updated_at");
    }

    #[test]
    fn hash_changes_with_forge_block() {
        let mut rfc = make_rfc();
        let h1 = sha256_hex(render_rfc_for_hash(&rfc).as_bytes());
        if let Some(f) = rfc.fm.forge.as_mut() {
            f.window.title = "Different".into();
        }
        let h2 = sha256_hex(render_rfc_for_hash(&rfc).as_bytes());
        assert_ne!(h1, h2);
    }
}
