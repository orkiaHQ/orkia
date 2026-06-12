// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! A single scoped SEAL chain.
//!
//! Each chain is a hash-linked, append-only log of events for one
//! subject — a job, a project. Records are persisted line-by-line as
//! NDJSON to the chain's file. Chains carry an in-memory mirror of
//! the file (read once at load), so verification doesn't re-touch
//! disk after construction.
//!
//! Scopes are decided by the caller (`SealManager`): this type only
//! knows the file path and whether the chain is closed. The chain
//! has no concept of "global" — every chain is bounded.

use std::path::PathBuf;

use chrono::Utc;
use orkia_rfc_core::RfcId;
use sha2::{Digest, Sha256};

use crate::seal::{SealError, SealRecord};

/// 64-char hex string used as `prev_hash` for the genesis record of
/// every chain. Picked so that no real SHA-256 can collide with it
/// (probability ~ 2⁻²⁵⁶), and so visual scans for it in NDJSON
/// instantly identify genesis entries.
pub const ZERO_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// SHA-256 of the concatenated fields, hex-encoded.
///
/// The optional `rfc_id` segment is included only when `Some` (prefixed
/// with the `rfc_id:` tag), so a record without RFC context produces the
/// same hash as records written before the field existed — preserving
///
/// Note (SEC-080, deferred): this concatenation has no explicit per-field
/// length prefix. The audit flagged this as a NIT — non-exploitable at the
/// current threat model (`prev_hash` is fixed-length, `detail.to_string()`
/// always starts with a non-ambiguous JSON char, `event_type` is a
/// controlled string). Adding length prefixes would change every hash and
/// silently invalidate all on-disk chains, so it is intentionally NOT done
/// here; any future domain-separation hardening must be paired with a chain
/// format/version migration.
pub fn compute_hash(
    prev_hash: &str,
    event_type: &str,
    detail: &serde_json::Value,
    timestamp: &str,
    rfc_id: Option<&RfcId>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prev_hash.as_bytes());
    hasher.update(event_type.as_bytes());
    hasher.update(detail.to_string().as_bytes());
    hasher.update(timestamp.as_bytes());
    if let Some(rfc_id) = rfc_id {
        hasher.update(b"rfc_id:");
        hasher.update(rfc_id.as_str().as_bytes());
    }
    hex::encode(hasher.finalize())
}

/// An append-only hash-linked chain backed by one file. The struct
/// holds the in-memory mirror; the file is the durable record.
///
/// Closed chains reject further appends but stay queryable —
/// `verify`, `tip_hash`, `records` all keep working. Closing a job
/// chain (on `agent.complete`) is what lets the project chain refer
/// to it with a stable hash.
pub struct SealChain {
    records: Vec<SealRecord>,
    file_path: PathBuf,
    next_seq: u64,
    closed: bool,
}

