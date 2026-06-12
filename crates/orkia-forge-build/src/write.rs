// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Persist the build response to disk under a project directory.
//!
//! Lays out:
//!
//! ```text
//! <dir>/
//!   app.html        # generated
//!   app.css         # generated
//!   app.js          # generated
//!   icon.svg        # generated
//!   manifest.toml   # merged
//!   data/           # preserved (rerun-safe)
//!   seal/
//!     events.jsonl  # created empty if absent
//!   build/
//!     response.json # full server response for audit
//!     rfc.txt       # exact RFC bytes used for the build
//! ```

use std::path::{Path, PathBuf};

use orkia_shell_types::BuilderError;

use crate::manifest::build_manifest;
use crate::response::BuildResponse;

/// Write the generated app to disk: manifest.toml, app.html,
/// app.css, app.js, icon.svg, plus `build/response.json` for
/// traceability. The existing `data/` and `seal/` dirs are
/// preserved (rerun-safe).
pub fn write_build(
    dir: &Path,
    forge_block: &orkia_rfc_core::frontmatter::ForgeFrontmatterBlock,
    rfc_content: &str,
    resp: &BuildResponse,
    rfc_hash: &str,
) -> Result<Vec<PathBuf>, BuilderError> {
    std::fs::create_dir_all(dir).map_err(BuilderError::Io)?;
    std::fs::create_dir_all(dir.join("data")).map_err(BuilderError::Io)?;
    std::fs::create_dir_all(dir.join("seal")).map_err(BuilderError::Io)?;
    std::fs::create_dir_all(dir.join("build")).map_err(BuilderError::Io)?;
    let seal_log = dir.join("seal").join("events.jsonl");
    if !seal_log.exists() {
        std::fs::write(&seal_log, b"").map_err(BuilderError::Io)?;
    }

    let mut files = Vec::new();
    let html_path = dir.join("app.html");
    std::fs::write(&html_path, &resp.files.html).map_err(BuilderError::Io)?;
    files.push(html_path);

    let css_path = dir.join("app.css");
    std::fs::write(&css_path, &resp.files.css).map_err(BuilderError::Io)?;
    files.push(css_path);

    let js_path = dir.join("app.js");
    std::fs::write(&js_path, &resp.files.js).map_err(BuilderError::Io)?;
    files.push(js_path);

    let icon_path = dir.join("icon.svg");
    std::fs::write(&icon_path, &resp.files.icon_svg).map_err(BuilderError::Io)?;
    files.push(icon_path);

    // build/response.json — full server response for audit + V1
    // `--rerun` to know what the previous build emitted.
    let resp_path = dir.join("build").join("response.json");
    let resp_json = serde_json::json!({
        "build_id": resp.build_id,
        "builder_version": resp.builder_version,
        "model": resp.model,
        "manifest_overrides": resp.manifest_overrides,
        "usage": {
            "input_tokens": resp.usage.input_tokens,
            "output_tokens": resp.usage.output_tokens,
            "retries": resp.usage.retries,
            "duration_ms": resp.usage.duration_ms,
        },
    });
    std::fs::write(
        &resp_path,
        serde_json::to_vec_pretty(&resp_json).map_err(|e| BuilderError::Manifest(e.to_string()))?,
    )
    .map_err(BuilderError::Io)?;
    files.push(resp_path);

    // build/rfc.txt — preserves the exact RFC bytes used for the
    // build, so `--rerun` can hash and compare against the new RFC.
    let rfc_snapshot = dir.join("build").join("rfc.txt");
    std::fs::write(&rfc_snapshot, rfc_content).map_err(BuilderError::Io)?;
    files.push(rfc_snapshot);

    // Manifest. Merge manifest_overrides from the server into the
    // base shape derived from the RFC's [forge] block.
    let manifest = build_manifest(forge_block, resp, rfc_hash)?;
    let manifest_path = dir.join("manifest.toml");
    manifest
        .save(&manifest_path)
        .map_err(|e| BuilderError::Manifest(e.to_string()))?;
    files.push(manifest_path);

    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use orkia_forge_types::ForgeManifest;
    use orkia_rfc_core::frontmatter::{ForgeFrontmatterBlock, ForgePermissions, ForgeWindow};

    fn fixture() -> (ForgeFrontmatterBlock, BuildResponse) {
        let forge = ForgeFrontmatterBlock {
            name: "hello".into(),
            description: "desc".into(),
            icon: None,
            window: ForgeWindow {
                title: "Hello".into(),
                width: 480,
                height: 320,
                resizable: true,
            },
            permissions: ForgePermissions::default(),
            agent: None,
        };
        let resp = BuildResponse {
            build_id: "b1".into(),
            builder_version: "v0.1.0".into(),
            model: "test-model".into(),
            files: crate::response::GeneratedFiles {
                html: "<html></html>".into(),
                css: "body{}".into(),
                js: "console.log(1);".into(),
                icon_svg: "<svg/>".into(),
            },
            manifest_overrides: serde_json::json!({}),
            usage: crate::response::ServerUsage {
                input_tokens: 10,
                output_tokens: 20,
                retries: 0,
                duration_ms: 100,
                remaining_quota: 99,
                quota_reset_at: Utc::now(),
            },
        };
        (forge, resp)
    }

    #[test]
    fn write_build_creates_expected_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (forge, resp) = fixture();
        let written = write_build(tmp.path(), &forge, "raw-rfc", &resp, "abc123").unwrap();
        // 7 files: html, css, js, icon, build/response.json, build/rfc.txt, manifest.toml
        assert_eq!(written.len(), 7);
        for expected in &[
            "app.html",
            "app.css",
            "app.js",
            "icon.svg",
            "manifest.toml",
            "build/response.json",
            "build/rfc.txt",
        ] {
            assert!(tmp.path().join(expected).exists(), "missing {expected}");
        }
        assert!(tmp.path().join("data").is_dir());
        assert!(tmp.path().join("seal/events.jsonl").exists());
    }

    #[test]
    fn write_build_preserves_existing_data_and_seal() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("data")).unwrap();
        std::fs::write(tmp.path().join("data/state.json"), b"{}").unwrap();
        std::fs::create_dir_all(tmp.path().join("seal")).unwrap();
        std::fs::write(tmp.path().join("seal/events.jsonl"), b"existing\n").unwrap();

        let (forge, resp) = fixture();
        write_build(tmp.path(), &forge, "raw", &resp, "h").unwrap();

        assert_eq!(
            std::fs::read_to_string(tmp.path().join("data/state.json")).unwrap(),
            "{}"
        );
        // seal/events.jsonl was non-empty before; we should not have
        // overwritten it.
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("seal/events.jsonl")).unwrap(),
            "existing\n"
        );
    }

    #[test]
    fn write_build_stamps_rfc_hash_in_manifest() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (forge, resp) = fixture();
        write_build(tmp.path(), &forge, "raw", &resp, "feedface").unwrap();
        let m = ForgeManifest::load(&tmp.path().join("manifest.toml")).unwrap();
        assert_eq!(m.forge.rfc_hash, "sha256:feedface");
    }

    #[test]
    fn write_build_persists_rfc_snapshot() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (forge, resp) = fixture();
        write_build(tmp.path(), &forge, "exact-rfc-bytes", &resp, "h").unwrap();
        let snap = std::fs::read_to_string(tmp.path().join("build/rfc.txt")).unwrap();
        assert_eq!(snap, "exact-rfc-bytes");
    }
}
