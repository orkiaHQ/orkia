// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Content-addressed hashing of RFCs.
//!
//! The hash is computed over a stable wire representation that
//! intentionally excludes mutable frontmatter fields (`updated_at`,
//! `locked_by`, etc.). This keeps the hash deterministic across
//! cosmetic edits and lets `--rerun` detect "RFC content actually
//! changed?" with byte precision.

use orkia_rfc_core::RfcRecord;
use sha2::{Digest, Sha256};

/// Hex SHA-256 of the given bytes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// Render a stable wire representation of an RFC for the backend.
/// Mirrors ScaffoldBuilder's render so the hash matches across
/// scaffold/remote builders. Mutable frontmatter (timestamps, lock
/// fields) is intentionally excluded.
pub fn render_rfc_for_wire(rfc: &RfcRecord) -> String {
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
    if let Some(forge) = &rfc.fm.forge
        && let Ok(t) = toml::to_string(forge)
    {
        s.push_str("[forge]\n");
        s.push_str(&t);
    }
    s.push_str("---body---\n");
    s.push_str(&rfc.body);
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, TimeZone};
    use orkia_rfc_core::frontmatter::{ForgeFrontmatterBlock, ForgePermissions, ForgeWindow};
    use orkia_rfc_core::{ContentHash, RfcFrontmatter, RfcId, RfcState};

    fn make_rfc(ts: chrono::DateTime<FixedOffset>) -> RfcRecord {
        RfcRecord {
            fm: RfcFrontmatter {
                id: RfcId::new("hello"),
                state: RfcState::DraftEmpty,
                version: 1,
                created_at: ts,
                updated_at: ts,
                content_hash: ContentHash("sha256:0".into()),
                agents: vec![],
                locked_by: None,
                locked_at: None,
                title: None,
                status: None,
                assigned: None,
                kind: Some("forge-app".into()),
                forge: Some(ForgeFrontmatterBlock {
                    name: "hello".into(),
                    description: "x".into(),
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
            body: "body".into(),
        }
    }

    #[test]
    fn render_rfc_is_stable_across_updated_at() {
        let ts = FixedOffset::east_opt(0)
            .unwrap()
            .with_ymd_and_hms(2026, 5, 22, 14, 0, 0)
            .unwrap();
        let mut rfc = make_rfc(ts);
        let h1 = sha256_hex(render_rfc_for_wire(&rfc).as_bytes());
        rfc.fm.updated_at += chrono::Duration::hours(1);
        let h2 = sha256_hex(render_rfc_for_wire(&rfc).as_bytes());
        assert_eq!(h1, h2);
    }

    #[test]
    fn rfc_hash_changes_on_body_change() {
        let ts = FixedOffset::east_opt(0)
            .unwrap()
            .with_ymd_and_hms(2026, 5, 22, 14, 0, 0)
            .unwrap();
        let mut rfc = make_rfc(ts);
        let h1 = sha256_hex(render_rfc_for_wire(&rfc).as_bytes());
        rfc.body.push_str(" extra");
        let h2 = sha256_hex(render_rfc_for_wire(&rfc).as_bytes());
        assert_ne!(h1, h2);
    }

    #[test]
    fn sha256_hex_known_vector() {
        // SHA-256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
