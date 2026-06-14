// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file for terms.

//! The issues store: the **single source of truth** for one RFC's dispatch
//! run (`SPEC-ORKIA-RFC-DISPATCH` §4). One human-readable file per task at
//! `<rfc_dir>/issues/<id>.md` — beside the RFC, exactly like a team turns an
//! RFC into issues. A run-level `<rfc_dir>/issues/.run.toml` carries the
//! run identity.
//!
//! Each issue is `+++`-delimited TOML frontmatter (the RFC's own format) plus
//! a body that carries the composed prompt and, once the task finishes, the
//! captured agent response. That one artifact does three jobs:
//!
//!   * **Durability / reconstruction** — the frontmatter's `status` / `job_id`
//!     / `agent` / `depends_on` / `response_sha` let a daemon restart rebuild
//!     the whole run by scanning the directory; no separate event log.
//!   * **Composition** — a dependent task reads its dependencies' embedded
//!     responses straight from their issue files.
//!   * **Aggregation** — the finished issue *is* the reviewable deliverable.
//!
//! Every write is atomic (tmp + fsync + rename) so a crash never leaves a
//! torn issue. Every read is defensive (§7): a malformed file is surfaced as
//! an error, never a panic.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const FM_DELIM: &str = "+++";
const PROMPT_MARKER: &str = "<!-- orkia:prompt -->";
const RESPONSE_MARKER: &str = "<!-- orkia:response -->";
const RUN_FILE: &str = ".run.toml";

/// A task's lifecycle status, serialized lowercase in the frontmatter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Declared in the plan, not yet spawned.
    Pending,
    /// Handed to the detached spawner; `job_id` is set.
    Spawned,
    /// Final response captured and embedded.
    Done,
    /// Failed (no response, unreadable output, or lost on restart).
    Failed,
}

/// The `+++` frontmatter of one issue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IssueMeta {
    pub id: String,
    pub title: String,
    pub agent: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_sha: Option<String>,
    /// Dispatch SEAL-chain hash for this task's recorded output
    /// (`SPEC-ORKIA-RFC-DISPATCH` §6). Set together with `status = Done`: it
    /// makes the `done` claim provable against `<rfc_dir>/seal/dispatch.seal.jsonl`
    /// ([`crate::seal::DispatchSeal`]). `None` until the task finishes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seal: Option<String>,
}

/// A whole issue: its frontmatter, the composed prompt, and (once finished)
/// the captured response.
#[derive(Debug, Clone, PartialEq)]
pub struct Issue {
    pub meta: IssueMeta,
    pub prompt: String,
    pub response: Option<String>,
}

impl Issue {
    pub fn is_done(&self) -> bool {
        matches!(self.meta.status, Status::Done)
    }
}

/// Run-level identity, written once at start and updated on (re)authorize /
/// close. Needed because the kernel's DAG state is in-memory: `run_id` keys
/// `advance`, `plan_hash` detects drift, `closed` stops a finished run from
/// being resumed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunMeta {
    pub run_id: String,
    pub plan_hash: String,
    pub strategy: String,
    pub started: String,
    /// `None` while the run is live; `Some(reason)` once terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum IssueError {
    #[error("issues {op} failed: {source}")]
    Io {
        op: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("issue encode failed: {0}")]
    Encode(#[from] toml::ser::Error),
    #[error("issue `{id}` is malformed: {detail}")]
    Malformed { id: String, detail: String },
}

impl IssueError {
    fn io(op: &'static str, source: std::io::Error) -> Self {
        Self::Io { op, source }
    }
}

/// Append-free, atomic-rewrite store rooted at `<rfc_dir>/issues/`.
pub struct IssueStore {
    dir: PathBuf,
}

impl IssueStore {
    /// Store for the RFC whose file lives in `rfc_dir`.
    pub fn new(rfc_dir: &Path) -> Self {
        Self {
            dir: rfc_dir.join("issues"),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// On-disk path of one issue. Public so the actor can name a recovered
    /// task's deliverable when fast-forwarding the kernel on resume.
    pub fn issue_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.md"))
    }

    /// Write (or overwrite) one issue atomically: tmp + fsync + rename, so a
    /// crash mid-write never leaves a torn file (§3 durability).
    pub fn write(&self, issue: &Issue) -> Result<(), IssueError> {
        let body = render(issue)?;
        atomic_write(&self.dir, &self.issue_path(&issue.meta.id), &body)
    }

    /// Read one issue, or `None` if it has not been written yet.
    pub fn read(&self, id: &str) -> Result<Option<Issue>, IssueError> {
        let raw = match fs::read_to_string(self.issue_path(id)) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(IssueError::io("read", e)),
        };
        parse(id, &raw).map(Some)
    }

