// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! The dispatch SEAL chain (`SPEC-ORKIA-RFC-DISPATCH` §6).
//!
//! One hash-linked, append-only NDJSON log per RFC at
//! `<rfc_dir>/seal/dispatch.seal.jsonl`. The run [`Driver`](crate::run) is its
//! sole writer (§2 — one owner): a run owns its RFC, and the issue store keeps
//! two runs for the same RFC from overlapping. Each finished task appends one
//! [`DispatchSealRecord`] whose `hash` links to the previous record's, and that
//! hash is stamped into the task's issue (`IssueMeta::seal`).
//!
//! Why this exists: an issue's `status = done` is mutable markdown. The chain
//! makes that claim tamper-evident — the record binds `{task_id,
//! response_sha256, response_path}` into a hash that depends on every prior
//! record, so a verifier can prove the output was recorded, in order, and
//! unaltered. Issues stay the single source of truth; the chain is the audit
//! anchor they point into — NOT a parallel run-state ledger (the run-progress
//! event log was deliberately removed; §1 model-change note).
//!
//! This is the OSS realization of `DecisionKind::DispatchOutput`. When the
//! premium kernel build is unblocked, the same record can move kernel-side
//! (sealed at `advance` and returned), with the issue still carrying its hash.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use orkia_rfc_core::DecisionKind;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const SEAL_FILE: &str = "dispatch.seal.jsonl";

/// Genesis `prev_hash`. No real SHA-256 collides with it (~2⁻²⁵⁶), and a visual
/// scan of the NDJSON spots genesis records instantly. Mirrors the shell's
/// `seal::ZERO_HASH` convention.
const ZERO_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Cap on the chain file read when finding the tip / verifying. A record is
/// ~300 B, so 4 MiB admits ~14 000 tasks — far past any real DAG. Exceeding it
/// is an error so we fail closed (§8) rather than materialize an oversized read.
const MAX_SEAL_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum SealError {
    #[error("dispatch seal {op} failed: {source}")]
    Io {
        op: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("dispatch seal chain is corrupt: {0}")]
    Corrupt(String),
}

impl SealError {
    fn io(op: &'static str, source: std::io::Error) -> Self {
        Self::Io { op, source }
    }
}

/// One sealed dispatch record. Serialized as a single NDJSON line. `kind`
/// discriminates the shape: a [`DecisionKind::DispatchOutput`] carries
/// `response_*`; a [`DecisionKind::AcceptanceVerdict`] carries the verdict
/// fields (`attempt`/`exit_code`/`passed`/`accept_command`). Both share the
/// chain so a verifier reconstructs the convergence in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchSealRecord {
    /// [`DecisionKind::DispatchOutput`] or [`DecisionKind::AcceptanceVerdict`].
    pub kind: DecisionKind,
    pub ts: String,
    pub task_id: String,
    pub agent: String,
    /// 16-hex digest of the captured response (same convention as the
    /// final-response sha). `None` only if the agent produced no output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_sha256: Option<String>,
    /// Output records only; empty on a verdict record.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub response_path: String,
    // ── AcceptanceVerdict-only fields (all None/absent on an output record) ──
    /// Zero-based attempt this verdict judged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    /// Exit code of the `accept` command (`0` ⇒ passed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Whether the oracle passed (`exit_code == 0`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passed: Option<bool>,
    /// The acceptance command that produced this verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accept_command: Option<String>,
    /// GlobalVerdict / ReplanDecision: the fleet round (`0` for the first pass).
    /// `None` on task-level records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<u32>,
    /// ReplanDecision-only: the controller's choice for the round (e.g.
    /// `rerun-all`, `give-up: <reason>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    pub prev_hash: String,
    pub hash: String,
}

/// SHA-256 over the linked fields, hex-encoded. `prev_hash` first makes each
/// record depend on the entire prefix (tamper-evident ordering).
fn compute_hash(prev_hash: &str, task_id: &str, sha: &str, path: &str, ts: &str) -> String {
    let mut h = Sha256::new();
    h.update(prev_hash.as_bytes());
    h.update(DecisionKind::DispatchOutput.as_str().as_bytes());
    h.update(task_id.as_bytes());
    h.update(sha.as_bytes());
    h.update(path.as_bytes());
    h.update(ts.as_bytes());
    hex::encode(h.finalize())
}

