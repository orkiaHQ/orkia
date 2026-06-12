// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! Three concerns live here:
//!
//! 1. **Parsing**: find the split between the shell prefix and the
//!    trailing `@agent body`. Quote-aware, command-substitution-aware.
//!    `find_shell_agent_split` returns the byte offset of the `|`
//!    that introduces the agent stage; `parse_shell_to_agent` produces
//!    the `(shell, agent, body)` triple. Multi-`@` stages are rejected
//!    here so the dispatcher never sees an invalid `ShellToAgent`.
//!
//! 2. **Execution**: run the shell prefix via `sh -c '<cmd>'` with the
//!    brush-exported env + cwd, capturing stdout cleanly (no PTY
//!    a future `BrushSession::run_capture` can replace this without
//!    touching the dispatcher.
//!
//!    just `<captured>` when the body is empty.

use std::path::Path;
use std::process::Stdio;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// truncated and a warning is surfaced to the user. Configurable in a
/// future revision via `~/.orkia/config.toml`.
pub const MAX_CAPTURED_BYTES: usize = 2 * 1024 * 1024;

/// One stage of a parsed `<shell> | @<agent> [body]` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellToAgentParse {
    pub shell: String,
    pub agent: String,
    pub body: String,
}

/// Parse error from `parse_shell_to_agent`. Kept distinct from
/// `ShellError` so the REPL can route to a dedicated dispatcher
/// without touching the legacy pipeline parser.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    /// The line has no ` | @<name>` suffix that we recognise. Caller
    /// should fall through to the regular classifier.
    #[error("not a shell-to-agent pipe")]
    NotAShellAgentPipe,
    /// Two or more `@<name>` stages on the right — this is the
    /// agent-to-agent pipeline case which lives in Team
    #[error("multi-agent pipelines (@a | @b) are not supported in this edition")]
    MultiAgentPipeline,
    /// `<shell> | @<empty>` — missing agent name.
    #[error("pipeline target missing agent name")]
    MissingAgentName,
}

