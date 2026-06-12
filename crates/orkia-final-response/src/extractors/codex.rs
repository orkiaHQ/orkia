// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Codex CLI transcript extractor.
//!
//! Format: JSONL at
//! `$CODEX_HOME/sessions/<YYYY>/<MM>/<DD>/rollout-<ts>-<session-uuid>.jsonl`
//! (default `$CODEX_HOME = ~/.codex`). Older sessions migrate to
//! `$CODEX_HOME/archived_sessions/rollout-<ts>-<uuid>.jsonl` (flat).
//!
//! Each line is `{"timestamp":…,"type":<kind>,"payload":{…}}`. Two
//! redundant places carry the assistant text — we prefer the convenience
//! event (`event_msg` / `agent_message` with `phase=="final_answer"`)
//! and fall back to the structured response item.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::extractor::{ExtractionContext, ExtractionError, TranscriptExtractor};

pub struct CodexExtractor;

struct ScanResult {
    last_agent_message: Option<String>,
    last_final_answer: Option<String>,
    last_assistant_item: Option<String>,
    saw_any_assistant: bool,
}

fn process_record(v: &Value, res: &mut ScanResult) {
    let rec_type = v.get("type").and_then(Value::as_str);
    let payload = v.get("payload");
    match (rec_type, payload) {
        (Some("event_msg"), Some(p)) => {
            if p.get("type").and_then(Value::as_str) == Some("agent_message")
                && let Some(msg) = p.get("message").and_then(Value::as_str)
            {
                res.saw_any_assistant = true;
                res.last_agent_message = Some(msg.to_string());
                if p.get("phase").and_then(Value::as_str) == Some("final_answer") {
                    res.last_final_answer = Some(msg.to_string());
                }
            }
        }
        (Some("response_item"), Some(p)) => {
            if p.get("type").and_then(Value::as_str) == Some("message")
                && p.get("role").and_then(Value::as_str) == Some("assistant")
            {
                res.saw_any_assistant = true;
                res.last_assistant_item = Some(collect_output_text(p));
            }
        }
        _ => {}
    }
}

fn scan_records(reader: BufReader<File>) -> Result<ScanResult, ExtractionError> {
    use orkia_shell_types::input_limits::AGENT_TRANSCRIPT_LINE_MAX_BYTES;
    let mut res = ScanResult {
        last_agent_message: None,
        last_final_answer: None,
        last_assistant_item: None,
        saw_any_assistant: false,
    };
    for line in reader.lines() {
        let line = line.map_err(ExtractionError::TranscriptUnreadable)?;
        if line.trim().is_empty() {
            continue;
        }
        if line.len() > AGENT_TRANSCRIPT_LINE_MAX_BYTES {
            tracing::warn!(
                cap = AGENT_TRANSCRIPT_LINE_MAX_BYTES,
                bytes = line.len(),
                "codex transcript: skipping oversize line"
            );
            continue;
        }
        match serde_json::from_str::<Value>(&line) {
            Ok(v) => process_record(&v, &mut res),
            // Log only offset, not the full Display which may include a
            // transcript excerpt (SEC-075).
            Err(e) => tracing::debug!(
                line = e.line(),
                column = e.column(),
                "codex transcript: skipping malformed line",
            ),
        }
    }
    Ok(res)
}

fn select_best(res: ScanResult) -> Result<String, ExtractionError> {
    if let Some(t) = res
        .last_final_answer
        .or(res.last_agent_message)
        .or(res.last_assistant_item)
    {
        return Ok(t.trim().to_string());
    }
    if res.saw_any_assistant {
        return Ok(String::new());
    }
    Err(ExtractionError::NoAssistantMessage)
}

impl TranscriptExtractor for CodexExtractor {
    fn extract_final_assistant_text(
        &self,
        ctx: &ExtractionContext,
    ) -> Result<String, ExtractionError> {
        let path = resolve_path(ctx).ok_or(ExtractionError::TranscriptNotFound)?;
        let file = File::open(&path).map_err(map_open_err)?;
        let reader = BufReader::new(file);
        // Two-strategy single pass: prefer final agent_message; fall
        // back to the last assistant response_item.
        let res = scan_records(reader)?;
        select_best(res)
    }
}

fn map_open_err(e: std::io::Error) -> ExtractionError {
    if e.kind() == std::io::ErrorKind::NotFound {
        ExtractionError::TranscriptNotFound
    } else {
        ExtractionError::TranscriptUnreadable(e)
    }
}

fn collect_output_text(payload: &Value) -> String {
    let Some(arr) = payload.get("content").and_then(Value::as_array) else {
        return String::new();
    };
    let mut out = String::new();
    for block in arr {
        if block.get("type").and_then(Value::as_str) != Some("output_text") {
            continue;
        }
        if let Some(t) = block.get("text").and_then(Value::as_str) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(t);
        }
    }
    out
}

/// Canonicalize `hint` and verify it lives under `$CODEX_HOME` (or
/// `~/.codex`). Rejects symlink escapes because `canonicalize` resolves
/// them before the prefix check. Returns `None` if the hint does not
/// exist, cannot be canonicalized, or is outside the allowed root.
fn confine_hint(hint: &Path, root_override: Option<&Path>) -> Option<PathBuf> {
    let canonical = hint.canonicalize().ok()?;
    let allowed_root = match root_override {
        Some(r) => r.to_path_buf(),
        None => codex_home()?,
    };
    let allowed_canonical = allowed_root.canonicalize().ok()?;
    if canonical.starts_with(&allowed_canonical) {
        Some(canonical)
    } else {
        None
    }
}