impl SealChain {
    /// Build an empty chain anchored at `file_path`. Parent
    /// directories are created. The file is not created on disk
    /// until the first `append`.
    pub fn create(file_path: PathBuf) -> std::io::Result<Self> {
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Self {
            records: Vec::new(),
            file_path,
            next_seq: 0,
            closed: false,
        })
    }

    /// Best-effort wrapper around [`Self::load`]: if loading fails
    /// (e.g. parent directory cannot be created due to permissions,
    /// or the file is unreadable), the error is logged and a fresh
    /// chain is returned that is immediately marked closed. Subsequent
    /// `append` calls no-op rather than corrupting the on-disk file.
    ///
    /// Callers that need to know whether persistence works should use
    /// `load` and surface the error themselves; this exists so that
    /// audit failures do not panic the shell (CLAUDE.md fail-closed:
    /// the chain becomes inert rather than crashing the process).
    pub fn load_or_quarantine(file_path: PathBuf) -> Self {
        match Self::load(file_path.clone()) {
            Ok(chain) => chain,
            Err(e) => {
                tracing::error!(
                    path = %file_path.display(),
                    error = %e,
                    "seal: chain load failed — quarantining (future appends will no-op)",
                );
                Self {
                    records: Vec::new(),
                    file_path,
                    next_seq: 0,
                    closed: true,
                }
            }
        }
    }

    /// Load an existing chain from `file_path`. A missing file is
    /// treated as an empty chain (so the caller can transparently
    /// turn first-touch into a `create`). Malformed lines are
    /// logged and skipped — defence against partial writes on prior
    /// crashes.
    ///
    /// The chain is considered closed if its last record's
    /// `event_type` is one of the terminal events
    /// (`agent.complete`, `agent.failed`); future `append` calls
    /// will silently no-op until `close()` is reset (we don't expose
    /// a reset — closure is one-way).
    pub fn load(file_path: PathBuf) -> std::io::Result<Self> {
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Distinguish "file absent" (normal → empty chain) from a real read
        // error (permissions, non-UTF-8, disk). The old `unwrap_or_default()`
        // turned an unreadable-but-present chain into an empty one, so the next
        // append rewrote a genesis on top of it — a corrupt mixed file that
        // `load_or_quarantine` never saw because `load` returned Ok (BUG-097).
        let content = match std::fs::read_to_string(&file_path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        let mut records: Vec<SealRecord> = Vec::new();
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<SealRecord>(line) {
                Ok(r) => records.push(r),
                Err(e) => {
                    tracing::warn!(
                        path = %file_path.display(),
                        error = %e,
                        "seal: skipping malformed record on load",
                    );
                }
            }
        }
        let next_seq = records.last().map(|r| r.seq + 1).unwrap_or(0);
        let closed = records
            .last()
            .map(|r| is_terminal_event(&r.event_type))
            .unwrap_or(false);
        Ok(Self {
            records,
            file_path,
            next_seq,
            closed,
        })
    }

    /// Append one record. Fail-closed: returns `Err` if the chain
    /// was already closed (`SealError::Closed`), if serialization
    /// fails (`SealError::Serialize`), or if the disk write fails
    /// (`SealError::Io`). On success the record is durable before
    /// this returns — we open + append + drop the file handle
    /// synchronously, so a process crash after the call preserves
    /// the record.
    ///
    /// The in-memory mirror is only updated **after** the disk write
    /// succeeds. A failed append leaves both `records` and `next_seq`
    /// unchanged, so a caller that ignores the `Err` cannot mistake a
    /// lost record for a present one.
    pub fn append(
        &mut self,
        event_type: &str,
        detail: serde_json::Value,
    ) -> Result<&SealRecord, SealError> {
        self.append_with_rfc(event_type, detail, None)
    }

    /// Like [`Self::append`] but tags the record with an RFC id so it
    /// contributes to that RFC's SEAL v1 document at closure.
    ///
    /// The REPL calls this from `emit_audit_event` with the current
    /// RFC context (`rfc cd <slug>` sets it, `rfc cd --clear` clears it).
    /// Pass `None` to record an event outside any RFC scope — the wire
    /// format and hash are then identical to a pre-`rfc_id` chain.
    pub fn append_with_rfc(
        &mut self,
        event_type: &str,
        detail: serde_json::Value,
        rfc_id: Option<RfcId>,
    ) -> Result<&SealRecord, SealError> {
        if self.closed {
            return Err(SealError::Closed);
        }
        let prev_hash = self
            .records
            .last()
            .map(|r| r.hash.clone())
            .unwrap_or_else(|| ZERO_HASH.to_string());
        let timestamp = Utc::now().to_rfc3339();
        let hash = compute_hash(&prev_hash, event_type, &detail, &timestamp, rfc_id.as_ref());
        let record = SealRecord {
            seq: self.next_seq,
            timestamp,
            event_type: event_type.to_string(),
            detail,
            hash,
            prev_hash,
            rfc_id,
        };

        // Serialize + write to disk BEFORE mutating in-memory state.
        // If either step fails, propagate the error and leave the
        let line = serde_json::to_string(&record)?;
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.file_path)?;
            writeln!(f, "{line}")?;
        }

        if is_terminal_event(event_type) {
            self.closed = true;
        }
        self.next_seq += 1;
        self.records.push(record);
        // `last` is Some because we just pushed above; return a typed error
        // instead of panicking if that invariant is ever broken (BUG-080).
        self.records
            .last()
            .ok_or_else(|| SealError::Internal("record vanished after push".into()))
    }

    /// Manually close the chain. Idempotent. A closed chain refuses
    /// further `append` calls. Use this when you want to seal
    /// without writing a terminal event (rare — typically the
    /// caller emits `agent.complete` and `append` auto-closes).
    pub fn close(&mut self) {
        self.closed = true;
    }

    /// `(valid, broken_at_seq)`. Walks the chain, recomputing each
    /// hash and checking the `prev_hash` linkage. Returns the first
    /// `seq` that fails verification, or `None` when the whole
    /// chain validates.
    pub fn verify(&self) -> (bool, Option<u64>) {
        for (i, record) in self.records.iter().enumerate() {
            let expected_prev = if i == 0 {
                ZERO_HASH.to_string()
            } else {
                self.records[i - 1].hash.clone()
            };
            if record.prev_hash != expected_prev {
                return (false, Some(record.seq));
            }
            let computed = compute_hash(
                &expected_prev,
                &record.event_type,
                &record.detail,
                &record.timestamp,
                record.rfc_id.as_ref(),
            );
            if computed != record.hash {
                return (false, Some(record.seq));
            }
        }
        (true, None)
    }

    /// Hash of the most recent record. Used by `SealManager` to
    /// embed a job chain's terminal hash into the project chain's
    /// `job.reference` record — a Merkle-style bridge between scopes.
    pub fn tip_hash(&self) -> Option<&str> {
        self.records.last().map(|r| r.hash.as_str())
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn records(&self) -> &[SealRecord] {
        &self.records
    }

    pub fn file_path(&self) -> &std::path::Path {
        &self.file_path
    }
}