/// Look for the split between a shell prefix and a trailing `@agent`
/// stage. Returns `Some(byte_index_of_pipe)` for the *last* unquoted
/// `|` immediately followed by whitespace + `@`. Returns `None` if no
/// such split exists.
///
/// Quote-awareness: single and double quotes pause the scan, and
/// `$(...)` / `` `...` `` command substitutions are skipped so a `|`
/// inside `echo "$(cat foo | bar)"` does not falsely split.
pub fn find_shell_agent_split(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;

    // Trackers for the *latest* candidate split found while scanning
    let mut last_split: Option<usize> = None;

    let mut in_single = false;
    let mut in_double = false;
    let mut paren_depth: u32 = 0; // $(...) depth, skipping nested
    let mut backtick = false;

    while i < len {
        let c = bytes[i];

        // Single-quoted strings are opaque to *everything* including
        // backslashes per POSIX. Only a matching `'` ends them.
        if in_single {
            if c == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }

        // Double-quoted strings honour backslash escapes for `"` and
        // `$`. We do NOT need to evaluate `$(...)` here — we just
        // need to not split on `|` inside the substitution.
        if in_double {
            match c {
                b'"' => in_double = false,
                // POSIX: inside double quotes a backslash only escapes `$`,
                // `` ` ``, `"`, `\`, and newline; before anything else it is
                // literal and must not consume the next byte (BUG-N09).
                b'\\'
                    if i + 1 < len
                        && matches!(bytes[i + 1], b'"' | b'$' | b'`' | b'\\' | b'\n') =>
                {
                    i += 2;
                    continue;
                }
                b'$' if i + 1 < len && bytes[i + 1] == b'(' => {
                    paren_depth += 1;
                    i += 2;
                    continue;
                }
                b'`' => backtick = !backtick,
                _ => {}
            }
            i += 1;
            continue;
        }

        // Inside a `$(...)` substitution: ignore everything except
        // nested parens.
        if paren_depth > 0 {
            match c {
                b'(' => paren_depth += 1,
                b')' => paren_depth -= 1,
                _ => {}
            }
            i += 1;
            continue;
        }

        // Inside backticks: ignore everything except the closing tick.
        if backtick {
            if c == b'`' {
                backtick = false;
            }
            i += 1;
            continue;
        }

        match c {
            b'\'' => in_single = true,
            b'"' => in_double = true,
            b'`' => backtick = true,
            b'\\' if i + 1 < len => {
                i += 2;
                continue;
            }
            b'$' if i + 1 < len && bytes[i + 1] == b'(' => {
                paren_depth += 1;
                i += 2;
                continue;
            }
            b'|' => {
                // Skip `||` (logical-or).
                if i + 1 < len && bytes[i + 1] == b'|' {
                    i += 2;
                    continue;
                }
                // Check what follows: whitespace then `@` qualifies.
                let mut j = i + 1;
                while j < len && (bytes[j] == b' ' || bytes[j] == b'\t') {
                    j += 1;
                }
                if j < len && bytes[j] == b'@' {
                    last_split = Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }

    last_split
}

/// Parse a complete `<shell> | @<agent> [body]` line. Returns
/// `NotAShellAgentPipe` if no qualifying split exists — the caller
/// then falls through to the regular classifier. The shell prefix
/// **may** contain `@` characters that look like agent refs (e.g.,
/// `echo @foo`) — they only matter when introduced by `| @`.
pub fn parse_shell_to_agent(line: &str) -> Result<ShellToAgentParse, ParseError> {
    // A leading-`#` line is a shell comment (brush no-op). It must never
    // parse as a pipe of any kind — a comment that merely *mentions*
    // `@a | @b` used to reach the REPL's mixed-pipe catch and spawn a
    // real pipeline job from the comment text.
    if line.trim_start().starts_with('#') {
        return Err(ParseError::NotAShellAgentPipe);
    }
    // never parse as a pipe of any kind (same doctrine as the comment
    // guard). `!cat f | @faye` is a brush byte pipeline, not an agent pipe.
    if line.trim_start().starts_with('!') {
        return Err(ParseError::NotAShellAgentPipe);
    }
    let split = find_shell_agent_split(line).ok_or(ParseError::NotAShellAgentPipe)?;

    let shell = line[..split].trim().to_string();
    if shell.is_empty() {
        // `| @faye` with nothing on the left — not a shell-to-agent
        // pipe, just falls through (the REPL will treat the line as
        // a regular agent invocation or error out).
        return Err(ParseError::NotAShellAgentPipe);
    }

    // Multi-agent on both sides: shell prefix starts with `@`, which
    // happens when the user wrote `@a | @b` (or `@a body | @b`). The
    // right side is by-construction `@<name>` (find_shell_agent_split
    // only returns splits followed by `@`). So this is an agent-to-
    // agent pipeline — Team feature, not Solo.
    if shell.starts_with('@') {
        return Err(ParseError::MultiAgentPipeline);
    }

    // Multi-agent in the middle: the shell prefix itself contains
    // another `| @<name>` (e.g. `foo | @a | @b` — split lands on the
    // second `|`, shell becomes `foo | @a`, which is itself a
    // shell-to-agent pattern). Same Team-required outcome.
    if find_shell_agent_split(&shell).is_some() {
        return Err(ParseError::MultiAgentPipeline);
    }

    // The agent stage: everything after the split pipe.
    let after = line[split + 1..].trim_start();
    // Must start with `@`.
    let after = after
        .strip_prefix('@')
        .ok_or(ParseError::NotAShellAgentPipe)?;

    // A bare `|` inside the agent stage (e.g., `cat f | @faye | grep`)
    // means the agent's output is piped onward. This is not a clean
    // shell-to-agent pipe; fall through so the REPL's type check rejects
    // it as a TypeMismatch (the agent emits a ByteStream, not the
    // structured value a downstream stage expects).
    if has_unquoted_pipe(after) {
        return Err(ParseError::NotAShellAgentPipe);
    }

    // Split agent name from body.
    let mut iter = after.splitn(2, char::is_whitespace);
    let agent = iter.next().unwrap_or("").trim().to_string();
    if agent.is_empty() {
        return Err(ParseError::MissingAgentName);
    }
    let body = iter.next().unwrap_or("").trim().to_string();

    Ok(ShellToAgentParse { shell, agent, body })
}

/// Lightweight scan for *any* unquoted `|` (skipping `||`). Used to
/// reject agent stages that themselves contain a pipe.
fn has_unquoted_pipe(s: &str) -> bool {
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut paren_depth: u32 = 0;
    let mut backtick = false;

    while i < len {
        let c = bytes[i];
        if in_single {
            if c == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            match c {
                b'"' => in_double = false,
                // POSIX: backslash in double quotes only escapes `$` ` " \ and
                // newline; otherwise it is literal (BUG-N09).
                b'\\'
                    if i + 1 < len
                        && matches!(bytes[i + 1], b'"' | b'$' | b'`' | b'\\' | b'\n') =>
                {
                    i += 2;
                    continue;
                }
                _ => {}
            }
            i += 1;
            continue;
        }
        if paren_depth > 0 {
            match c {
                b'(' => paren_depth += 1,
                b')' => paren_depth -= 1,
                _ => {}
            }
            i += 1;
            continue;
        }
        if backtick {
            if c == b'`' {
                backtick = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' => in_single = true,
            b'"' => in_double = true,
            b'`' => backtick = true,
            b'\\' if i + 1 < len => {
                i += 2;
                continue;
            }
            b'$' if i + 1 < len && bytes[i + 1] == b'(' => {
                paren_depth += 1;
                i += 2;
                continue;
            }
            b'|' => {
                if i + 1 < len && bytes[i + 1] == b'|' {
                    i += 2;
                    continue;
                }
                return true;
            }
            _ => {}
        }
        i += 1;
    }
    false
}

/// Result of running the shell prefix.
#[derive(Debug, Clone)]
pub struct CapturedShellOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
    /// True when stdout exceeded `MAX_CAPTURED_BYTES` and the trailing
    /// bytes were dropped.
    pub stdout_truncated: bool,
}

/// As a fallback, this loses brush aliases / functions but keeps the
/// dispatcher straightforward and avoids interleaving the shell
/// stage's output with the REPL renderer. The brush-exported env is
/// passed in by the caller so user `export VAR=...` semantics carry
/// through.
///
/// Stdin is `/dev/null` — the shell prefix in a `<shell> | @agent`
/// composition does not receive user input mid-execution.
pub async fn capture_shell_output(
    cmd: &str,
    env: &[(String, String)],
    cwd: &Path,
) -> std::io::Result<CapturedShellOutput> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Read stdout up to the cap; drain the rest into the void so the
    // child can exit instead of blocking on a full pipe. stderr has no
    // user-visible cap in v1; if the shell stage misbehaves and
    // produces gigabytes of stderr we'd still rather see all of it
    // surfaced to the user than truncate (it stays small in practice).
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("missing stdout pipe"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("missing stderr pipe"))?;

    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let mut truncated = false;
        let mut chunk = [0u8; 8192];
        loop {
            let n = match stdout.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => return Err(e),
            };
            if buf.len() + n <= MAX_CAPTURED_BYTES {
                buf.extend_from_slice(&chunk[..n]);
            } else if buf.len() < MAX_CAPTURED_BYTES {
                let remaining = MAX_CAPTURED_BYTES - buf.len();
                buf.extend_from_slice(&chunk[..remaining]);
                truncated = true;
            } else {
                truncated = true;
                // Continue draining to let the child exit cleanly.
            }
        }
        Ok::<(Vec<u8>, bool), std::io::Error>((buf, truncated))
    });

    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        stderr.read_to_end(&mut buf).await?;
        Ok::<Vec<u8>, std::io::Error>(buf)
    });

    let status = child.wait().await?;
    let (stdout_bytes, stdout_truncated) = stdout_task
        .await
        .map_err(|e| std::io::Error::other(format!("stdout task: {e}")))??;
    let stderr_bytes = stderr_task
        .await
        .map_err(|e| std::io::Error::other(format!("stderr task: {e}")))??;

    Ok(CapturedShellOutput {
        stdout: stdout_bytes,
        stderr: stderr_bytes,
        exit_code: status.code().unwrap_or(-1),
        stdout_truncated,
    })
}

