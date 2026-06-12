// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! On-disk persistence for captured agent responses.
//!
//! Each agent job has a run-dir at `<data_dir>/agents/<agent>/jobs/<id>/`.
//! Per turn we write:
//!
//! - `final-response.md.<N>` — immutable, numbered monotonically.
//! - `final-response.md` — a copy of the latest, overwritten each turn.
//! - `final-responses.jsonl` — append-only index (one record per turn).
//!

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Hard cap on the bytes we will persist for one turn. Beyond this, the
/// stored file is truncated with a clear trailer marker. The preview is
/// always derived from the (possibly truncated) bytes.
pub const RESPONSE_BYTE_CAP: usize = 8 * 1024 * 1024;
const TRUNCATION_MARKER: &str = "\n\n[…truncated to 8 MiB final-response cap]\n";
const PREVIEW_CHARS: usize = 280;

#[derive(Debug)]
pub struct WriteOutcome {
    pub current_path: PathBuf,
    pub history_path: PathBuf,
    pub sha256_short: String,
    pub bytes: u64,
    pub preview: String,
    pub turn_index: u32,
}

#[derive(Debug)]
pub enum StorageError {
    Io(std::io::Error),
}

impl From<std::io::Error> for StorageError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for StorageError {}

pub fn run_dir(data_dir: &Path, agent: &str, job_id: u32) -> PathBuf {
    data_dir
        .join("agents")
        .join(agent)
        .join("jobs")
        .join(job_id.to_string())
}

/// Write the response for one turn into the run-dir, returning the
/// metadata that the caller needs to build the journal envelope and the
/// `FinalResponseEvent`.
pub fn persist_response(
    data_dir: &Path,
    agent: &str,
    job_id: u32,
    raw: &str,
) -> Result<WriteOutcome, StorageError> {
    let dir = run_dir(data_dir, agent, job_id);
    std::fs::create_dir_all(&dir)?;

    let (bytes_to_write, was_truncated) = if raw.len() > RESPONSE_BYTE_CAP {
        let head = &raw.as_bytes()[..safe_truncate(raw, RESPONSE_BYTE_CAP)];
        let mut buf = Vec::with_capacity(head.len() + TRUNCATION_MARKER.len());
        buf.extend_from_slice(head);
        buf.extend_from_slice(TRUNCATION_MARKER.as_bytes());
        (buf, true)
    } else {
        (raw.as_bytes().to_vec(), false)
    };

    let turn_index = next_turn_index(&dir);
    let history_path = dir.join(format!("final-response.md.{turn_index}"));
    write_atomic(&history_path, &bytes_to_write)?;

    let current_path = dir.join("final-response.md");
    write_atomic(&current_path, &bytes_to_write)?;

    let sha256_short = sha256_short(&bytes_to_write);
    let bytes = bytes_to_write.len() as u64;
    let preview = build_preview(&bytes_to_write, was_truncated);

    append_index_record(
        &dir,
        &IndexRecord {
            ts: chrono::Utc::now().to_rfc3339(),
            sha: &sha256_short,
            bytes,
            path: &format!("final-response.md.{turn_index}"),
        },
    )?;

    Ok(WriteOutcome {
        current_path,
        history_path,
        sha256_short,
        bytes,
        preview,
        turn_index,
    })
}

/// Find the next monotonic turn index by scanning existing
/// `final-response.md.<N>` files. Cheap (one readdir) and avoids
/// needing an in-memory counter.
fn next_turn_index(dir: &Path) -> u32 {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(e) => {
            // A *transient* read error (not "dir absent") returning 0 would
            // overwrite `final-response.md.0` and desync the JSONL index —
            // surface it instead of pretending this is the first turn (BUG-N06).
            tracing::warn!(dir = %dir.display(), error = %e, "final-response: cannot scan turn dir; turn index may collide");
            return 0;
        }
    };
    let mut max: Option<u32> = None;
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Some(rest) = name.strip_prefix("final-response.md.") else {
            continue;
        };
        let Ok(n) = rest.parse::<u32>() else { continue };
        max = Some(max.map_or(n, |m| m.max(n)));
    }
    max.map_or(0, |m| m + 1)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // Atomic-ish: write to tmp sibling then rename. We do not fsync —
    // the run-dir is not a SEAL chain, lifecycle GC handles cleanup.
    //
    // The tmp name must be unique per call: duplicate Stop ingestion plus
    // FRS instances in separate processes can persist the same turn
    // concurrently, and a shared `.tmp` sibling lets one writer rename the
    // other's file away — the loser's rename then fails ENOENT and drops
    // the response event the pipeline stage waiter needs.
    static WRITE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = WRITE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp.{}.{seq}", std::process::id()));
    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
    }
    let renamed = std::fs::rename(&tmp, path);
    if renamed.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    renamed
}

fn safe_truncate(s: &str, max_bytes: usize) -> usize {
    if s.len() <= max_bytes {
        return s.len();
    }
    let bytes = s.as_bytes();
    let mut end = max_bytes;
    while end > 0 && (bytes[end] & 0b1100_0000) == 0b1000_0000 {
        end -= 1;
    }
    end
}

fn sha256_short(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let hex = digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    hex.chars().take(16).collect()
}