/// SHA-256 over an [`DecisionKind::AcceptanceVerdict`] record's linked fields.
/// `prev_hash` first (tamper-evident ordering), then the verdict payload.
fn compute_verdict_hash(
    prev_hash: &str,
    task_id: &str,
    attempt: u32,
    exit_code: i32,
    passed: bool,
    command: &str,
    ts: &str,
) -> String {
    let mut h = Sha256::new();
    h.update(prev_hash.as_bytes());
    h.update(DecisionKind::AcceptanceVerdict.as_str().as_bytes());
    h.update(task_id.as_bytes());
    h.update(attempt.to_le_bytes());
    h.update(exit_code.to_le_bytes());
    h.update([passed as u8]);
    h.update(command.as_bytes());
    h.update(ts.as_bytes());
    hex::encode(h.finalize())
}

/// SHA-256 over a [`DecisionKind::GlobalVerdict`] record's linked fields.
fn compute_global_verdict_hash(
    prev_hash: &str,
    round: u32,
    exit_code: i32,
    passed: bool,
    command: &str,
    ts: &str,
) -> String {
    let mut h = Sha256::new();
    h.update(prev_hash.as_bytes());
    h.update(DecisionKind::GlobalVerdict.as_str().as_bytes());
    h.update(round.to_le_bytes());
    h.update(exit_code.to_le_bytes());
    h.update([passed as u8]);
    h.update(command.as_bytes());
    h.update(ts.as_bytes());
    hex::encode(h.finalize())
}

/// SHA-256 over a [`DecisionKind::ReplanDecision`] record's linked fields.
fn compute_replan_hash(prev_hash: &str, round: u32, decision: &str, ts: &str) -> String {
    let mut h = Sha256::new();
    h.update(prev_hash.as_bytes());
    h.update(DecisionKind::ReplanDecision.as_str().as_bytes());
    h.update(round.to_le_bytes());
    h.update(decision.as_bytes());
    h.update(ts.as_bytes());
    hex::encode(h.finalize())
}

/// Append-only dispatch SEAL chain for one RFC.
pub struct DispatchSeal {
    path: PathBuf,
}

