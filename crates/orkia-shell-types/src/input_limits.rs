// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Per-boundary input-size caps for JSON parsers at trust boundaries.
//!
//! Every site that runs `serde_json::from_*` on bytes that came from
//! outside the process MUST first establish that the input is bounded.
//! Without a cap, a peer (an agent PTY, an external MCP server, an
//! LLM SSE stream, a hook subprocess) can OOM the process by streaming
//! arbitrary bytes.
//!
//! Two ways to apply the cap:
//!
//! 1. **Reader-side** (preferred): use [`read_line_bounded`] when
//!    parsing from a `BufRead`/`AsyncBufReadExt` — the line never
//!    grows past the cap because the reader returns
//!    `ErrorKind::InvalidData` as soon as it would overflow.
//! 2. **Parser-side**: when the input is already a `&str` / `&[u8]`
//!    you don't control (e.g. test fixtures, hook payloads that
//!    arrive whole), call [`check_len`] before `serde_json::from_*`
//!    so an oversize input drops with a structured error instead of
//!    allocating the entire serde tree.
//!
//! The constants below are the canonical caps. Adjust per-boundary if
//! a real workload pushes against them; do not silently disable.

/// JSON-RPC frame on the MCP / RFC pipe-server channels. Tool-call
/// payloads + JSON-RPC envelope; tens of KiB in normal use.
pub const MCP_FRAME_MAX_BYTES: usize = 256 * 1024;

/// One journal envelope (`PreToolUse` hook, `Stop` event, etc.) on
/// the unix-socket journal listener.
pub const JOURNAL_LINE_MAX_BYTES: usize = 256 * 1024;

/// `approval.request.json` content. Small structured object; tighter
/// cap reflects the request shape, not the actual filesystem limit.
pub const APPROVAL_REQUEST_MAX_BYTES: usize = 64 * 1024;

/// Forge bridge frame on the bidirectional viewer/runner pipe.
pub const FORGE_FRAME_MAX_BYTES: usize = 256 * 1024;

/// One Server-Sent Event chunk from an LLM provider. Some providers
/// stream large code blocks in a single `data:` line.
pub const LLM_SSE_CHUNK_MAX_BYTES: usize = 1024 * 1024;

/// One line of an agent transcript (`claude` `.jsonl`, `codex`
/// `rollout-*.jsonl`, `gemini` JSON arrays). Same shape as SSE.
pub const AGENT_TRANSCRIPT_LINE_MAX_BYTES: usize = 1024 * 1024;

/// JWT / claim / OAuth response body. Tight — claims are small.
pub const AUTH_RESPONSE_MAX_BYTES: usize = 16 * 1024;

/// Hook subprocess single-frame payload (`payload-PID.json` or stdin
/// from the hook driver). Same shape as journal envelopes.
pub const HOOK_PAYLOAD_MAX_BYTES: usize = 256 * 1024;

/// Validate that `input.len() <= cap`. Returns a structured I/O error
/// on overflow so call sites can `?`-propagate without panic.
pub fn check_len(input: &[u8], cap: usize, boundary: &'static str) -> std::io::Result<()> {
    if input.len() > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{boundary}: input exceeds {cap}-byte cap ({} bytes)",
                input.len(),
            ),
        ));
    }
    Ok(())
}

/// Read one line from `reader`, but cap the line at `cap` bytes.
///
/// On success returns the line (including its trailing `\n` when the
/// upstream included one). Returns
/// `Err(io::Error { kind: InvalidData, … })` if a line longer than
/// `cap` would have been read — the caller's expected action is to
/// log + drop the frame, since a partial JSON line is meaningless.
///
/// The trailing `+1` in the `take` argument lets us distinguish
/// "exactly `cap` bytes" (valid) from "more than `cap` bytes
/// available, line truncated" (drop).
pub fn read_line_bounded<R: std::io::BufRead>(
    reader: &mut R,
    cap: usize,
    boundary: &'static str,
) -> std::io::Result<String> {
    use std::io::{BufRead, Read};
    let mut buf = String::new();
    // `Read::take` consumes its receiver by value, so we borrow
    // `reader` mutably via `by_ref` to keep ownership outside the
    // helper. The resulting `Take<&mut R>` implements `BufRead`
    // because `&mut R` does.
    let mut limited = reader.by_ref().take((cap as u64).saturating_add(1));
    let n = limited.read_line(&mut buf)?;
    if n == 0 {
        return Ok(String::new());
    }
    if buf.len() > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{boundary}: line exceeds {cap}-byte cap"),
        ));
    }
    Ok(buf)
}

/// Async callers (`tokio::io::AsyncBufReadExt`) can't import a generic
/// helper without dragging `tokio` into this types crate. The
/// equivalent four-line idiom they use inline is:
///
/// ```ignore
/// use tokio::io::AsyncReadExt;
/// let mut buf = String::new();
/// let n = (&mut reader).take(cap as u64 + 1).read_to_string(&mut buf).await?;
/// orkia_shell_types::input_limits::check_len(buf.as_bytes(), cap, "boundary")?;
/// ```
///
/// or for line-oriented streams the simpler:
///
/// ```ignore
/// let n = reader.read_line(&mut buf).await?;
/// orkia_shell_types::input_limits::check_len(buf.as_bytes(), cap, "boundary")?;
/// ```
///
/// The parser-side `check_len` is the canonical post-read guard.
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn check_len_passes_within_cap() {
        assert!(check_len(b"hello", 16, "test").is_ok());
        assert!(check_len(b"sixteenbyteslong", 16, "test").is_ok());
    }

    #[test]
    fn check_len_rejects_over_cap() {
        let err = check_len(b"too-long-payload", 4, "test").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("test:"));
    }

    #[test]
    fn read_line_bounded_returns_line_within_cap() {
        let input = b"{\"hello\":\"world\"}\n{\"next\":1}\n";
        let mut cursor = Cursor::new(input.to_vec());
        let line = read_line_bounded(&mut cursor, 64, "mcp").unwrap();
        assert_eq!(line, "{\"hello\":\"world\"}\n");
    }

    #[test]
    fn read_line_bounded_rejects_over_cap() {
        let input = b"this-line-is-way-too-long-for-the-cap\n";
        let mut cursor = Cursor::new(input.to_vec());
        let err = read_line_bounded(&mut cursor, 8, "mcp").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn read_line_bounded_at_cap_boundary() {
        // Exactly cap bytes (including newline) — should succeed.
        let line = b"12345678\n"; // 9 bytes total
        let mut cursor = Cursor::new(line.to_vec());
        let out = read_line_bounded(&mut cursor, 9, "test").unwrap();
        assert_eq!(out, "12345678\n");
    }

    #[test]
    fn read_line_bounded_eof_returns_empty() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        let out = read_line_bounded(&mut cursor, 64, "test").unwrap();
        assert!(out.is_empty());
    }
}
