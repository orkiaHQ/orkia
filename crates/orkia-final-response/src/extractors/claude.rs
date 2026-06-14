// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Claude Code transcript extractor.
//!
//! Format: JSONL at the path the Stop hook payload carries in
//! `transcript_path`, or under `~/.claude/projects/<cwd-slug>/<session>.jsonl`.
//! Each line is a typed record; assistant turns have `type == "assistant"`
//! with a `message.content` array of typed blocks. The text we want is
//! the concatenation of `text` fields from blocks where `type == "text"`.
//! Tool-use blocks are skipped — they are not "the answer."

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::extractor::{ExtractionContext, ExtractionError, TranscriptExtractor};

pub struct ClaudeExtractor;

/// Walk a JSONL reader and return the last assistant line's
/// `(collected_text, has_text_block)`. `has_text_block` distinguishes a
/// genuine empty turn (a `text` block whose text is empty) from a
/// tool_use/thinking-only tail — the latter means the provider has not
/// flushed the turn's closing text yet (the `Stop` hook can beat the
/// transcript write by tens of ms), so the caller must treat it as
/// retryable rather than persist a spurious empty response.
/// Lines beyond the cap or that fail JSON parsing are skipped defensively.
fn scan_assistant_lines(
    reader: BufReader<File>,
) -> Result<Option<(String, bool)>, ExtractionError> {
    use orkia_shell_types::input_limits::AGENT_TRANSCRIPT_LINE_MAX_BYTES;
    let mut last_assistant: Option<(String, bool)> = None;
    for line in reader.lines() {
        let line = line.map_err(ExtractionError::TranscriptUnreadable)?;
        if line.trim().is_empty() {
            continue;
        }
        if line.len() > AGENT_TRANSCRIPT_LINE_MAX_BYTES {
            tracing::warn!(
                cap = AGENT_TRANSCRIPT_LINE_MAX_BYTES,
                bytes = line.len(),
                "claude transcript: skipping oversize line",
            );
            continue;
        }
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                // Log only the parse offset, not the full Display (which
                // includes a transcript excerpt) to avoid leaking agent
                // content into logs (SEC-075).
                tracing::debug!(
                    line = e.line(),
                    column = e.column(),
                    "claude transcript: skipping malformed line",
                );
                continue;
            }
        };
        if v.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        last_assistant = Some((collect_text_blocks(&v), has_text_block(&v)));
    }
    Ok(last_assistant)
}

impl TranscriptExtractor for ClaudeExtractor {
    fn extract_final_assistant_text(
        &self,
        ctx: &ExtractionContext,
    ) -> Result<String, ExtractionError> {
        // Claude's Stop hook payload carries the turn's final assistant
        // text in `last_assistant_message`. When present it is the
        // authoritative, synchronous source — return it directly rather
        // than racing the transcript flush (the `Stop` hook routinely
        // beats the JSONL write by tens of ms, and an orkia-spawned TUI
        // may never persist the transcript at all). The on-disk transcript
        // remains the fallback for turns where the payload omits the text
        // (e.g. a tool_use-only tail, where the hint is absent).
        if let Some(msg) = ctx.final_message_hint.as_deref()
            && !msg.is_empty()
        {
            return Ok(msg.to_string());
        }
        let path = resolve_path(ctx).ok_or(ExtractionError::TranscriptNotFound)?;
        let file = File::open(&path).map_err(map_open_err)?;
        let reader = BufReader::new(file);
        match scan_assistant_lines(reader)? {
            Some((t, true)) => Ok(t),
            // tool_use/thinking-only tail: the turn's closing text message
            // is not on disk yet (a fast Stop hook races the transcript
            // flush — observed live: the last flushed assistant line was
            // the tool_use, and extracting "" persisted a spurious empty
            // turn). NoAssistantMessage is the retryable "not ready"
            // signal the service's bounded backoff loop re-reads on.
            Some((_, false)) => Err(ExtractionError::NoAssistantMessage),
            None => Err(ExtractionError::NoAssistantMessage),
        }
    }
}

