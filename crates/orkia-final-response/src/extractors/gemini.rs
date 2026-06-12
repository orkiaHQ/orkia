// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Gemini CLI transcript extractor.
//!
//! Storage today: `~/.gemini/tmp/<project_hash>/{chats,checkpoints}/…`.
//! Legacy files are a single JSON array; the in-flight migration
//! (gemini-cli #15292) is JSONL. We sniff the first non-whitespace byte
//! and walk both shapes the same way.
//!
//! Content schema: each record has `role` ("user" | "model" | "tool")
//! and a `parts[]` array of typed blocks. Assistant text lives in
//! blocks with a top-level `text` string field; `functionCall` /
//! `functionResponse` parts are skipped.
//!
//! Session-to-file mapping is heuristic: Gemini does not embed a
//! session id in the filename. We pick the most-recently-modified file
//! under the project-hash subtree, falling back to a global mtime scan
//! under `~/.gemini/tmp/` if the project hash cannot be located.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::extractor::{ExtractionContext, ExtractionError, TranscriptExtractor};

pub struct GeminiExtractor;

impl TranscriptExtractor for GeminiExtractor {
    fn extract_final_assistant_text(
        &self,
        ctx: &ExtractionContext,
    ) -> Result<String, ExtractionError> {
        let path = resolve_path(ctx).ok_or(ExtractionError::TranscriptNotFound)?;
        extract_from_path(&path)
    }
}

fn sniff_first_byte(path: &Path) -> Result<(Option<u8>, File), ExtractionError> {
    let mut file = File::open(path).map_err(map_open_err)?;
    let mut head = [0u8; 16];
    let n = file
        .read(&mut head)
        .map_err(ExtractionError::TranscriptUnreadable)?;
    let first = head[..n].iter().find(|b| !b.is_ascii_whitespace()).copied();
    // Rewind by reopening.
    let file = File::open(path).map_err(map_open_err)?;
    Ok((first, file))
}

fn read_json_array_capped(mut file: File, cap: usize) -> Result<Vec<Value>, ExtractionError> {
    let mut s = String::with_capacity(8 * 1024);
    use std::io::Read;
    let n = file
        .by_ref()
        .take(cap as u64 + 1)
        .read_to_string(&mut s)
        .map_err(ExtractionError::TranscriptUnreadable)?;
    if n > cap {
        return Err(ExtractionError::MalformedTranscript(format!(
            "gemini transcript exceeds {cap}-byte cap",
        )));
    }
    serde_json::from_str::<Vec<Value>>(&s)
        .map_err(|e| ExtractionError::MalformedTranscript(e.to_string()))
}

fn parse_jsonl(file: File) -> Result<Vec<Value>, ExtractionError> {
    use orkia_shell_types::input_limits::AGENT_TRANSCRIPT_LINE_MAX_BYTES;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(ExtractionError::TranscriptUnreadable)?;
        if line.trim().is_empty() {
            continue;
        }
        if line.len() > AGENT_TRANSCRIPT_LINE_MAX_BYTES {
            tracing::warn!(
                cap = AGENT_TRANSCRIPT_LINE_MAX_BYTES,
                bytes = line.len(),
                "gemini transcript: skipping oversize line",
            );
            continue;
        }
        match serde_json::from_str::<Value>(&line) {
            Ok(v) => out.push(v),
            // Log only offset, not the full Display which may include a
            // transcript excerpt (SEC-075).
            Err(e) => {
                tracing::debug!(
                    line = e.line(),
                    column = e.column(),
                    "gemini transcript: skipping malformed line",
                );
            }
        }
    }
    Ok(out)
}

fn pick_last_model(records: &[Value]) -> Result<String, ExtractionError> {
    let mut last_model: Option<String> = None;
    let mut saw_model = false;
    for rec in records {
        // The migrating JSONL format wraps the content record under a
        // top-level field; legacy array entries are the content record
        // directly. Try both.
        let content = if rec.get("role").is_some() {
            rec
        } else if let Some(c) = rec.get("content") {
            c
        } else if let Some(c) = rec.get("message") {
            c
        } else {
            continue;
        };
        if content.get("role").and_then(Value::as_str) != Some("model") {
            continue;
        }
        saw_model = true;
        last_model = Some(collect_parts_text(content));
    }
    match last_model {
        Some(t) => Ok(t.trim().to_string()),
        None if saw_model => Ok(String::new()),
        None => Err(ExtractionError::NoAssistantMessage),
    }
}

fn extract_from_path(path: &Path) -> Result<String, ExtractionError> {
    use orkia_shell_types::input_limits::AGENT_TRANSCRIPT_LINE_MAX_BYTES;
    let (first, file) = sniff_first_byte(path)?;
    let records: Vec<Value> = match first {
        Some(b'[') => {
            // Whole-file JSON array — cap the total read at 64 ×
            // line-cap so a giant array still gets size-limited.
            let array_cap = AGENT_TRANSCRIPT_LINE_MAX_BYTES.saturating_mul(64);
            read_json_array_capped(file, array_cap)?
        }
        Some(b'{') => parse_jsonl(file)?,
        _ => {
            return Err(ExtractionError::MalformedTranscript(
                "empty or non-JSON".into(),
            ));
        }
    };
    pick_last_model(&records)
}