fn build_preview(bytes: &[u8], was_truncated: bool) -> String {
    let s = String::from_utf8_lossy(bytes);
    let mut out: String = s.chars().take(PREVIEW_CHARS).collect();
    let needs_ellipsis =
        s.chars().count() > PREVIEW_CHARS || (was_truncated && !out.ends_with('…'));
    if needs_ellipsis {
        out.push('…');
    }
    out
}

struct IndexRecord<'a> {
    ts: String,
    sha: &'a str,
    bytes: u64,
    path: &'a str,
}

fn append_index_record(dir: &Path, rec: &IndexRecord<'_>) -> std::io::Result<()> {
    let path = dir.join("final-responses.jsonl");
    let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
    // Serialize to a String first and emit ONE write_all: `writeln!`
    // with a `serde_json::Value` streams many tiny write() calls into
    // the unbuffered fd, so two concurrent appenders interleave
    // byte-by-byte and corrupt the line. A single O_APPEND write keeps
    // each line intact regardless of who else holds the file open.
    let mut line = serde_json::json!({
        "ts": rec.ts,
        "sha": rec.sha,
        "bytes": rec.bytes,
        "path": rec.path,
    })
    .to_string();
    line.push('\n');
    f.write_all(line.as_bytes())?;
    Ok(())
}

/// Build the documented failure-preview string for a given extraction
/// error or empty-turn case. Capped at 280 chars.
pub fn failure_preview(reason: &str) -> String {
    let preview = format!("<extraction failed: {reason}>");
    preview.chars().take(PREVIEW_CHARS).collect()
}

pub const EMPTY_TURN_PREVIEW: &str = "<no assistant text in final turn>";

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn persists_first_turn() {
        let dir = tempdir().expect("tempdir");
        let outcome = persist_response(dir.path(), "faye", 7, "hello world").expect("ok");
        assert_eq!(outcome.bytes, "hello world".len() as u64);
        assert_eq!(outcome.turn_index, 0);
        assert_eq!(outcome.sha256_short.len(), 16);
        assert!(outcome.current_path.ends_with("final-response.md"));
        assert!(outcome.history_path.ends_with("final-response.md.0"));
        let read = std::fs::read_to_string(&outcome.current_path).expect("read");
        assert_eq!(read, "hello world");
    }

    #[test]
    fn turn_index_increments() {
        let dir = tempdir().expect("tempdir");
        for n in 0..3 {
            let outcome =
                persist_response(dir.path(), "faye", 1, &format!("turn-{n}")).expect("ok");
            assert_eq!(outcome.turn_index, n as u32);
        }
        let listing: Vec<String> = std::fs::read_dir(run_dir(dir.path(), "faye", 1))
            .expect("dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(listing.contains(&"final-response.md.0".to_string()));
        assert!(listing.contains(&"final-response.md.1".to_string()));
        assert!(listing.contains(&"final-response.md.2".to_string()));
    }

    #[test]
    fn truncation_appends_marker_and_caps_bytes() {
        let dir = tempdir().expect("tempdir");
        let big = "a".repeat(RESPONSE_BYTE_CAP + 1024);
        let outcome = persist_response(dir.path(), "faye", 9, &big).expect("ok");
        assert!(outcome.bytes as usize <= RESPONSE_BYTE_CAP + TRUNCATION_MARKER.len());
        let read = std::fs::read_to_string(&outcome.current_path).expect("read");
        assert!(read.ends_with(TRUNCATION_MARKER));
    }

    #[test]
    fn sha_is_sixteen_hex_chars() {
        let dir = tempdir().expect("tempdir");
        let outcome = persist_response(dir.path(), "x", 1, "abc").expect("ok");
        assert_eq!(outcome.sha256_short.len(), 16);
        assert!(outcome.sha256_short.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn concurrent_persists_to_same_job_all_succeed() {
        // Duplicate Stop ingestion makes two extractions persist the same
        // turn at the same time. With a shared tmp sibling one rename used
        // to fail ENOENT and drop the response event — every persist must
        // succeed regardless of interleaving.
        let dir = tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let handles: Vec<_> = (0..8)
            .map(|n| {
                let root = root.clone();
                std::thread::spawn(move || persist_response(&root, "faye", 3, &format!("r-{n}")))
            })
            .collect();
        for h in handles {
            h.join().expect("join").expect("persist must not race-fail");
        }
        let current = run_dir(&root, "faye", 3).join("final-response.md");
        assert!(
            std::fs::read_to_string(current)
                .expect("read")
                .starts_with("r-")
        );
        // No stray tmp files left behind.
        let leftovers: Vec<String> = std::fs::read_dir(run_dir(&root, "faye", 3))
            .expect("dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "stray tmp files: {leftovers:?}");
    }

    #[test]
    fn index_jsonl_appended() {
        let dir = tempdir().expect("tempdir");
        persist_response(dir.path(), "z", 2, "first").expect("ok");
        persist_response(dir.path(), "z", 2, "second").expect("ok");
        let p = run_dir(dir.path(), "z", 2).join("final-responses.jsonl");
        let s = std::fs::read_to_string(&p).expect("read");
        assert_eq!(s.lines().count(), 2);
    }
}