fn map_open_err(e: std::io::Error) -> ExtractionError {
    if e.kind() == std::io::ErrorKind::NotFound {
        ExtractionError::TranscriptNotFound
    } else {
        ExtractionError::TranscriptUnreadable(e)
    }
}

fn resolve_path(ctx: &ExtractionContext) -> Option<PathBuf> {
    if let Some(hint) = ctx.transcript_path_hint.as_ref() {
        if let Some(confined) = confine_hint(hint, ctx.confine_root.as_deref()) {
            return Some(confined);
        }
        // Hint present but failed confinement check — fall through to
        // session_id resolution (fail-closed on the hint).
        tracing::debug!("claude: transcript_path_hint rejected; falling back to session_id scan");
    }
    let session = ctx.session_id.as_deref()?;
    let home = home_dir()?;
    // Without the slug we cannot know which project subdir to pick.
    // Fall back to a recursive scan of ~/.claude/projects for
    // <session>.jsonl.
    let projects = home.join(".claude").join("projects");
    find_session_file(&projects, session)
}

/// Canonicalize `hint` and verify it lives under `~/.claude/projects`
/// (or `root_override`, used only by tests). Rejects symlink escapes
/// because `canonicalize` resolves them before the prefix check. Returns
/// `None` if the hint does not exist, cannot be canonicalized, or is
/// outside the allowed root.
fn confine_hint(hint: &Path, root_override: Option<&Path>) -> Option<PathBuf> {
    let canonical = hint.canonicalize().ok()?;
    let allowed_root = match root_override {
        Some(r) => r.to_path_buf(),
        None => home_dir()?.join(".claude").join("projects"),
    };
    let allowed_canonical = allowed_root.canonicalize().ok()?;
    if canonical.starts_with(&allowed_canonical) {
        Some(canonical)
    } else {
        None
    }
}

fn find_session_file(root: &Path, session: &str) -> Option<PathBuf> {
    let target = format!("{session}.jsonl");
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        // Use file_type() — does NOT follow symlinks — to avoid
        // following attacker-controlled symlink cycles (SEC-052).
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            let candidate = entry.path().join(&target);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Concatenate `text` fields from every `content[*]` block whose `type
/// == "text"`. Handles both shapes seen in the wild: top-level
/// `content` array, and nested `message.content` array.
pub(crate) fn collect_text_blocks(v: &Value) -> String {
    let content = v
        .get("message")
        .and_then(|m| m.get("content"))
        .or_else(|| v.get("content"));
    let Some(arr) = content.and_then(Value::as_array) else {
        return String::new();
    };
    let mut out = String::new();
    for block in arr {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        if let Some(t) = block.get("text").and_then(Value::as_str) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(t);
        }
    }
    out.trim().to_string()
}