/// Compose the body that gets injected into the agent's stdin. Per
/// do before the data), then a blank line, then the captured bytes
/// verbatim. Empty body = just the captured bytes.
pub fn compose_body(body: &str, captured: &[u8]) -> Vec<u8> {
    let body = body.trim();
    let mut out: Vec<u8> = Vec::with_capacity(body.len() + 2 + captured.len());
    if !body.is_empty() {
        out.extend_from_slice(body.as_bytes());
        out.push(b'\n');
        out.push(b'\n');
    }
    out.extend_from_slice(captured);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── find_shell_agent_split ──────────────────────────────────

    #[test]
    fn splits_simple_pipe() {
        let line = "cat file.md | @faye";
        let idx = find_shell_agent_split(line).expect("split found");
        assert_eq!(&line[idx..idx + 1], "|");
        assert_eq!(line[..idx].trim_end(), "cat file.md");
    }

    #[test]
    fn splits_at_last_pipe_when_multiple() {
        // `cat a | grep b | @faye` should split at the second `|`.
        let line = "cat a | grep b | @faye";
        let idx = find_shell_agent_split(line).expect("split found");
        assert_eq!(line[..idx].trim_end(), "cat a | grep b");
    }

    #[test]
    fn pipe_inside_double_quotes_ignored() {
        let line = r#"echo "foo | bar" | @faye"#;
        let idx = find_shell_agent_split(line).expect("split found");
        assert_eq!(line[..idx].trim_end(), r#"echo "foo | bar""#);
    }

    #[test]
    fn pipe_inside_single_quotes_ignored() {
        let line = "echo 'foo | bar' | @faye";
        let idx = find_shell_agent_split(line).expect("split found");
        assert_eq!(line[..idx].trim_end(), "echo 'foo | bar'");
    }

    #[test]
    fn pipe_inside_command_substitution_ignored() {
        let line = "echo $(cat a | grep b) | @faye";
        let idx = find_shell_agent_split(line).expect("split found");
        assert_eq!(line[..idx].trim_end(), "echo $(cat a | grep b)");
    }

    #[test]
    fn pipe_inside_backticks_ignored() {
        let line = "echo `cat a | grep b` | @faye";
        let idx = find_shell_agent_split(line).expect("split found");
        assert_eq!(line[..idx].trim_end(), "echo `cat a | grep b`");
    }

    #[test]
    fn logical_or_not_a_split() {
        // `false || echo ok` has no shell-agent pipe.
        let line = "false || echo ok";
        assert!(find_shell_agent_split(line).is_none());
    }

    #[test]
    fn no_pipe_no_split() {
        assert!(find_shell_agent_split("ls -la").is_none());
        assert!(find_shell_agent_split("@faye hi").is_none());
        assert!(find_shell_agent_split("").is_none());
    }

    #[test]
    fn pipe_without_at_not_a_split() {
        // `cat a | grep b` is a pure shell pipeline.
        assert!(find_shell_agent_split("cat a | grep b").is_none());
    }

    // ── parse_shell_to_agent ────────────────────────────────────

    #[test]
    fn parses_simple_form() {
        let p = parse_shell_to_agent("cat file.md | @faye").unwrap();
        assert_eq!(p.shell, "cat file.md");
        assert_eq!(p.agent, "faye");
        assert_eq!(p.body, "");
    }

    #[test]
    fn parses_with_body() {
        let p = parse_shell_to_agent("cat file.md | @faye review this carefully").unwrap();
        assert_eq!(p.shell, "cat file.md");
        assert_eq!(p.agent, "faye");
        assert_eq!(p.body, "review this carefully");
    }

    #[test]
    fn parses_multi_stage_shell_prefix() {
        let p = parse_shell_to_agent("cat a | grep b | @sage check").unwrap();
        assert_eq!(p.shell, "cat a | grep b");
        assert_eq!(p.agent, "sage");
        assert_eq!(p.body, "check");
    }

    #[test]
    fn rejects_multi_agent_pipeline() {
        let err = parse_shell_to_agent("foo | @a | @b").unwrap_err();
        assert_eq!(err, ParseError::MultiAgentPipeline);
    }

    #[test]
    fn rejects_agent_on_left() {
        let err = parse_shell_to_agent("@faye foo | grep bar").unwrap_err();
        // The split detector found `| @` only if the right side starts
        // with `@`; here right side is `grep bar` so it should not even
        // be detected as shell-to-agent. We instead expect NotAShellAgentPipe.
        //
        // Actually: split detector looks for `|` followed by `@`. Here
        // `grep` doesn't start with `@`, so no split. Falls through.
        assert_eq!(err, ParseError::NotAShellAgentPipe);
    }

    #[test]
    fn rejects_explicit_agent_to_agent_pipeline() {
        // `@a | @b` is a multi-agent pipeline (Team). The shell prefix
        // is `@a`, which is itself an agent ref — by construction the
        // right side is also `@`-prefixed, so this is agent→agent.
        let err = parse_shell_to_agent("@a | @b").unwrap_err();
        assert_eq!(err, ParseError::MultiAgentPipeline);
    }

    #[test]
    fn rejects_agent_with_body_into_agent() {
        // `@a do thing | @b` — same outcome: agent→agent, Team feature.
        let err = parse_shell_to_agent("@a do thing | @b review").unwrap_err();
        assert_eq!(err, ParseError::MultiAgentPipeline);
    }

    #[test]
    fn rejects_empty_agent_name() {
        // `cat | @ ` — find_shell_agent_split looks for `|` followed
        // by whitespace+`@`. After `@` there's no name. We should
        // detect MissingAgentName.
        let err = parse_shell_to_agent("cat file | @").unwrap_err();
        assert_eq!(err, ParseError::MissingAgentName);
    }

    #[test]
    fn rejects_empty_shell_prefix() {
        let err = parse_shell_to_agent(" | @faye").unwrap_err();
        assert_eq!(err, ParseError::NotAShellAgentPipe);
    }

    #[test]
    fn not_a_pipe_falls_through() {
        assert_eq!(
            parse_shell_to_agent("ls -la").unwrap_err(),
            ParseError::NotAShellAgentPipe,
        );
        assert_eq!(
            parse_shell_to_agent("@faye hi").unwrap_err(),
            ParseError::NotAShellAgentPipe,
        );
    }

    #[test]
    fn comment_lines_never_parse_as_pipes() {
        // Narration comments mentioning pipes must stay comments —
        // not ShellToAgent, and not MultiAgentPipeline (which the REPL
        // turns into a real pipeline job).
        assert_eq!(
            parse_shell_to_agent("# so @a | @b is live — orkia hands the stages to the kernel:")
                .unwrap_err(),
            ParseError::NotAShellAgentPipe,
        );
        assert_eq!(
            parse_shell_to_agent("# cat file.md | @faye review").unwrap_err(),
            ParseError::NotAShellAgentPipe,
        );
        assert_eq!(
            parse_shell_to_agent("  # indented | @sage too").unwrap_err(),
            ParseError::NotAShellAgentPipe,
        );
    }

    #[test]
    fn bang_lines_never_parse_as_pipes() {
        // never be captured as a shell-to-agent pipe (or any pipe).
        assert_eq!(
            parse_shell_to_agent("!echo hi | @faye").unwrap_err(),
            ParseError::NotAShellAgentPipe,
        );
        assert_eq!(
            parse_shell_to_agent("!cat file.md | @faye review").unwrap_err(),
            ParseError::NotAShellAgentPipe,
        );
        assert_eq!(
            parse_shell_to_agent("  !indented | @sage too").unwrap_err(),
            ParseError::NotAShellAgentPipe,
        );
    }

    // ── compose_body ────────────────────────────────────────────

    #[test]
    fn compose_body_with_instruction() {
        let out = compose_body("summarize this", b"hello world\n");
        assert_eq!(out, b"summarize this\n\nhello world\n");
    }

    #[test]
    fn compose_body_empty_instruction() {
        let out = compose_body("", b"hello\n");
        assert_eq!(out, b"hello\n");
    }

    #[test]
    fn compose_body_whitespace_only_instruction() {
        let out = compose_body("   \n  ", b"hello\n");
        // Body is trimmed; whitespace-only counts as empty.
        assert_eq!(out, b"hello\n");
    }

    #[test]
    fn compose_body_preserves_captured_bytes_verbatim() {
        // Binary-ish content (with NUL etc.) is preserved.
        let captured = b"line1\nline2\nbinary\x00bytes";
        let out = compose_body("look at this", captured);
        let mut expected = Vec::from("look at this\n\n".as_bytes());
        expected.extend_from_slice(captured);
        assert_eq!(out, expected);
    }

    // ── capture_shell_output (integration) ──────────────────────

    #[tokio::test]
    async fn capture_basic_stdout() {
        let cwd = std::env::current_dir().unwrap();
        let out = capture_shell_output("printf 'hello\n'", &[], &cwd)
            .await
            .expect("capture ok");
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.stdout, b"hello\n");
        assert!(out.stderr.is_empty());
        assert!(!out.stdout_truncated);
    }

    #[tokio::test]
    async fn capture_propagates_exit_code() {
        let cwd = std::env::current_dir().unwrap();
        let out = capture_shell_output("exit 7", &[], &cwd)
            .await
            .expect("capture ok");
        assert_eq!(out.exit_code, 7);
        assert!(out.stdout.is_empty());
    }

    #[tokio::test]
    async fn capture_separates_stderr_from_stdout() {
        let cwd = std::env::current_dir().unwrap();
        let out =
            capture_shell_output("printf 'on stdout\n'; printf 'on stderr\n' 1>&2", &[], &cwd)
                .await
                .expect("capture ok");
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.stdout, b"on stdout\n");
        assert_eq!(out.stderr, b"on stderr\n");
    }

    #[tokio::test]
    async fn capture_enforces_size_cap() {
        // Generate slightly more than the cap by repeating a string in
        // a single fast write. `head -c` keeps it portable across BSD
        // and GNU coreutils.
        let cwd = std::env::current_dir().unwrap();
        let cap_plus = MAX_CAPTURED_BYTES + 4096;
        let cmd = format!("yes a | head -c {cap_plus}");
        let out = capture_shell_output(&cmd, &[], &cwd)
            .await
            .expect("capture ok");
        assert!(out.stdout_truncated, "expected truncation");
        assert_eq!(out.stdout.len(), MAX_CAPTURED_BYTES);
    }

    #[tokio::test]
    async fn capture_honours_env() {
        let cwd = std::env::current_dir().unwrap();
        let env = vec![("ORKIA_TEST_VAR".to_string(), "hello-pipe".to_string())];
        let out = capture_shell_output("printf '%s\\n' \"$ORKIA_TEST_VAR\"", &env, &cwd)
            .await
            .expect("capture ok");
        assert_eq!(out.stdout, b"hello-pipe\n");
    }
}
