// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! ```text
//! <project_dir>/
//!   rfcs/<id>.md
//!   rfcs/<id>.history/v<n>.md
//!   decisions/<id>.jsonl
//!   decisions/<id>.history/v<n>.jsonl
//!   seal/<id>.seal.jsonl
//! ```

use chrono::Utc;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

/// Map a state-machine state to the legacy short string the workspace
/// loader expects in `RfcSummary.status`. Workspace-side tooling has no
/// concept of `draft-empty` vs `draft-active`, so both collapse to "draft".
fn legacy_status_for(state: RfcState) -> &'static str {
    match state {
        RfcState::DraftEmpty | RfcState::DraftActive => "draft",
        RfcState::Active => "active",
        RfcState::Archived => "archived",
        RfcState::Completed => "completed",
        RfcState::Abandoned => "abandoned",
    }
}

use crate::decision::{
    DecisionCounts, DecisionRecord, append as decision_append, counts_from,
    read_all as decision_read_all,
};
use crate::error::RfcError;
use crate::frontmatter::{RfcFrontmatter, parse_frontmatter, render_frontmatter};
use crate::hash::content_hash_of;
use crate::id::RfcId;
use crate::state::RfcState;

/// A loaded RFC: frontmatter + body. The hash on `fm.content_hash` is the one
/// persisted on disk; the caller should compare with `content_hash_of(&body)`
/// if it needs to detect manual edits outside Orkia.
#[derive(Debug, Clone)]
pub struct RfcRecord {
    pub fm: RfcFrontmatter,
    pub body: String,
}

/// Filesystem-backed RFC store rooted at a project directory.
#[derive(Debug, Clone)]
pub struct RfcStore {
    project_dir: PathBuf,
}

impl RfcStore {
    pub fn new(project_dir: impl Into<PathBuf>) -> Self {
        Self {
            project_dir: project_dir.into(),
        }
    }

    pub fn project_dir(&self) -> &Path {
        &self.project_dir
    }