    /// Every issue in the directory (the full DAG snapshot), id-sorted. A
    /// malformed file aborts the scan — reconstruction must not silently
    /// drop a task it cannot read (§8 fail-closed).
    pub fn list(&self) -> Result<Vec<Issue>, IssueError> {
        let mut out = Vec::new();
        let entries = match fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(IssueError::io("readdir", e)),
        };
        for entry in entries {
            let path = entry.map_err(|e| IssueError::io("readdir", e))?.path();
            let Some(id) = issue_id_of(&path) else {
                continue;
            };
            let raw = fs::read_to_string(&path).map_err(|e| IssueError::io("read", e))?;
            out.push(parse(&id, &raw)?);
        }
        out.sort_by(|a, b| a.meta.id.cmp(&b.meta.id));
        Ok(out)
    }

    /// Persist run identity atomically.
    pub fn write_run(&self, meta: &RunMeta) -> Result<(), IssueError> {
        let body = toml::to_string_pretty(meta)?;
        atomic_write(&self.dir, &self.dir.join(RUN_FILE), &body)
    }

    /// Read run identity, or `None` if this RFC has no run yet.
    pub fn read_run(&self) -> Result<Option<RunMeta>, IssueError> {
        let raw = match fs::read_to_string(self.dir.join(RUN_FILE)) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(IssueError::io("read", e)),
        };
        toml::from_str(&raw)
            .map(Some)
            .map_err(|e| IssueError::Malformed {
                id: RUN_FILE.into(),
                detail: e.to_string(),
            })
    }
}

/// `<id>` for a `*.md` file, or `None` for `.run.toml` / non-issue entries.
fn issue_id_of(path: &Path) -> Option<String> {
    if path.extension().and_then(|e| e.to_str()) != Some("md") {
        return None;
    }
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string)
}

/// Render an issue to its on-disk form: `+++` TOML frontmatter then a body
/// with marker-delimited prompt and response sections.
fn render(issue: &Issue) -> Result<String, IssueError> {
    let fm = toml::to_string_pretty(&issue.meta)?;
    let response = issue.response.as_deref().unwrap_or("");
    Ok(format!(
        "{FM_DELIM}\n{fm}{FM_DELIM}\n\n# {title}\n\n{PROMPT_MARKER}\n{prompt}\n\n{RESPONSE_MARKER}\n{response}\n",
        title = issue.meta.title,
        prompt = issue.prompt,
    ))
}

/// Parse an issue from its on-disk form. Defensive: any structural surprise
/// is an `IssueError::Malformed`, never a panic (§7).
fn parse(id: &str, raw: &str) -> Result<Issue, IssueError> {
    let malformed = |detail: &str| IssueError::Malformed {
        id: id.to_string(),
        detail: detail.to_string(),
    };
    let after_open = raw
        .strip_prefix(FM_DELIM)
        .and_then(|s| s.strip_prefix('\n'))
        .ok_or_else(|| malformed("missing opening +++"))?;
    let close = after_open
        .find(&format!("\n{FM_DELIM}"))
        .ok_or_else(|| malformed("missing closing +++"))?;
    let fm_src = &after_open[..close];
    let body = &after_open[close + 1 + FM_DELIM.len()..];
    let meta: IssueMeta =
        toml::from_str(fm_src).map_err(|e| malformed(&format!("frontmatter: {e}")))?;
    let prompt = section(body, PROMPT_MARKER, Some(RESPONSE_MARKER))
        .ok_or_else(|| malformed("missing prompt section"))?;
    let response = section(body, RESPONSE_MARKER, None).filter(|s| !s.is_empty());
    Ok(Issue {
        meta,
        prompt,
        response,
    })
}

/// Substring between `start` marker and `end` marker (or end-of-string),
/// trimmed. A plain split, not a markdown parse — robust to arbitrary
/// response bytes.
fn section(body: &str, start: &str, end: Option<&str>) -> Option<String> {
    let from = body.find(start)? + start.len();
    let tail = &body[from..];
    let slice = match end {
        Some(e) => tail.split(e).next().unwrap_or(tail),
        None => tail,
    };
    Some(slice.trim().to_string())
}

