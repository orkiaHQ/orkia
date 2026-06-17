// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Decision log: append-only JSONL records. Full review-flow semantics live
//! the lifecycle counters that the state machine needs (open clarifications,
//! unreviewed design decisions).

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use crate::error::RfcError;
use crate::id::{AgentId, RfcId};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DecisionId(pub String);

impl DecisionId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DecisionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    Clarification,
    ClarificationResolved,
    DesignProposed,
    DesignReviewed,
    /// One dispatch task's final-response, recorded against the RFC
    /// (`SPEC-ORKIA-RFC-DISPATCH` §6). Used as the `kind` tag of a dispatch
    /// SEAL-chain record; the chain is the tamper-evident audit anchor the
    /// task's issue file points into (issues stay the source of truth).
    DispatchOutput,
    /// One dispatch task's acceptance-oracle verdict (SPEC-CONVERGENCE-LOOP-V1):
    /// did the task actually *succeed* (the `accept` command's exit code), per
    /// attempt. Sealed into the same dispatch chain, interleaved with the
    /// `DispatchOutput` records, so a verifier reconstructs the convergence
    /// (failed attempts included), tamper-evident.
    AcceptanceVerdict,
    /// The RFC-level / integration verdict for one fleet round
    /// (SPEC-FLEET-CONVERGENCE-V2): the `[dispatch].accept` command's result
    /// once the DAG drained. Sealed into the dispatch chain so the whole
    /// convergence saga (per round) is reconstructable.
    GlobalVerdict,
    /// One fleet re-plan decision (SPEC-FLEET-CONVERGENCE-V2): after an
    /// integration `GlobalVerdict` failed, what the controller chose to do
    /// (re-run, give up) for that round. Sealed so the whole re-planning saga
    /// — every round and decision — is reconstructable.
    ReplanDecision,
}

impl DecisionKind {
    /// The snake_case wire tag (matches the `Serialize` rename), so a single
    /// definition feeds both serialization and the dispatch SEAL chain.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Clarification => "clarification",
            Self::ClarificationResolved => "clarification_resolved",
            Self::DesignProposed => "design_proposed",
            Self::DesignReviewed => "design_reviewed",
            Self::DispatchOutput => "dispatch_output",
            Self::AcceptanceVerdict => "acceptance_verdict",
            Self::GlobalVerdict => "global_verdict",
            Self::ReplanDecision => "replan_decision",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionStatus {
    Open,
    Resolved,
    Proposed,
    Reviewed,
}

/// One lifecycle event for a decision. The same `id` is reused across all
/// events belonging to one decision so consumers can reconstruct its history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub id: DecisionId,
    pub rfc_id: RfcId,
    pub rfc_version: u32,
    pub ts: DateTime<FixedOffset>,
    pub actor: AgentId,
    pub kind: DecisionKind,
    pub content: serde_json::Value,
    pub status: DecisionStatus,
    pub prev_hash: String,
    pub hash: String,
}

/// Append a decision record to the log. The caller is responsible for
/// computing `prev_hash`/`hash` (which the SEAL chain owns at a higher layer).
pub fn append(path: &Path, record: &DecisionRecord) -> Result<(), RfcError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| RfcError::io("decision_mkdir", e))?;
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| RfcError::io("decision_open", e))?;
    let line = serde_json::to_string(record).map_err(|e| RfcError::io("decision_serialize", e))?;
    writeln!(f, "{line}").map_err(|e| RfcError::io("decision_write", e))?;
    f.sync_all()
        .map_err(|e| RfcError::io("decision_fsync", e))?;
    Ok(())
}

/// Maximum bytes read from a single decision log file. Guards against
/// unbounded memory growth when an agent appends many decisions. A single
/// log entry is ~500 B, so 4 MiB admits ~8 000 entries — generous for any
/// realistic RFC lifecycle. Exceeding this is treated as an I/O error so
/// callers fail-closed rather than materialising an oversized allocation.
/// (SEC-078)
const MAX_DECISION_LOG_BYTES: u64 = 4 * 1024 * 1024; // 4 MiB