fn collect_parts_text(content: &Value) -> String {
    let Some(arr) = content.get("parts").and_then(Value::as_array) else {
        return String::new();
    };
    let mut out = String::new();
    for part in arr {
        // Only text parts. Tool calls / responses live under
        // `functionCall` / `functionResponse` and are skipped.
        let Some(text) = part.get("text").and_then(Value::as_str) else {
            continue;
        };
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(text);
    }
    out
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
        // Hint present but failed confinement — fall through to
        // newest-transcript scan (fail-closed on the hint).
        tracing::debug!("gemini: transcript_path_hint rejected; falling back to newest scan");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let tmp_root = home.join(".gemini").join("tmp");
    if !tmp_root.exists() {
        return None;
    }
    newest_transcript(&tmp_root)
}

/// Canonicalize `hint` and verify it lives under `~/.gemini/tmp`.
/// Rejects symlink escapes because `canonicalize` resolves them before
/// the prefix check. Returns `None` if the hint does not exist, cannot
/// be canonicalized, or is outside the allowed root.
fn confine_hint(hint: &Path, root_override: Option<&Path>) -> Option<PathBuf> {
    let canonical = hint.canonicalize().ok()?;
    let allowed_root = match root_override {
        Some(r) => r.to_path_buf(),
        None => std::env::var_os("HOME")
            .map(PathBuf::from)?
            .join(".gemini")
            .join("tmp"),
    };
    let allowed_canonical = allowed_root.canonicalize().ok()?;
    if canonical.starts_with(&allowed_canonical) {
        Some(canonical)
    } else {
        None
    }
}

fn newest_transcript(root: &Path) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    walk(root, &mut |p| {
        if !p.is_file() {
            return;
        }
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
            return;
        };
        // Catch both legacy `.json` and migrating `.jsonl` files under
        // chats/ and checkpoints/.
        if !(name.ends_with(".json") || name.ends_with(".jsonl")) {
            return;
        }
        let Ok(meta) = p.metadata() else { return };
        let Ok(mtime) = meta.modified() else { return };
        match &best {
            Some((t, _)) if *t >= mtime => {}
            _ => best = Some((mtime, p.to_path_buf())),
        }
    });
    best.map(|(_, p)| p)
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

    fn write(path: &Path, content: &str) {
        let mut f = File::create(path).expect("create");
        f.write_all(content.as_bytes()).expect("write");
    }

    fn ctx(path: PathBuf) -> ExtractionContext {
        let confine_root = path.parent().map(Path::to_path_buf);
        ExtractionContext {
            job_id: 1,
            agent: "faye".into(),
            session_id: None,
            transcript_path_hint: Some(path),
            spawn_cwd: None,
            confine_root,
        }
    }

    #[test]
    fn extracts_from_legacy_json_array() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("chat.json");
        write(
            &path,
            r#"[
              {"role":"user","parts":[{"text":"hi"}]},
              {"role":"model","parts":[{"text":"intermediate"},{"functionCall":{"name":"ls","args":{}}}]},
              {"role":"user","parts":[{"functionResponse":{"name":"ls"}}]},
              {"role":"model","parts":[{"text":"final reply"},{"text":"continued"}]}
            ]"#,
        );
        let out = GeminiExtractor
            .extract_final_assistant_text(&ctx(path))
            .expect("ok");
        assert_eq!(out, "final reply\ncontinued");
    }

    #[test]
    fn extracts_from_jsonl() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("session.jsonl");
        write(
            &path,
            r#"{"role":"user","parts":[{"text":"hi"}]}
{"role":"model","parts":[{"text":"hello"}]}
{"role":"user","parts":[{"text":"again"}]}
{"role":"model","parts":[{"functionCall":{"name":"ls"}},{"text":"done"}]}"#,
        );
        let out = GeminiExtractor
            .extract_final_assistant_text(&ctx(path))
            .expect("ok");
        assert_eq!(out, "done");
    }

    #[test]
    fn function_only_model_turn_returns_empty() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("session.jsonl");
        write(
            &path,
            r#"{"role":"model","parts":[{"functionCall":{"name":"ls"}}]}"#,
        );
        let out = GeminiExtractor
            .extract_final_assistant_text(&ctx(path))
            .expect("ok");
        assert_eq!(out, "");
    }

    #[test]
    fn no_model_role_returns_no_message() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("session.jsonl");
        write(&path, r#"{"role":"user","parts":[{"text":"hi"}]}"#);
        let err = GeminiExtractor
            .extract_final_assistant_text(&ctx(path))
            .expect_err("err");
        assert!(matches!(err, ExtractionError::NoAssistantMessage));
    }
}