impl DispatchSeal {
    /// Chain for the RFC whose file lives in `rfc_dir`. Sibling of the issue
    /// store (`<rfc_dir>/issues/`); the seal log is `<rfc_dir>/seal/`.
    pub fn new(rfc_dir: &Path) -> Self {
        Self {
            path: rfc_dir.join("seal").join(SEAL_FILE),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Seal one finished task's output, returning the new record's `hash` (the
    /// "seal name" stamped into the issue). Chains onto the current tip; the
    /// first record links to [`ZERO_HASH`]. Fail-closed: a read/parse/write
    /// error propagates so the caller refuses to mark the task `done` without
    /// its audit anchor (§8).
    pub fn seal_output(
        &self,
        task_id: &str,
        agent: &str,
        response_sha256: Option<&str>,
        response_path: &str,
        ts: &str,
    ) -> Result<String, SealError> {
        let prev_hash = self.tip()?.unwrap_or_else(|| ZERO_HASH.to_string());
        let sha = response_sha256.unwrap_or("");
        let hash = compute_hash(&prev_hash, task_id, sha, response_path, ts);
        let record = DispatchSealRecord {
            kind: DecisionKind::DispatchOutput,
            ts: ts.to_string(),
            task_id: task_id.to_string(),
            agent: agent.to_string(),
            response_sha256: response_sha256.map(str::to_string),
            response_path: response_path.to_string(),
            attempt: None,
            exit_code: None,
            passed: None,
            accept_command: None,
            round: None,
            decision: None,
            prev_hash,
            hash: hash.clone(),
        };
        self.append(&record)?;
        Ok(hash)
    }

    /// Seal one task's acceptance-oracle verdict (SPEC-CONVERGENCE-LOOP-V1),
    /// returning the record `hash` (stamped into `IssueMeta::verdict_seal`).
    /// Chains onto the current tip exactly like [`seal_output`]; interleaved
    /// with output records so the convergence (failed attempts included) is
    /// reconstructable. Fail-closed on read/parse/write error.
    #[allow(clippy::too_many_arguments)]
    pub fn seal_verdict(
        &self,
        task_id: &str,
        agent: &str,
        attempt: u32,
        accept_command: &str,
        exit_code: i32,
        passed: bool,
        ts: &str,
    ) -> Result<String, SealError> {
        let prev_hash = self.tip()?.unwrap_or_else(|| ZERO_HASH.to_string());
        let hash = compute_verdict_hash(
            &prev_hash,
            task_id,
            attempt,
            exit_code,
            passed,
            accept_command,
            ts,
        );
        let record = DispatchSealRecord {
            kind: DecisionKind::AcceptanceVerdict,
            ts: ts.to_string(),
            task_id: task_id.to_string(),
            agent: agent.to_string(),
            response_sha256: None,
            response_path: String::new(),
            attempt: Some(attempt),
            exit_code: Some(exit_code),
            passed: Some(passed),
            accept_command: Some(accept_command.to_string()),
            round: None,
            decision: None,
            prev_hash,
            hash: hash.clone(),
        };
        self.append(&record)?;
        Ok(hash)
    }

    /// Seal one fleet round's RFC-level / integration verdict
    /// (SPEC-FLEET-CONVERGENCE-V2): the `[dispatch].accept` result once the DAG
    /// drained. Chains onto the tip like the task-level seals.
    #[allow(clippy::too_many_arguments)]
    pub fn seal_global_verdict(
        &self,
        round: u32,
        accept_command: &str,
        exit_code: i32,
        passed: bool,
        ts: &str,
    ) -> Result<String, SealError> {
        let prev_hash = self.tip()?.unwrap_or_else(|| ZERO_HASH.to_string());
        let hash = compute_global_verdict_hash(
            &prev_hash,
            round,
            exit_code,
            passed,
            accept_command,
            ts,
        );
        let record = DispatchSealRecord {
            kind: DecisionKind::GlobalVerdict,
            ts: ts.to_string(),
            // task_id is the run-level marker for a fleet verdict.
            task_id: "<rfc>".to_string(),
            agent: "<fleet>".to_string(),
            response_sha256: None,
            response_path: String::new(),
            attempt: None,
            exit_code: Some(exit_code),
            passed: Some(passed),
            accept_command: Some(accept_command.to_string()),
            round: Some(round),
            decision: None,
            prev_hash,
            hash: hash.clone(),
        };
        self.append(&record)?;
        Ok(hash)
    }

    /// Seal one fleet re-plan decision (SPEC-FLEET-CONVERGENCE-V2): after an
    /// integration verdict failed, what the controller chose for `round`.
    pub fn seal_replan_decision(
        &self,
        round: u32,
        decision: &str,
        ts: &str,
    ) -> Result<String, SealError> {
        let prev_hash = self.tip()?.unwrap_or_else(|| ZERO_HASH.to_string());
        let hash = compute_replan_hash(&prev_hash, round, decision, ts);
        let record = DispatchSealRecord {
            kind: DecisionKind::ReplanDecision,
            ts: ts.to_string(),
            task_id: "<rfc>".to_string(),
            agent: "<fleet>".to_string(),
            response_sha256: None,
            response_path: String::new(),
            attempt: None,
            exit_code: None,
            passed: None,
            accept_command: None,
            round: Some(round),
            decision: Some(decision.to_string()),
            prev_hash,
            hash: hash.clone(),
        };
        self.append(&record)?;
        Ok(hash)
    }

    /// All records in chain order. Empty if the chain has never been written.
    pub fn records(&self) -> Result<Vec<DispatchSealRecord>, SealError> {
        let raw = match self.read_capped()? {
            Some(s) => s,
            None => return Ok(Vec::new()),
        };
        raw.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                serde_json::from_str::<DispatchSealRecord>(l)
                    .map_err(|e| SealError::Corrupt(format!("unparseable record: {e}")))
            })
            .collect()
    }

    /// Re-walk the chain, recomputing each hash. `Ok(true)` iff every link is
    /// intact (genesis links to [`ZERO_HASH`], each record's `prev_hash` is its
    /// predecessor's `hash`, and each `hash` recomputes from its fields).
    pub fn verify(&self) -> Result<bool, SealError> {
        let records = self.records()?;
        let mut prev = ZERO_HASH;
        for r in &records {
            if r.prev_hash != prev {
                return Ok(false);
            }
            let recomputed = match r.kind {
                DecisionKind::AcceptanceVerdict => compute_verdict_hash(
                    &r.prev_hash,
                    &r.task_id,
                    r.attempt.unwrap_or(0),
                    r.exit_code.unwrap_or(0),
                    r.passed.unwrap_or(false),
                    r.accept_command.as_deref().unwrap_or(""),
                    &r.ts,
                ),
                DecisionKind::GlobalVerdict => compute_global_verdict_hash(
                    &r.prev_hash,
                    r.round.unwrap_or(0),
                    r.exit_code.unwrap_or(0),
                    r.passed.unwrap_or(false),
                    r.accept_command.as_deref().unwrap_or(""),
                    &r.ts,
                ),
                DecisionKind::ReplanDecision => compute_replan_hash(
                    &r.prev_hash,
                    r.round.unwrap_or(0),
                    r.decision.as_deref().unwrap_or(""),
                    &r.ts,
                ),
                _ => {
                    let sha = r.response_sha256.as_deref().unwrap_or("");
                    compute_hash(&r.prev_hash, &r.task_id, sha, &r.response_path, &r.ts)
                }
            };
            if recomputed != r.hash {
                return Ok(false);
            }
            prev = &r.hash;
        }
        Ok(true)
    }

    /// The current tip hash, or `None` for an unwritten chain. Fails closed on a
    /// malformed last line (§8) so a corrupt tail never produces a fork.
    fn tip(&self) -> Result<Option<String>, SealError> {
        let raw = match self.read_capped()? {
            Some(s) => s,
            None => return Ok(None),
        };
        match raw.lines().rfind(|l| !l.trim().is_empty()) {
            None => Ok(None),
            Some(line) => {
                let rec: DispatchSealRecord = serde_json::from_str(line)
                    .map_err(|e| SealError::Corrupt(format!("unparseable tip: {e}")))?;
                Ok(Some(rec.hash))
            }
        }
    }

    /// Read the whole file, rejecting one larger than [`MAX_SEAL_BYTES`].
    /// `None` if the file does not exist yet.
    fn read_capped(&self) -> Result<Option<String>, SealError> {
        let meta = match fs::metadata(&self.path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(SealError::io("stat", e)),
        };
        if meta.len() > MAX_SEAL_BYTES {
            return Err(SealError::Corrupt(format!(
                "chain exceeds {MAX_SEAL_BYTES} bytes"
            )));
        }
        fs::read_to_string(&self.path)
            .map(Some)
            .map_err(|e| SealError::io("read", e))
    }

    /// Durable append: create the `seal/` dir, append one NDJSON line, fsync.
    fn append(&self, record: &DispatchSealRecord) -> Result<(), SealError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|e| SealError::io("mkdir", e))?;
        }
        let line = serde_json::to_string(record)
            .map_err(|e| SealError::Corrupt(format!("serialize: {e}")))?;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| SealError::io("open", e))?;
        writeln!(f, "{line}").map_err(|e| SealError::io("write", e))?;
        f.sync_all().map_err(|e| SealError::io("fsync", e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seal(dir: &Path) -> DispatchSeal {
        DispatchSeal::new(dir)
    }

    #[test]
    fn empty_chain_has_no_tip_and_verifies() {
        let tmp = tempfile::tempdir().unwrap();
        let s = seal(tmp.path());
        assert_eq!(s.tip().unwrap(), None);
        assert!(s.verify().unwrap());
        assert!(s.records().unwrap().is_empty());
    }

    #[test]
    fn records_link_onto_each_other() {
        let tmp = tempfile::tempdir().unwrap();
        let s = seal(tmp.path());
        let h1 = s
            .seal_output("spec-a", "faye", Some("aaaa"), "issues/spec-a.md", "t1")
            .unwrap();
        let h2 = s
            .seal_output("spec-b", "sage", Some("bbbb"), "issues/spec-b.md", "t2")
            .unwrap();
        let recs = s.records().unwrap();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].prev_hash, ZERO_HASH);
        assert_eq!(recs[0].hash, h1);
        assert_eq!(recs[1].prev_hash, h1); // chained
        assert_eq!(recs[1].hash, h2);
        assert!(s.verify().unwrap());
    }

    #[test]
    fn tamper_with_a_field_breaks_verify() {
        let tmp = tempfile::tempdir().unwrap();
        let s = seal(tmp.path());
        s.seal_output("spec-a", "faye", Some("aaaa"), "issues/spec-a.md", "t1")
            .unwrap();
        // Flip the recorded sha without recomputing the hash → chain invalid.
        let raw = fs::read_to_string(s.path()).unwrap();
        let mut rec: DispatchSealRecord = serde_json::from_str(raw.trim()).unwrap();
        rec.response_sha256 = Some("dead".into());
        fs::write(
            s.path(),
            format!("{}\n", serde_json::to_string(&rec).unwrap()),
        )
        .unwrap();
        assert!(!s.verify().unwrap());
    }

    #[test]
    fn no_output_seals_with_empty_sha() {
        let tmp = tempfile::tempdir().unwrap();
        let s = seal(tmp.path());
        let h = s
            .seal_output("spec-a", "faye", None, "issues/spec-a.md", "t1")
            .unwrap();
        let recs = s.records().unwrap();
        assert_eq!(recs[0].response_sha256, None);
        assert_eq!(recs[0].hash, h);
        assert!(s.verify().unwrap());
    }

    #[test]
    fn verdict_records_interleave_with_output_and_verify() {
        let tmp = tempfile::tempdir().unwrap();
        let s = seal(tmp.path());
        // The convergence shape: output(attempt 0) → verdict(fail) →
        // output(attempt 1) → verdict(pass).
        s.seal_output("impl", "faye", Some("aaaa"), "issues/impl.md", "t1")
            .unwrap();
        let v0 = s
            .seal_verdict("impl", "faye", 0, "cargo test", 1, false, "t2")
            .unwrap();
        s.seal_output("impl", "faye", Some("bbbb"), "issues/impl.md", "t3")
            .unwrap();
        let v1 = s
            .seal_verdict("impl", "faye", 1, "cargo test", 0, true, "t4")
            .unwrap();
        let recs = s.records().unwrap();
        assert_eq!(recs.len(), 4);
        assert_eq!(recs[1].kind, DecisionKind::AcceptanceVerdict);
        assert_eq!(recs[1].hash, v0);
        assert_eq!(recs[1].attempt, Some(0));
        assert_eq!(recs[1].passed, Some(false));
        assert_eq!(recs[1].exit_code, Some(1));
        assert_eq!(recs[3].hash, v1);
        assert_eq!(recs[3].passed, Some(true));
        assert!(s.verify().unwrap()); // mixed-kind chain verifies
    }

    #[test]
    fn global_verdict_chains_after_task_records_and_verifies() {
        let tmp = tempfile::tempdir().unwrap();
        let s = seal(tmp.path());
        // A full fleet round: task output + task verdict, then the integration
        // GlobalVerdict for round 0.
        s.seal_output("impl", "faye", Some("aaaa"), "issues/impl.md", "t1")
            .unwrap();
        s.seal_verdict("impl", "faye", 0, "cargo test -p x", 0, true, "t2")
            .unwrap();
        let g = s
            .seal_global_verdict(0, "cargo test --workspace", 1, false, "t3")
            .unwrap();
        let recs = s.records().unwrap();
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[2].kind, DecisionKind::GlobalVerdict);
        assert_eq!(recs[2].hash, g);
        assert_eq!(recs[2].round, Some(0));
        assert_eq!(recs[2].passed, Some(false));
        assert!(s.verify().unwrap()); // output + acceptance + global all verify
    }

    #[test]
    fn tampered_verdict_breaks_verify() {
        let tmp = tempfile::tempdir().unwrap();
        let s = seal(tmp.path());
        s.seal_verdict("impl", "faye", 0, "cargo test", 1, false, "t1")
            .unwrap();
        // Flip `passed` true without recomputing the hash → chain invalid.
        let raw = fs::read_to_string(s.path()).unwrap();
        let mut rec: DispatchSealRecord = serde_json::from_str(raw.trim()).unwrap();
        rec.passed = Some(true);
        fs::write(s.path(), format!("{}\n", serde_json::to_string(&rec).unwrap())).unwrap();
        assert!(!s.verify().unwrap());
    }
}