/// tmp + fsync + rename, creating the directory on first write. Mirrors
/// `RfcStore::write_atomic`.
fn atomic_write(dir: &Path, final_path: &Path, body: &str) -> Result<(), IssueError> {
    fs::create_dir_all(dir).map_err(|e| IssueError::io("mkdir", e))?;
    let tmp = final_path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp).map_err(|e| IssueError::io("create", e))?;
        f.write_all(body.as_bytes())
            .map_err(|e| IssueError::io("write", e))?;
        f.sync_all().map_err(|e| IssueError::io("fsync", e))?;
    }
    fs::rename(&tmp, final_path).map_err(|e| IssueError::io("rename", e))?;
    Ok(())
}

/// First non-empty line of a task body, trimmed of markdown heading marks and
/// capped, as a human title. Falls back to the id.
pub fn title_from_body(body: &str, id: &str) -> String {
    let line = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.trim_start_matches('#').trim());
    match line {
        Some(l) if !l.is_empty() => l.chars().take(80).collect(),
        _ => id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn meta(id: &str, status: Status) -> IssueMeta {
        IssueMeta {
            id: id.into(),
            title: "Design the API".into(),
            agent: "faye".into(),
            depends_on: vec!["t-root".into()],
            status,
            job_id: Some(7),
            response_sha: None,
            seal: None,
        }
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempdir().unwrap();
        let store = IssueStore::new(dir.path());
        let issue = Issue {
            meta: meta("t-api", Status::Spawned),
            prompt: "design it\nwith care".into(),
            response: None,
        };
        store.write(&issue).unwrap();
        let back = store.read("t-api").unwrap().unwrap();
        assert_eq!(back, issue);
        // No stray tmp file.
        assert!(!dir.path().join("issues/t-api.tmp").exists());
    }

    #[test]
    fn response_section_round_trips_arbitrary_bytes() {
        let dir = tempdir().unwrap();
        let store = IssueStore::new(dir.path());
        let issue = Issue {
            meta: meta("t-api", Status::Done),
            prompt: "p".into(),
            // Response containing markdown that must not confuse the parser.
            response: Some("## Heading\n+++ not a delimiter\ndone".into()),
        };
        store.write(&issue).unwrap();
        let back = store.read("t-api").unwrap().unwrap();
        assert_eq!(
            back.response.as_deref(),
            Some("## Heading\n+++ not a delimiter\ndone")
        );
    }

    #[test]
    fn file_is_beside_rfc_under_issues() {
        let dir = tempdir().unwrap();
        let store = IssueStore::new(dir.path());
        store
            .write(&Issue {
                meta: meta("t-api", Status::Pending),
                prompt: "p".into(),
                response: None,
            })
            .unwrap();
        assert!(dir.path().join("issues/t-api.md").exists());
    }

    #[test]
    fn list_skips_run_file_and_sorts() {
        let dir = tempdir().unwrap();
        let store = IssueStore::new(dir.path());
        for id in ["t-c", "t-a", "t-b"] {
            store
                .write(&Issue {
                    meta: meta(id, Status::Pending),
                    prompt: "p".into(),
                    response: None,
                })
                .unwrap();
        }
        store
            .write_run(&RunMeta {
                run_id: "r-001".into(),
                plan_hash: "h".into(),
                strategy: "dag".into(),
                started: "t".into(),
                closed: None,
            })
            .unwrap();
        let ids: Vec<_> = store
            .list()
            .unwrap()
            .into_iter()
            .map(|i| i.meta.id)
            .collect();
        assert_eq!(ids, vec!["t-a", "t-b", "t-c"]);
    }

    #[test]
    fn run_meta_round_trips() {
        let dir = tempdir().unwrap();
        let store = IssueStore::new(dir.path());
        let rm = RunMeta {
            run_id: "r-001".into(),
            plan_hash: "abc".into(),
            strategy: "dag".into(),
            started: "2026-06-13T00:00:00Z".into(),
            closed: None,
        };
        store.write_run(&rm).unwrap();
        assert_eq!(store.read_run().unwrap(), Some(rm));
    }

    #[test]
    fn malformed_frontmatter_is_error_not_panic() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("issues")).unwrap();
        std::fs::write(dir.path().join("issues/t-x.md"), "no frontmatter here").unwrap();
        let store = IssueStore::new(dir.path());
        assert!(matches!(
            store.read("t-x"),
            Err(IssueError::Malformed { .. })
        ));
    }

    #[test]
    fn title_falls_back_to_id() {
        assert_eq!(title_from_body("  \n\n", "t-api"), "t-api");
        assert_eq!(
            title_from_body("# Build the parser\nmore", "t-x"),
            "Build the parser"
        );
    }
}