    /// Reject any `RfcId` whose string representation contains path-traversal
    /// characters (`/`, `\`, `..`, empty segments). A valid id must form
    /// exactly one `Normal` path component — i.e. a bare filename fragment
    /// with no directory separators. Fail-closed: unknown/ambiguous ids are
    /// rejected, not trusted. (SEC-033)
    fn validate_id(id: &RfcId) -> Result<(), RfcError> {
        let p = std::path::Path::new(id.as_str());
        let mut components = p.components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(_)), None) => Ok(()),
            _ => Err(RfcError::Io {
                operation: "validate_rfc_id",
                message: format!(
                    "RfcId {:?} contains path-traversal characters or is otherwise invalid",
                    id.as_str()
                ),
            }),
        }
    }

    pub fn rfc_path(&self, id: &RfcId) -> PathBuf {
        self.project_dir.join("rfcs").join(format!("{id}.md"))
    }

    pub fn decision_path(&self, id: &RfcId) -> PathBuf {
        self.project_dir
            .join("decisions")
            .join(format!("{id}.jsonl"))
    }

    pub fn seal_path(&self, id: &RfcId) -> PathBuf {
        self.project_dir
            .join("seal")
            .join(format!("{id}.seal.jsonl"))
    }

    fn rfc_history_dir(&self, id: &RfcId) -> PathBuf {
        self.project_dir.join("rfcs").join(format!("{id}.history"))
    }

    fn decision_history_dir(&self, id: &RfcId) -> PathBuf {
        self.project_dir
            .join("decisions")
            .join(format!("{id}.history"))
    }

    /// Create a brand-new RFC in DraftEmpty state. Errors if one already
    /// exists at the same id.
    pub fn create(&self, id: &RfcId, title: Option<&str>) -> Result<RfcRecord, RfcError> {
        Self::validate_id(id)?;
        self.create_with_legacy(id, title, &[])
    }

    /// Same as [`create`] but also seeds the legacy `assigned` mirror in the
    /// frontmatter. Used by the workspace-side `create_rfc` so the existing
    /// project loader (which reads `assigned` for RFC list rendering) still
    /// sees the agent assignments.
    pub fn create_with_legacy(
        &self,
        id: &RfcId,
        title: Option<&str>,
        assigned: &[String],
    ) -> Result<RfcRecord, RfcError> {
        // `create` already validates before delegating here, but external
        // callers may bypass `create` and call this directly.
        Self::validate_id(id)?;
        let path = self.rfc_path(id);
        if path.exists() {
            return Err(RfcError::Frontmatter {
                message: format!("RFC {id} already exists"),
            });
        }
        let body = String::new();
        let hash = content_hash_of(&body);
        let now = Utc::now().fixed_offset();
        let fm = RfcFrontmatter {
            id: id.clone(),
            state: RfcState::DraftEmpty,
            version: 1,
            created_at: now,
            updated_at: now,
            content_hash: hash,
            agents: Vec::new(),
            locked_by: None,
            locked_at: None,
            title: title.map(str::to_string),
            // Mirror `RfcState::DraftEmpty` as the legacy short form so
            // the workspace loader populates `RfcSummary.status`.
            status: Some("draft".to_string()),
            assigned: if assigned.is_empty() {
                None
            } else {
                Some(assigned.to_vec())
            },
            kind: None,
            forge: None,
            scope: None,
            operator: None,
        };
        self.write_atomic(id, &fm, &body)?;
        Ok(RfcRecord { fm, body })
    }

    /// Load an RFC from disk.
    pub fn load(&self, id: &RfcId) -> Result<RfcRecord, RfcError> {
        Self::validate_id(id)?;
        let path = self.rfc_path(id);
        if !path.exists() {
            return Err(RfcError::NotFound {
                rfc_id: id.clone(),
                operation: "load".into(),
            });
        }
        let raw = fs::read_to_string(&path).map_err(|e| RfcError::io("rfc_read", e))?;
        let (fm, body) = parse_frontmatter(&raw)?;
        Ok(RfcRecord { fm, body })
    }

    /// Write the full RFC atomically (tmp + fsync + rename). Recomputes
    /// `updated_at` and `content_hash`, and re-syncs the legacy `status`
    /// mirror from the current `state` so the workspace loader stays
    /// coherent across state transitions.
    pub fn save(&self, mut fm: RfcFrontmatter, body: String) -> Result<RfcRecord, RfcError> {
        Self::validate_id(&fm.id)?;
        fm.updated_at = Utc::now().fixed_offset();
        fm.content_hash = content_hash_of(&body);
        fm.status = Some(legacy_status_for(fm.state).to_string());
        let id = fm.id.clone();
        self.write_atomic(&id, &fm, &body)?;
        Ok(RfcRecord { fm, body })
    }

    fn write_atomic(&self, id: &RfcId, fm: &RfcFrontmatter, body: &str) -> Result<(), RfcError> {
        let final_path = self.rfc_path(id);
        if let Some(parent) = final_path.parent() {
            fs::create_dir_all(parent).map_err(|e| RfcError::io("rfc_mkdir", e))?;
        }
        let tmp_path = final_path.with_extension("md.tmp");
        let content = render_frontmatter(fm, body)?;
        {
            let mut f =
                fs::File::create(&tmp_path).map_err(|e| RfcError::io("rfc_tmp_create", e))?;
            f.write_all(content.as_bytes())
                .map_err(|e| RfcError::io("rfc_tmp_write", e))?;
            f.sync_all().map_err(|e| RfcError::io("rfc_tmp_fsync", e))?;
        }
        fs::rename(&tmp_path, &final_path).map_err(|e| RfcError::io("rfc_rename", e))?;
        Ok(())
    }

    /// Durable write to an arbitrary path: tmp + fsync + rename. Used to make
    /// archive writes crash-safe (BUG-090).
    fn write_path_atomic(
        path: &std::path::Path,
        content: &[u8],
        op: &'static str,
    ) -> Result<(), RfcError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| RfcError::io(op, e))?;
        }
        let tmp_path = path.with_extension("tmp");
        {
            let mut f = fs::File::create(&tmp_path).map_err(|e| RfcError::io(op, e))?;
            f.write_all(content).map_err(|e| RfcError::io(op, e))?;
            f.sync_all().map_err(|e| RfcError::io(op, e))?;
        }
        fs::rename(&tmp_path, path).map_err(|e| RfcError::io(op, e))?;
        Ok(())
    }

    /// List all RFCs in the project. Each returned record is loaded; on parse
    /// failure the offending file is skipped (logging is the caller's job).
    pub fn list(&self) -> Result<Vec<RfcRecord>, RfcError> {
        let dir = self.project_dir.join("rfcs");
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&dir).map_err(|e| RfcError::io("rfc_listdir", e))? {
            let entry = entry.map_err(|e| RfcError::io("rfc_list_entry", e))?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            let raw = match fs::read_to_string(&path) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if let Ok((fm, body)) = parse_frontmatter(&raw) {
                out.push(RfcRecord { fm, body });
            }
        }
        out.sort_by(|a, b| a.fm.id.0.cmp(&b.fm.id.0));
        Ok(out)
    }

    pub fn append_decision(&self, record: &DecisionRecord) -> Result<(), RfcError> {
        Self::validate_id(&record.rfc_id)?;
        decision_append(&self.decision_path(&record.rfc_id), record)
    }

    pub fn read_decisions(&self, id: &RfcId) -> Result<Vec<DecisionRecord>, RfcError> {
        Self::validate_id(id)?;
        decision_read_all(&self.decision_path(id))
    }

    pub fn decision_counts(&self, id: &RfcId) -> Result<DecisionCounts, RfcError> {
        let records = self.read_decisions(id)?;
        Ok(counts_from(&records))
    }

    /// Reopen an RFC: archive current version's body and decision log to
    /// `<id>.history/v<n>.{md,jsonl}`, then seed a new current version at
    pub fn reopen(&self, id: &RfcId) -> Result<RfcRecord, RfcError> {
        // `load` already validates, but validate early for the clearest error.
        Self::validate_id(id)?;
        let current = self.load(id)?;
        let n = current.fm.version;
        // Archive RFC body.
        let hist_dir = self.rfc_history_dir(id);
        fs::create_dir_all(&hist_dir).map_err(|e| RfcError::io("reopen_mkdir_rfc", e))?;
        let archived_body_path = hist_dir.join(format!("v{n}.md"));
        let mut archived_fm = current.fm.clone();
        archived_fm.state = RfcState::Archived;
        let archived_content = render_frontmatter(&archived_fm, &current.body)?;
        // Durable archive write (BUG-090).
        Self::write_path_atomic(
            &archived_body_path,
            archived_content.as_bytes(),
            "reopen_write_archived_rfc",
        )?;
        // Archive the decision log by COPYING it to history first (durable),
        // leaving the live log in place until after the v+1 commit. The
        // original bug moved it before the commit, so a crash left the RFC at
        // version n with its decision log gone; now the live log is only
        // removed after the atomic commit succeeds (BUG-090).
        let dec_path = self.decision_path(id);
        let dec_existed = dec_path.exists();
        if dec_existed {
            let dec_hist_dir = self.decision_history_dir(id);
            let dec_bytes = fs::read(&dec_path).map_err(|e| RfcError::io("reopen_read_dec", e))?;
            Self::write_path_atomic(
                &dec_hist_dir.join(format!("v{n}.jsonl")),
                &dec_bytes,
                "reopen_archive_dec",
            )?;
        }
        // Seed v+1 in DraftActive from the same body.
        let now = Utc::now().fixed_offset();
        let new_body = current.body.clone();
        let new_fm = RfcFrontmatter {
            id: id.clone(),
            state: RfcState::DraftActive,
            version: n + 1,
            created_at: now,
            updated_at: now,
            content_hash: content_hash_of(&new_body),
            agents: Vec::new(),
            locked_by: None,
            locked_at: None,
            title: current.fm.title.clone(),
            // Legacy mirrors carried forward so the workspace loader keeps
            // showing the right status/assigned for v+1.
            status: Some(legacy_status_for(RfcState::DraftActive).to_string()),
            assigned: current.fm.assigned.clone(),
            kind: current.fm.kind.clone(),
            forge: current.fm.forge.clone(),
            scope: current.fm.scope,
            operator: current.fm.operator.clone(),
        };
        // Commit point: the atomic rename of the v+1 RFC. Everything above is
        // durable and non-destructive, so a crash before here leaves version n
        // fully intact (its decision log was copied, never moved).
        self.write_atomic(id, &new_fm, &new_body)?;
        // Post-commit: clear the live decision log so v+1 starts fresh. The
        // archived copy already exists, so removing it here is safe.
        if dec_existed {
            let _ = fs::remove_file(&dec_path);
        }
        Ok(RfcRecord {
            fm: new_fm,
            body: new_body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn store() -> (tempfile::TempDir, RfcStore) {
        let dir = tempdir().expect("tmpdir");
        let s = RfcStore::new(dir.path().to_path_buf());
        (dir, s)
    }

    #[test]
    fn create_then_load() {
        let (_d, s) = store();
        let id = RfcId::new("auth-pkce");
        let rec = s.create(&id, Some("PKCE")).expect("create");
        assert_eq!(rec.fm.state, RfcState::DraftEmpty);
        assert_eq!(rec.fm.version, 1);
        let loaded = s.load(&id).expect("load");
        assert_eq!(loaded.fm.id, id);
        assert_eq!(loaded.fm.state, RfcState::DraftEmpty);
    }

    #[test]
    fn create_twice_fails() {
        let (_d, s) = store();
        let id = RfcId::new("x");
        s.create(&id, None).expect("create");
        assert!(s.create(&id, None).is_err());
    }

    #[test]
    fn save_updates_hash() {
        let (_d, s) = store();
        let id = RfcId::new("h");
        let rec = s.create(&id, None).expect("create");
        let body = "## Context\nhello\n".to_string();
        let saved = s.save(rec.fm.clone(), body.clone()).expect("save");
        assert_eq!(saved.fm.content_hash, content_hash_of(&body));
        let loaded = s.load(&id).expect("load");
        assert_eq!(loaded.body, body);
        assert_eq!(loaded.fm.content_hash, saved.fm.content_hash);
    }

    #[test]
    fn reopen_archives_and_bumps_version() {
        let (_d, s) = store();
        let id = RfcId::new("r");
        let rec = s.create(&id, None).expect("create");
        let mut fm = rec.fm.clone();
        fm.state = RfcState::Active;
        let _ = s.save(fm, "body v1\n".into()).expect("save");
        let new_rec = s.reopen(&id).expect("reopen");
        assert_eq!(new_rec.fm.version, 2);
        assert_eq!(new_rec.fm.state, RfcState::DraftActive);
        let archived = s.project_dir().join("rfcs").join("r.history").join("v1.md");
        assert!(archived.exists(), "v1 should be archived");
    }

    #[test]
    fn list_returns_sorted() {
        let (_d, s) = store();
        s.create(&RfcId::new("b"), None).expect("b");
        s.create(&RfcId::new("a"), None).expect("a");
        let all = s.list().expect("list");
        let ids: Vec<&str> = all.iter().map(|r| r.fm.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    // ── SEC-033 path-traversal validation ─────────────────────────────────

    #[test]
    fn validate_id_accepts_valid_slugs() {
        for slug in &["auth-pkce", "rfc-001", "feature_x", "a", "abc123"] {
            assert!(
                RfcStore::validate_id(&RfcId::new(*slug)).is_ok(),
                "expected valid: {slug}"
            );
        }
    }

    #[test]
    fn validate_id_rejects_traversal() {
        for slug in &[
            "../../etc/passwd",
            "../sibling",
            "sub/dir",
            "",
            ".",
            "..",
            "/absolute",
        ] {
            assert!(
                RfcStore::validate_id(&RfcId::new(*slug)).is_err(),
                "expected rejection: {slug}"
            );
        }
    }

    #[test]
    fn create_rejects_traversal_id() {
        let (_d, s) = store();
        let id = RfcId::new("../../etc/passwd");
        assert!(s.create(&id, None).is_err());
    }

    #[test]
    fn load_rejects_traversal_id() {
        let (_d, s) = store();
        let id = RfcId::new("../outside");
        assert!(s.load(&id).is_err());
    }
}