/// Terminal event names — recording one of these closes the chain.
/// Defined once here so `append` and `load` agree on closure
/// semantics (otherwise a reload could re-open a chain by missing
/// the terminal marker).
fn is_terminal_event(event_type: &str) -> bool {
    matches!(event_type, "agent.complete" | "agent.failed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn chain_at(dir: &std::path::Path, name: &str) -> SealChain {
        SealChain::create(dir.join(name)).expect("create chain")
    }

    #[test]
    fn appends_link_correctly() {
        let dir = tempdir().unwrap();
        let mut c = chain_at(dir.path(), "a.jsonl");
        let r0 = c
            .append("event.a", serde_json::json!({"v": 1}))
            .expect("first append")
            .clone();
        assert_eq!(r0.seq, 0);
        assert_eq!(r0.prev_hash, ZERO_HASH);

        let r1 = c
            .append("event.b", serde_json::json!({"v": 2}))
            .expect("second append")
            .clone();
        assert_eq!(r1.seq, 1);
        assert_eq!(r1.prev_hash, r0.hash);
        assert_ne!(r1.hash, r0.hash);

        let (ok, broken) = c.verify();
        assert!(ok);
        assert!(broken.is_none());
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn persists_across_reload() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("p.jsonl");
        {
            let mut c = SealChain::create(path.clone()).unwrap();
            c.append("event.a", serde_json::json!({})).expect("append");
            c.append("event.b", serde_json::json!({})).expect("append");
        }
        let reloaded = SealChain::load(path).unwrap();
        assert_eq!(reloaded.len(), 2);
        let (ok, _) = reloaded.verify();
        assert!(ok);
        assert_eq!(reloaded.next_seq, 2);
    }

    #[test]
    fn tamper_detection_flags_modified_record() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        {
            let mut c = SealChain::create(path.clone()).unwrap();
            c.append("event.a", serde_json::json!({})).expect("append");
            c.append("event.b", serde_json::json!({})).expect("append");
            c.append("event.c", serde_json::json!({})).expect("append");
        }
        // Corrupt the middle record's event_type. The recomputed
        // hash will no longer match the stored hash → verify
        // reports the corrupted seq.
        let content = std::fs::read_to_string(&path).unwrap();
        let corrupted = content.replacen("event.b", "event.X", 1);
        std::fs::write(&path, corrupted).unwrap();

        let reloaded = SealChain::load(path).unwrap();
        let (ok, broken) = reloaded.verify();
        assert!(!ok);
        assert_eq!(broken, Some(1));
    }

    #[test]
    fn closed_chain_rejects_further_appends() {
        let dir = tempdir().unwrap();
        let mut c = chain_at(dir.path(), "c.jsonl");
        c.append("event.a", serde_json::json!({})).expect("append");
        // `agent.complete` is the terminal event — recording it
        // auto-closes the chain.
        c.append("agent.complete", serde_json::json!({"exit_code": 0}))
            .expect("terminal append");
        assert!(c.is_closed());
        let result = c.append("event.late", serde_json::json!({}));
        assert!(matches!(result, Err(SealError::Closed)));
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn load_detects_closed_chain_from_terminal_event() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("closed.jsonl");
        {
            let mut c = SealChain::create(path.clone()).unwrap();
            c.append("event.a", serde_json::json!({})).expect("append");
            c.append("agent.failed", serde_json::json!({"reason": "boom"}))
                .expect("terminal append");
        }
        let mut reloaded = SealChain::load(path).unwrap();
        assert!(reloaded.is_closed());
        let result = reloaded.append("event.late", serde_json::json!({}));
        assert!(matches!(result, Err(SealError::Closed)));
    }

    #[test]
    fn tip_hash_returns_last_record_hash() {
        let dir = tempdir().unwrap();
        let mut c = chain_at(dir.path(), "tip.jsonl");
        assert!(c.tip_hash().is_none());
        let r = c
            .append("event.a", serde_json::json!({}))
            .expect("append")
            .clone();
        assert_eq!(c.tip_hash(), Some(r.hash.as_str()));
    }

    #[test]
    fn missing_file_loads_as_empty_chain() {
        let dir = tempdir().unwrap();
        let chain = SealChain::load(dir.path().join("never-created.jsonl")).unwrap();
        assert!(chain.is_empty());
        assert!(!chain.is_closed());
        let (ok, _) = chain.verify();
        assert!(ok);
    }

    #[test]
    fn malformed_lines_skipped_on_load() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mixed.jsonl");
        // Build a file with one valid record + one garbage line +
        // one valid record. The garbage line must be skipped, the
        // two valid records must be preserved and chain.
        {
            let mut c = SealChain::create(path.clone()).unwrap();
            c.append("event.a", serde_json::json!({})).expect("append");
            c.append("event.b", serde_json::json!({})).expect("append");
        }
        let mut content = std::fs::read_to_string(&path).unwrap();
        content.insert_str(content.len() / 2, "this-is-not-json\n");
        std::fs::write(&path, content).unwrap();

        let reloaded = SealChain::load(path).unwrap();
        assert_eq!(reloaded.len(), 2);
        // The chain is still valid because the garbage line is
        // discarded before linkage check.
        let (ok, _) = reloaded.verify();
        assert!(ok);
    }

    #[cfg(unix)]
    #[test]
    fn append_returns_err_when_disk_write_fails() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let chain_path = dir.path().join("ro.jsonl");
        let mut chain = SealChain::create(chain_path.clone()).expect("create chain");

        // First append establishes the file on disk.
        chain
            .append("test.event", serde_json::json!({"k": "v"}))
            .expect("first append ok");

        // Strip write bits on both the file and its parent directory
        // so the next append cannot reopen-for-append. We restore
        // perms before tempdir drops so cleanup succeeds.
        let original_file = std::fs::metadata(&chain_path).unwrap().permissions();
        let original_dir = std::fs::metadata(dir.path()).unwrap().permissions();
        let mut ro_file = original_file.clone();
        ro_file.set_mode(0o444);
        std::fs::set_permissions(&chain_path, ro_file).unwrap();
        let mut ro_dir = original_dir.clone();
        ro_dir.set_mode(0o555);
        std::fs::set_permissions(dir.path(), ro_dir).unwrap();

        let result = chain.append("test.event", serde_json::json!({"k2": "v2"}));

        // Restore perms so the tempdir Drop can clean up.
        std::fs::set_permissions(dir.path(), original_dir).unwrap();
        std::fs::set_permissions(&chain_path, original_file).unwrap();

        assert!(
            matches!(result, Err(SealError::Io(_))),
            "expected Err(Io), got {result:?}"
        );
        // In-memory state must not contain the failed record.
        assert_eq!(
            chain.records().len(),
            1,
            "failed record must not appear in records()"
        );
    }

    #[test]
    fn rfc_id_none_hashes_identically_to_pre_field_records() {
        // A record with rfc_id == None must hash to the same value as
        // a record that never had the field — otherwise pre-existing
        // chains stop verifying after this field is introduced. The
        // hash function must omit the rfc_id segment entirely when None.
        let prev = ZERO_HASH;
        let event = "shell.command";
        let detail = serde_json::json!({"cmd": "ls"});
        let ts = "2026-05-26T00:00:00Z";

        let with_none = compute_hash(prev, event, &detail, ts, None);

        // Hand-compute the legacy hash (without any rfc_id involvement).
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(prev.as_bytes());
        h.update(event.as_bytes());
        h.update(detail.to_string().as_bytes());
        h.update(ts.as_bytes());
        let legacy = hex::encode(h.finalize());

        assert_eq!(
            with_none, legacy,
            "rfc_id: None must be absent from the hash input"
        );

        // And Some(rfc_id) must produce a different hash.
        let rfc = RfcId::new("auth-fix");
        let with_some = compute_hash(prev, event, &detail, ts, Some(&rfc));
        assert_ne!(with_some, legacy);
    }

    #[test]
    fn verify_accepts_mix_of_tagged_and_untagged_records() {
        let dir = tempdir().unwrap();
        let mut c = chain_at(dir.path(), "mixed.jsonl");

        // Untagged (legacy-shaped) record.
        c.append("event.legacy", serde_json::json!({}))
            .expect("legacy");

        // Tagged record (new path).
        c.append_with_rfc(
            "event.tagged",
            serde_json::json!({}),
            Some(RfcId::new("rfc-1")),
        )
        .expect("tagged");

        // Another untagged.
        c.append("event.legacy.2", serde_json::json!({}))
            .expect("legacy2");

        let (ok, broken) = c.verify();
        assert!(ok, "mixed chain must verify; broken at {broken:?}");
        assert_eq!(c.len(), 3);
        assert_eq!(c.records()[0].rfc_id, None);
        assert_eq!(
            c.records()[1].rfc_id.as_ref().map(|r| r.as_str()),
            Some("rfc-1")
        );
        assert_eq!(c.records()[2].rfc_id, None);
    }

    #[test]
    fn append_returns_err_when_chain_is_closed() {
        let dir = tempdir().unwrap();
        let mut c = chain_at(dir.path(), "closed.jsonl");
        c.append("agent.complete", serde_json::json!({"exit_code": 0}))
            .expect("terminal append");
        assert!(c.is_closed());
        let result = c.append("test.event", serde_json::json!({}));
        assert!(matches!(result, Err(SealError::Closed)));
    }
}
