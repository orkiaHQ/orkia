// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Manifest merge — combines the RFC's `[forge]` frontmatter block
//! with the server-supplied `manifest_overrides` into a single
//! [`ForgeManifest`].
//!
//! Precedence: server window-overrides win when present; otherwise
//! the RFC values stand.

use std::path::Path;

use chrono::Utc;
use orkia_forge_types::{ForgeConfig, ForgeManifest, Permissions, WindowConfig};
use orkia_shell_types::BuilderError;

use crate::response::BuildResponse;

/// Combine the RFC's `[forge]` block with the server-supplied
/// `manifest_overrides` into a fresh [`ForgeManifest`]. The
/// `rfc_hash` is stamped into `forge.rfc_hash` as `sha256:<hex>`.
pub fn build_manifest(
    forge: &orkia_rfc_core::frontmatter::ForgeFrontmatterBlock,
    resp: &BuildResponse,
    rfc_hash: &str,
) -> Result<ForgeManifest, BuilderError> {
    // Server-supplied window overrides win when present; otherwise we
    // honor the RFC's [forge.window] block.
    let server_window = resp.manifest_overrides.get("window");
    let title = server_window
        .and_then(|w| w.get("title"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| forge.window.title.clone());
    // Window dims come from the server (untrusted): clamp to a sane range so a
    // bogus value can't truncate via `as u32` or blow up `inner_size` (BUG-N07).
    const MIN_DIM: u64 = 100;
    const MAX_DIM: u64 = 16_384;
    let width = server_window
        .and_then(|w| w.get("width"))
        .and_then(|v| v.as_u64())
        .map(|n| n.clamp(MIN_DIM, MAX_DIM) as u32)
        .unwrap_or(forge.window.width);
    let height = server_window
        .and_then(|w| w.get("height"))
        .and_then(|v| v.as_u64())
        .map(|n| n.clamp(MIN_DIM, MAX_DIM) as u32)
        .unwrap_or(forge.window.height);
    let resizable = server_window
        .and_then(|w| w.get("resizable"))
        .and_then(|v| v.as_bool())
        .unwrap_or(forge.window.resizable);

    Ok(ForgeManifest {
        forge: ForgeConfig {
            name: forge.name.clone(),
            description: forge.description.clone(),
            version: "0.1.0".into(),
            api_version: orkia_forge_types::default_api_version(),
            rfc_id: forge.name.clone(),
            rfc_hash: format!("sha256:{rfc_hash}"),
            created_at: Utc::now(),
            icon: forge.icon.clone().unwrap_or_else(|| "default".into()),
            window: WindowConfig {
                title,
                width,
                height,
                resizable,
            },
            permissions: Permissions {
                storage: forge.permissions.storage,
                agent: forge.permissions.agent,
                network: forge.permissions.network.clone(),
                notification: forge.permissions.notification,
            },
        },
    })
}

/// Read the previous build's `rfc_hash` from `<dir>/manifest.toml`,
/// stripped of its `sha256:` prefix. Returns `None` when no
/// manifest exists or it is unreadable / malformed — the caller
/// treats absence as "no previous build."
pub fn load_previous_hash(dir: &Path) -> Option<String> {
    ForgeManifest::load(&dir.join("manifest.toml"))
        .ok()
        .and_then(|m| m.forge.rfc_hash.strip_prefix("sha256:").map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_forge_types::{ForgeConfig, Permissions, WindowConfig};
    use orkia_rfc_core::frontmatter::{ForgeFrontmatterBlock, ForgePermissions, ForgeWindow};

    fn forge_block() -> ForgeFrontmatterBlock {
        ForgeFrontmatterBlock {
            name: "hello".into(),
            description: "desc".into(),
            icon: None,
            window: ForgeWindow {
                title: "From RFC".into(),
                width: 480,
                height: 320,
                resizable: false,
            },
            permissions: ForgePermissions::default(),
            agent: None,
        }
    }

    fn response_with_overrides(overrides: serde_json::Value) -> BuildResponse {
        BuildResponse {
            build_id: "b1".into(),
            builder_version: "v0".into(),
            model: "m".into(),
            files: crate::response::GeneratedFiles {
                html: String::new(),
                css: String::new(),
                js: String::new(),
                icon_svg: String::new(),
            },
            manifest_overrides: overrides,
            usage: crate::response::ServerUsage {
                input_tokens: 0,
                output_tokens: 0,
                retries: 0,
                duration_ms: 0,
                remaining_quota: 0,
                quota_reset_at: Utc::now(),
            },
        }
    }

    #[test]
    fn merge_keeps_rfc_window_when_no_overrides() {
        let resp = response_with_overrides(serde_json::json!({}));
        let m = build_manifest(&forge_block(), &resp, "abc").unwrap();
        assert_eq!(m.forge.window.title, "From RFC");
        assert_eq!(m.forge.window.width, 480);
        assert!(!m.forge.window.resizable);
        assert_eq!(m.forge.rfc_hash, "sha256:abc");
    }

    #[test]
    fn merge_applies_server_window_overrides() {
        let resp = response_with_overrides(serde_json::json!({
            "window": {
                "title": "From Server",
                "width": 1024,
                "resizable": true,
            }
        }));
        let m = build_manifest(&forge_block(), &resp, "abc").unwrap();
        assert_eq!(m.forge.window.title, "From Server");
        assert_eq!(m.forge.window.width, 1024);
        // height was not overridden — stays from RFC
        assert_eq!(m.forge.window.height, 320);
        assert!(m.forge.window.resizable);
    }

    #[test]
    fn merge_stamps_rfc_hash() {
        let resp = response_with_overrides(serde_json::json!({}));
        let m = build_manifest(&forge_block(), &resp, "deadbeef").unwrap();
        assert_eq!(m.forge.rfc_hash, "sha256:deadbeef");
    }

    #[test]
    fn load_previous_hash_strips_prefix() {
        let tmp = tempfile::TempDir::new().unwrap();
        let manifest = ForgeManifest {
            forge: ForgeConfig {
                name: "hello".into(),
                description: String::new(),
                version: "0.1.0".into(),
                api_version: 1,
                rfc_id: "hello".into(),
                rfc_hash: "sha256:cafef00d".into(),
                created_at: Utc::now(),
                icon: "default".into(),
                window: WindowConfig {
                    title: "Hello".into(),
                    width: 480,
                    height: 320,
                    resizable: true,
                },
                permissions: Permissions::default(),
            },
        };
        manifest.save(&tmp.path().join("manifest.toml")).unwrap();
        let h = load_previous_hash(tmp.path()).unwrap();
        assert_eq!(h, "cafef00d");
    }

    #[test]
    fn load_previous_hash_missing_returns_none() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(load_previous_hash(tmp.path()).is_none());
    }
}