/// Read all records from a decision log. Returns an empty Vec if the file
/// does not exist. Rejects files larger than [`MAX_DECISION_LOG_BYTES`].
pub fn read_all(path: &Path) -> Result<Vec<DecisionRecord>, RfcError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let f = std::fs::File::open(path).map_err(|e| RfcError::io("decision_read", e))?;
    let meta = f
        .metadata()
        .map_err(|e| RfcError::io("decision_read_meta", e))?;
    if meta.len() > MAX_DECISION_LOG_BYTES {
        return Err(RfcError::Io {
            operation: "decision_read",
            message: format!(
                "decision log exceeds size cap ({} > {} bytes)",
                meta.len(),
                MAX_DECISION_LOG_BYTES
            ),
        });
    }
    let reader = BufReader::new(f);
    let mut out = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        let line = line.map_err(|e| RfcError::io("decision_read_line", e))?;
        if line.trim().is_empty() {
            continue;
        }
        let rec: DecisionRecord =
            serde_json::from_str(&line).map_err(|e| RfcError::Frontmatter {
                message: format!("decision log line {}: {}", i + 1, e),
            })?;
        out.push(rec);
    }
    Ok(out)
}

/// Counts open clarifications and unreviewed design decisions — the two
/// counters the state machine consumes via TransitionCtx.
#[derive(Debug, Default, Clone, Copy)]
pub struct DecisionCounts {
    pub open_clarifications: u32,
    pub unreviewed_decisions: u32,
}

pub fn counts_from(records: &[DecisionRecord]) -> DecisionCounts {
    use std::collections::HashMap;
    let mut latest: HashMap<&str, &DecisionRecord> = HashMap::new();
    for r in records {
        latest.insert(r.id.as_str(), r);
    }
    let mut counts = DecisionCounts::default();
    for r in latest.values() {
        match (r.kind, r.status) {
            (DecisionKind::Clarification, DecisionStatus::Open) => counts.open_clarifications += 1,
            (DecisionKind::DesignProposed, DecisionStatus::Proposed) => {
                counts.unreviewed_decisions += 1
            }
            _ => {}
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn rec(id: &str, kind: DecisionKind, status: DecisionStatus) -> DecisionRecord {
        DecisionRecord {
            id: DecisionId::new(id),
            rfc_id: RfcId::new("x"),
            rfc_version: 1,
            ts: chrono::Utc::now().fixed_offset(),
            actor: AgentId::new("faye"),
            kind,
            content: serde_json::json!({}),
            status,
            prev_hash: String::new(),
            hash: String::new(),
        }
    }

    #[test]
    fn append_and_read_roundtrip() {
        let dir = tempdir().expect("tmp");
        let path = dir.path().join("d.jsonl");
        append(
            &path,
            &rec("d-1", DecisionKind::Clarification, DecisionStatus::Open),
        )
        .expect("a1");
        append(
            &path,
            &rec(
                "d-1",
                DecisionKind::ClarificationResolved,
                DecisionStatus::Resolved,
            ),
        )
        .expect("a2");
        let all = read_all(&path).expect("read");
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn counts_use_latest_status_per_id() {
        let records = vec![
            rec("d-1", DecisionKind::Clarification, DecisionStatus::Open),
            rec(
                "d-1",
                DecisionKind::ClarificationResolved,
                DecisionStatus::Resolved,
            ),
            rec("d-2", DecisionKind::Clarification, DecisionStatus::Open),
            rec(
                "d-3",
                DecisionKind::DesignProposed,
                DecisionStatus::Proposed,
            ),
        ];
        let c = counts_from(&records);
        assert_eq!(c.open_clarifications, 1);
        assert_eq!(c.unreviewed_decisions, 1);
    }
}