fn codex_home() -> Option<PathBuf> {
    if let Some(custom) = std::env::var_os("CODEX_HOME") {
        return Some(PathBuf::from(custom));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex"))
}

fn resolve_path(ctx: &ExtractionContext) -> Option<PathBuf> {
    if let Some(hint) = ctx.transcript_path_hint.as_ref() {
        if let Some(confined) = confine_hint(hint, ctx.confine_root.as_deref()) {
            return Some(confined);
        }
        // Hint present but failed confinement — fall through to
        // session_id resolution (fail-closed on the hint).
        tracing::debug!("codex: transcript_path_hint rejected; falling back to session_id scan");
    }
    let home = codex_home()?;
    let sessions = home.join("sessions");
    let archived = home.join("archived_sessions");

    if let Some(uuid) = ctx.session_id.as_deref() {
        if let Some(p) = find_by_uuid(&sessions, uuid) {
            return Some(p);
        }
        if let Some(p) = find_by_uuid(&archived, uuid) {
            return Some(p);
        }
    }
    // No transcript hint and no session_id match: fail closed. Falling back to
    // the globally most-recent rollout would attribute a *different*
    // concurrent job's transcript to this one (Team, `@a | @b`), so we require
    // a real transcript_path/session_id instead (BUG-033).
    None
}

fn find_by_uuid(root: &Path, uuid: &str) -> Option<PathBuf> {
    if !root.exists() {
        return None;
    }
    let needle = format!("-{uuid}.jsonl");
    let mut best: Option<PathBuf> = None;
    walk(root, &mut |p| {
        if p.is_file()
            && let Some(name) = p.file_name().and_then(|n| n.to_str())
            && name.starts_with("rollout-")
            && name.ends_with(&needle)
        {
            best = Some(p.to_path_buf());
        }
    });
    best
}

/// Maximum recursion depth for directory walks. Guards against
/// symlink cycles that could cause a stack overflow (SEC-052).
const WALK_MAX_DEPTH: usize = 8;

fn walk(root: &Path, visit: &mut dyn FnMut(&Path)) {
    walk_depth(root, visit, 0);
}

fn walk_depth(root: &Path, visit: &mut dyn FnMut(&Path), depth: usize) {
    if depth >= WALK_MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        // Use file_type() — does NOT follow symlinks — to avoid
        // following attacker-controlled symlink cycles (SEC-052).
        let Ok(ft) = entry.file_type() else { continue };
        let p = entry.path();
        if ft.is_dir() {
            walk_depth(&p, visit, depth + 1);
        } else if ft.is_file() {
            visit(&p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn ctx(path: PathBuf) -> ExtractionContext {
        let confine_root = path.parent().map(Path::to_path_buf);
        ExtractionContext {
            job_id: 1,
            agent: "faye".into(),
            session_id: Some("uuid-abc".into()),
            transcript_path_hint: Some(path),
            spawn_cwd: None,
            confine_root,
        }
    }

    fn write_lines(dir: &Path, lines: &[&str]) -> PathBuf {
        let path = dir.join("rollout.jsonl");
        let mut f = File::create(&path).expect("create");
        for line in lines {
            writeln!(f, "{line}").expect("write");
        }
        path
    }

    #[test]
    fn prefers_final_answer_agent_message() {
        let dir = tempdir().expect("tempdir");
        let path = write_lines(
            dir.path(),
            &[
                r#"{"type":"session_meta","payload":{}}"#,
                r#"{"type":"response_item","payload":{"type":"reasoning","content":[]}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"structured-answer"}]}}"#,
                r#"{"type":"event_msg","payload":{"type":"agent_message","message":"chatty-non-final"}}"#,
                r#"{"type":"event_msg","payload":{"type":"agent_message","message":"FINAL","phase":"final_answer"}}"#,
            ],
        );
        let out = CodexExtractor
            .extract_final_assistant_text(&ctx(path))
            .expect("ok");
        assert_eq!(out, "FINAL");
    }

    #[test]
    fn falls_back_to_response_item() {
        let dir = tempdir().expect("tempdir");
        let path = write_lines(
            dir.path(),
            &[
                r#"{"type":"response_item","payload":{"type":"reasoning","content":[{"type":"reasoning","text":"think"}]}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"only-structured"}]}}"#,
            ],
        );
        let out = CodexExtractor
            .extract_final_assistant_text(&ctx(path))
            .expect("ok");
        assert_eq!(out, "only-structured");
    }

    #[test]
    fn no_assistant_returns_no_message() {
        let dir = tempdir().expect("tempdir");
        let path = write_lines(dir.path(), &[r#"{"type":"session_meta","payload":{}}"#]);
        let err = CodexExtractor
            .extract_final_assistant_text(&ctx(path))
            .expect_err("err");
        assert!(matches!(err, ExtractionError::NoAssistantMessage));
    }

    #[test]
    fn missing_path_returns_not_found() {
        let ctx = ExtractionContext {
            job_id: 1,
            agent: "faye".into(),
            session_id: None,
            transcript_path_hint: Some(PathBuf::from("/nope/does/not/exist.jsonl")),
            spawn_cwd: None,
            confine_root: None,
        };
        // No CODEX_HOME with sessions either, so resolve_path returns
        // None → NotFound.
        let _ = CodexExtractor.extract_final_assistant_text(&ctx);
    }
}