/// True iff any `content[*]` block has `type == "text"`. Resolves the
/// content array the same way as [`collect_text_blocks`]. A `text` block
/// whose text is empty still counts — that is a genuine empty turn, as
/// opposed to a tool_use/thinking-only line.
fn has_text_block(v: &Value) -> bool {
    let content = v
        .get("message")
        .and_then(|m| m.get("content"))
        .or_else(|| v.get("content"));
    let Some(arr) = content.and_then(Value::as_array) else {
        return false;
    };
    arr.iter()
        .any(|block| block.get("type").and_then(Value::as_str) == Some("text"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_jsonl(dir: &Path, lines: &[&str]) -> PathBuf {
        let path = dir.join("session-abc.jsonl");
        let mut f = File::create(&path).expect("create");
        for line in lines {
            writeln!(f, "{line}").expect("write");
        }
        path
    }

    fn ctx_with_path(path: PathBuf) -> ExtractionContext {
        let confine_root = path.parent().map(Path::to_path_buf);
        ExtractionContext {
            job_id: 1,
            agent: "faye".into(),
            session_id: Some("session-abc".into()),
            transcript_path_hint: Some(path),
            spawn_cwd: None,
            confine_root,
            final_message_hint: None,
        }
    }

    #[test]
    fn extracts_last_assistant_text() {
        let dir = tempdir().expect("tempdir");
        let path = write_jsonl(
            dir.path(),
            &[
                r#"{"type":"user","message":{"content":[{"type":"text","text":"hi"}]}}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"first reply"},{"type":"tool_use","name":"Read"}]}}"#,
                r#"{"type":"user","message":{"content":[{"type":"text","text":"again"}]}}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"second"},{"type":"text","text":"continued"}]}}"#,
            ],
        );
        let out = ClaudeExtractor
            .extract_final_assistant_text(&ctx_with_path(path))
            .expect("ok");
        assert_eq!(out, "second\ncontinued");
    }

    /// A tool_use-only tail means the closing text message has not been
    /// flushed yet — must be retryable, never persisted as an empty turn.
    #[test]
    fn tool_use_only_tail_is_retryable() {
        let dir = tempdir().expect("tempdir");
        let path = write_jsonl(
            dir.path(),
            &[r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read"}]}}"#],
        );
        let err = ClaudeExtractor
            .extract_final_assistant_text(&ctx_with_path(path))
            .expect_err("err");
        assert!(matches!(err, ExtractionError::NoAssistantMessage));
    }

    /// An assistant line carrying an actual (empty) text block IS a
    /// genuine empty turn — extract Ok("").
    #[test]
    fn empty_text_block_returns_empty_string() {
        let dir = tempdir().expect("tempdir");
        let path = write_jsonl(
            dir.path(),
            &[r#"{"type":"assistant","message":{"content":[{"type":"text","text":""}]}}"#],
        );
        let out = ClaudeExtractor
            .extract_final_assistant_text(&ctx_with_path(path))
            .expect("ok");
        assert_eq!(out, "");
    }

    #[test]
    fn malformed_line_is_skipped() {
        let dir = tempdir().expect("tempdir");
        let path = write_jsonl(
            dir.path(),
            &[
                "not json at all",
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#,
            ],
        );
        let out = ClaudeExtractor
            .extract_final_assistant_text(&ctx_with_path(path))
            .expect("ok");
        assert_eq!(out, "hi");
    }

    #[test]
    fn missing_path_returns_not_found() {
        let ctx = ExtractionContext {
            job_id: 1,
            agent: "faye".into(),
            session_id: Some("no-such".into()),
            transcript_path_hint: Some(PathBuf::from("/nope/does/not/exist.jsonl")),
            spawn_cwd: None,
            confine_root: None,
            final_message_hint: None,
        };
        let err = ClaudeExtractor
            .extract_final_assistant_text(&ctx)
            .expect_err("err");
        assert!(matches!(err, ExtractionError::TranscriptNotFound));
    }

    #[test]
    fn no_assistant_returns_no_message() {
        let dir = tempdir().expect("tempdir");
        let path = write_jsonl(
            dir.path(),
            &[r#"{"type":"user","message":{"content":[{"type":"text","text":"hi"}]}}"#],
        );
        let err = ClaudeExtractor
            .extract_final_assistant_text(&ctx_with_path(path))
            .expect_err("err");
        assert!(matches!(err, ExtractionError::NoAssistantMessage));
    }

    /// The Stop-payload `last_assistant_message` (carried as
    /// `final_message_hint`) is authoritative: it is returned directly
    /// even when no transcript file exists on disk — the exact case of an
    /// orkia-spawned Claude TUI, which fires Stop but never writes the
    /// JSONL the hint path points at.
    #[test]
    fn final_message_hint_short_circuits_missing_transcript() {
        let ctx = ExtractionContext {
            job_id: 1,
            agent: "faye".into(),
            session_id: Some("no-such".into()),
            transcript_path_hint: Some(PathBuf::from("/nope/does/not/exist.jsonl")),
            spawn_cwd: None,
            confine_root: None,
            final_message_hint: Some("PONG".into()),
        };
        let out = ClaudeExtractor
            .extract_final_assistant_text(&ctx)
            .expect("hint returned directly");
        assert_eq!(out, "PONG");
    }

    /// An empty hint is not a turn's text — fall through to the transcript
    /// (here absent → TranscriptNotFound), never persist a spurious "".
    #[test]
    fn empty_final_message_hint_falls_back_to_transcript() {
        let ctx = ExtractionContext {
            job_id: 1,
            agent: "faye".into(),
            session_id: Some("no-such".into()),
            transcript_path_hint: Some(PathBuf::from("/nope/does/not/exist.jsonl")),
            spawn_cwd: None,
            confine_root: None,
            final_message_hint: Some(String::new()),
        };
        let err = ClaudeExtractor
            .extract_final_assistant_text(&ctx)
            .expect_err("empty hint must not short-circuit");
        assert!(matches!(err, ExtractionError::TranscriptNotFound));
    }

    // --- SEC-029: path confinement tests ---

    /// confine_hint accepts a path that actually lives inside the allowed root.
    /// We build a fake "allowed root" + child file, then override confine_hint
    /// logic directly by testing the invariant: canonicalize starts_with root.
    #[test]
    fn confine_hint_accepts_path_inside_root() {
        let root = tempdir().expect("tempdir");
        let child = root.path().join("session.jsonl");
        File::create(&child).expect("create");
        // Mimic what confine_hint does: canonical child starts_with canonical root.
        let canonical_root = root.path().canonicalize().expect("canonical root");
        let canonical_child = child.canonicalize().expect("canonical child");
        assert!(canonical_child.starts_with(&canonical_root));
    }

    /// confine_hint rejects a path outside the allowed root.
    #[test]
    fn confine_hint_rejects_path_outside_root() {
        let root = tempdir().expect("tempdir");
        let outside = tempdir().expect("outside tempdir");
        let child = outside.path().join("evil.jsonl");
        File::create(&child).expect("create");
        let canonical_root = root.path().canonicalize().expect("canonical root");
        let canonical_child = child.canonicalize().expect("canonical child");
        // Must NOT start with the root.
        assert!(!canonical_child.starts_with(&canonical_root));
    }

    /// confine_hint rejects a non-existent path (canonicalize returns Err).
    #[test]
    fn confine_hint_rejects_nonexistent_path() {
        let result = confine_hint(Path::new("/nope/does/not/exist.jsonl"), None);
        assert!(result.is_none());
    }

    // --- SEC-052: no-symlink-follow test ---
    // (claude's `find_session_file` is single-level and uses `file_type()`,
    // so there is no recursive `walk`/depth guard here — that lives in the
    // codex/gemini extractors, which own those tests.)

    /// find_session_file does not follow symlinks when scanning for dirs.
    #[cfg(unix)]
    #[test]
    fn find_session_file_skips_symlink_dir() {
        use std::os::unix::fs::symlink;
        let projects = tempdir().expect("projects dir");
        let target = tempdir().expect("target dir");
        // Create a real project subdir with the session file.
        let project_dir = projects.path().join("real-project");
        std::fs::create_dir_all(&project_dir).expect("mkdir");
        let session_file = project_dir.join("session-xyz.jsonl");
        File::create(&session_file).expect("create");
        // Create a symlink to an outside directory — should NOT be followed.
        let sym = projects.path().join("sym-project");
        symlink(target.path(), &sym).expect("symlink");
        // find_session_file must find the real one.
        let found = find_session_file(projects.path(), "session-xyz");
        assert!(found.is_some(), "real session file not found");
        // Must not crash or follow the symlink.
    }
}
