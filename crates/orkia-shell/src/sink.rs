// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! Replaces the collecting tail (`into_value → value_to_blocks → emit_block`)
//! with a sink that consumes the final `PipelineData` line by line toward one
//! of two destinations:
//!
//! - [`ExternalSink`] streams each `Value` as deterministic TSV bytes
//!   (`convert::value_to_bytes`, already tested) into an external command's
//!   stdin, then closes it (EOF) so the command terminates — the
//!   `Value → Bytes` boundary. This makes `ork ls | where | grep` run
//!   end-to-end. Early termination survives: when an upstream `first N` drops
//!   its producer, the typed stream ends after N, the sink stops writing, and
//!   the stdin EOF lets the external command finish.
//! - The display path (chunked, incremental — [`chunk_to_blocks`]) lives as a
//!   `&mut self` method on the REPL since it emits through the renderer; this
//!   module owns the pure, testable alignment helper it uses.
//!
//! The sink never calls `into_value`. Collection, where needed, is the
//! upstream command's own responsibility (e.g. `sort-by`); `is_streaming`
//! stays unread.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use futures::stream::StreamExt;
use orkia_shell_types::exec::pipeline_data::{PipelineData, ValueStream};
use orkia_shell_types::{BlockContent, ExecError, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::exec::convert::value_to_bytes;
use crate::exec::display::value_to_blocks;
use crate::shell_agent_pipe::MAX_CAPTURED_BYTES;

/// Display chunk size (Policy C): rows are aligned and emitted in batches of
/// this many, so a long pipeline shows progress while a small result (< this)
/// emits as one chunk identical to the former collecting sink.
pub const DISPLAY_CHUNK_ROWS: usize = 256;

/// A sink that streams the typed output into an external command's stdin.
pub struct ExternalSink {
    pub command: String,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

/// Captured result of the downstream external command.
pub struct ExternalOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
}

impl ExternalSink {
    /// Drive `data` into the external command's stdin, line by line, then
    /// close stdin (EOF). Reads stdout/stderr concurrently to avoid a pipe
    /// deadlock. Never materializes the typed stream; never panics; the stdin
    /// fd is always closed (its `drop`) even on a write error or early stop.
    pub async fn drive(self, data: PipelineData) -> Result<ExternalOutput, ExecError> {
        let runtime = |message: String| ExecError::Runtime {
            command: self.command.clone(),
            message,
        };

        let (mut child, mut stdin, stdout, stderr) = self.spawn_child(&runtime)?;

        // Read both pipes concurrently so the child never blocks writing
        // output while we are blocked writing its input.
        let stdout_task = tokio::spawn(read_capped(stdout));
        let stderr_task = tokio::spawn(read_capped(stderr));

        let stream_error = pump_stdin(&mut stdin, data).await;
        drop(stdin); // EOF — always, even after a break.

        let status = child.wait().await.map_err(|e| runtime(e.to_string()))?;
        // Join the reader tasks but don't `?` their inner read errors yet: the
        // upstream (Value→Bytes) error is the root cause and must take priority
        // over a downstream reader hiccup, which would otherwise mask it
        // (BUG-094).
        let stdout = stdout_task.await.map_err(|e| runtime(e.to_string()))?;
        let stderr = stderr_task.await.map_err(|e| runtime(e.to_string()))?;

        if let Some(e) = stream_error {
            // Surface whatever the child already produced as a diagnostic
            // rather than discarding it silently.
            if let Ok(out) = &stdout
                && !out.is_empty()
            {
                tracing::warn!(
                    partial_stdout = %String::from_utf8_lossy(out),
                    "external sink: upstream error after partial output"
                );
            }
            return Err(e);
        }
        let stdout = stdout?;
        let stderr = stderr?;
        Ok(ExternalOutput {
            stdout,
            stderr,
            exit_code: status.code().unwrap_or(-1),
        })
    }

    /// Spawn the child process and take the three stdio fds.
    fn spawn_child<F>(
        &self,
        runtime: &F,
    ) -> Result<
        (
            tokio::process::Child,
            tokio::process::ChildStdin,
            tokio::process::ChildStdout,
            tokio::process::ChildStderr,
        ),
        ExecError,
    >
    where
        F: Fn(String) -> ExecError,
    {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&self.command)
            .current_dir(&self.cwd)
            .env_clear()
            .envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| runtime(e.to_string()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| runtime("missing stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| runtime("missing stdout".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| runtime("missing stderr".into()))?;
        Ok((child, stdin, stdout, stderr))
    }
}

impl ExternalOutput {
    /// Render the captured output into display blocks: stdout as text, plus
    /// stderr as an error block when the command failed.
    pub fn into_blocks(self) -> Vec<BlockContent> {
        let mut blocks = Vec::new();
        let stdout = String::from_utf8_lossy(&self.stdout);
        let stdout = stdout.trim_end_matches('\n');
        if !stdout.is_empty() {
            blocks.push(BlockContent::Text(stdout.to_string()));
        }
        if self.exit_code != 0 && !self.stderr.is_empty() {
            let stderr = String::from_utf8_lossy(&self.stderr);
            blocks.push(BlockContent::Error(stderr.trim_end().to_string()));
        }
        blocks
    }
}

/// Stream `data` as bytes into `stdin`, one chunk per upstream Value.
/// Returns `Some(ExecError)` on a stream conversion error; broken-pipe
/// (child exited early) is silently swallowed. The caller must `drop(stdin)`
/// after this returns to signal EOF to the child.
async fn pump_stdin(
    stdin: &mut tokio::process::ChildStdin,
    data: PipelineData,
) -> Option<ExecError> {
    let mut bytes = value_to_bytes(data);
    while let Some(chunk) = bytes.next().await {
        match chunk {
            Ok(buf) => {
                if stdin.write_all(&buf).await.is_err() {
                    break; // broken pipe — child exited early, not an error
                }
            }
            Err(e) => return Some(e),
        }
    }
    None
}

/// Read a child pipe up to the capture cap, draining the rest so the child can
/// exit instead of blocking on a full pipe.
async fn read_capped<R>(mut reader: R) -> Result<Vec<u8>, ExecError>
where
    R: AsyncReadExt + Unpin,
{
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = reader
            .read(&mut chunk)
            .await
            .map_err(|e| ExecError::Runtime {
                command: "<external>".to_string(),
                message: e.to_string(),
            })?;
        if n == 0 {
            break;
        }
        if buf.len() < MAX_CAPTURED_BYTES {
            let room = MAX_CAPTURED_BYTES - buf.len();
            buf.extend_from_slice(&chunk[..n.min(room)]);
        }
        // Beyond the cap we keep reading (to drain) but stop appending.
    }
    Ok(buf)
}

/// Align one chunk of rows into display blocks. `include_header` is true only
/// for the first chunk, so the column header appears once. Reuses
/// `value_to_blocks`; subsequent chunks drop the leading header block.
pub fn chunk_to_blocks(rows: &[Value], include_header: bool) -> Vec<BlockContent> {
    let mut blocks = value_to_blocks(&Value::List(rows.to_vec()));
    if !include_header && matches!(blocks.first(), Some(BlockContent::SystemInfo(_))) {
        blocks.remove(0);
    }
    blocks
}

/// producer (fewer than `DISPLAY_CHUNK_ROWS` rows, spaced out in time) still
/// shows its lines at this cadence rather than waiting to fill a size chunk.
/// Short enough to feel real-time, long enough not to thrash the renderer.
pub const DISPLAY_FLUSH_INTERVAL: Duration = Duration::from_millis(100);

/// Drive a `ListStream` to the display, flushing aligned chunks **either** when
/// `DISPLAY_CHUNK_ROWS` accumulate (Policy C size flush) **or** every
/// `flush_interval` (temporal flush) — whichever comes first. The interval tick
/// is *flush-buffer-only*: it emits only what is already buffered, never pulls
/// from the stream and never blocks (`StreamExt::next` is cancel-safe, so a tick
/// firing mid-poll drops the in-flight `next()` without consuming an item), and
/// no-ops on an empty buffer. EOF flushes the remainder and returns. `emit`
/// receives each aligned block. C1 holds: this is a sink-only policy, invisible
pub async fn drive_list_stream<F>(
    mut stream: ValueStream,
    flush_interval: Duration,
    mut emit: F,
) -> Result<(), ExecError>
where
    F: FnMut(BlockContent),
{
    let mut chunk: Vec<Value> = Vec::with_capacity(DISPLAY_CHUNK_ROWS);
    let mut header_emitted = false;
    let mut ticker = tokio::time::interval(flush_interval);
    // The first interval tick is immediate; consume it so a later tick means
    // "flush_interval elapsed since the last flush", not "stream started".
    ticker.tick().await;
    loop {
        tokio::select! {
            item = stream.next() => match item {
                Some(item) => {
                    chunk.push(item?);
                    if chunk.len() >= DISPLAY_CHUNK_ROWS {
                        flush_chunk(&mut chunk, &mut header_emitted, &mut emit);
                    }
                }
                None => {
                    flush_chunk(&mut chunk, &mut header_emitted, &mut emit);
                    break;
                }
            },
            _ = ticker.tick() => {
                flush_chunk(&mut chunk, &mut header_emitted, &mut emit);
            }
        }
    }
    Ok(())
}

/// Flush the buffered rows as aligned blocks (no-op if empty). The column
/// header appears only on the first non-empty flush.
fn flush_chunk<F: FnMut(BlockContent)>(
    chunk: &mut Vec<Value>,
    header_emitted: &mut bool,
    emit: &mut F,
) {
    if chunk.is_empty() {
        return;
    }
    for block in chunk_to_blocks(chunk, !*header_emitted) {
        emit(block);
    }
    *header_emitted = true;
    chunk.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use indexmap::IndexMap;

    use crate::exec::commands::first::First;
    use crate::exec::engine::{PipelineInput, run_plan};
    use crate::exec::probe::{CountingProducer, empty_ctx};
    use crate::exec::registry::CommandRegistry;
    use orkia_shell_types::ParsedStage;

    fn stage(name: &str, args: &[&str]) -> ParsedStage {
        ParsedStage {
            name: name.to_string(),
            raw_args: args.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn external(command: &str) -> ExternalSink {
        ExternalSink {
            command: command.to_string(),
            env: Vec::new(),
            cwd: std::env::temp_dir(),
        }
    }

    fn empty_input() -> PipelineInput {
        PipelineInput {
            data: PipelineData::Empty,
            label: "input".to_string(),
        }
    }

    /// `count | first 10 | cat`: `first` drops the producer after 10, the
    /// typed stream ends, the sink writes ≤10 lines then closes stdin (EOF),
    /// and `cat` terminates. The producer must have stopped at 10.
    #[tokio::test]
    async fn external_early_termination_stops_producer_and_closes_stdin() {
        let producer = Arc::new(CountingProducer::new(1000));
        let mut registry = CommandRegistry::new();
        registry.register(producer.clone());
        registry.register(Arc::new(First));

        let plan = vec![stage("count", &[]), stage("first", &["10"])];
        let data = run_plan(&plan, empty_input(), &empty_ctx(), &registry)
            .await
            .expect("pipeline runs");

        let output = external("cat").drive(data).await.expect("cat runs");

        assert_eq!(
            producer.produced.load(Ordering::SeqCst),
            10,
            "first 10 must stop the producer at 10"
        );
        let lines = String::from_utf8(output.stdout).expect("utf8");
        assert_eq!(
            lines.lines().count(),
            10,
            "cat received exactly 10 lines then EOF"
        );
        assert_eq!(
            output.exit_code, 0,
            "cat exited cleanly (was not left blocking)"
        );
    }

    /// No early termination: the full stream flows through to `cat` line by
    /// line (proves no cap / no materialization failure on a large stream).
    #[tokio::test]
    async fn external_streams_full_stream() {
        let producer = Arc::new(CountingProducer::new(1000));
        let mut registry = CommandRegistry::new();
        registry.register(producer.clone());

        let data = run_plan(
            &[stage("count", &[])],
            empty_input(),
            &empty_ctx(),
            &registry,
        )
        .await
        .expect("runs");
        let output = external("cat").drive(data).await.expect("cat runs");

        assert_eq!(producer.produced.load(Ordering::SeqCst), 1000);
        assert_eq!(
            String::from_utf8(output.stdout)
                .expect("utf8")
                .lines()
                .count(),
            1000
        );
    }

    /// A command that never reads stdin and exits immediately (`true`) must
    /// not panic the sink — the broken pipe is swallowed and stdin closed.
    #[tokio::test]
    async fn external_broken_pipe_is_safe() {
        let producer = Arc::new(CountingProducer::new(100_000));
        let mut registry = CommandRegistry::new();
        registry.register(producer.clone());

        let data = run_plan(
            &[stage("count", &[])],
            empty_input(),
            &empty_ctx(),
            &registry,
        )
        .await
        .expect("runs");
        // `true` reads nothing and exits 0; writing to its stdin breaks.
        let output = external("true").drive(data).await.expect("no panic");
        assert_eq!(output.exit_code, 0);
    }

    fn record(n: i64) -> Value {
        let mut r = IndexMap::new();
        r.insert("n".to_string(), Value::Int(n));
        Value::Record(r)
    }

    #[test]
    fn chunk_header_appears_only_on_first_chunk() {
        let rows = vec![record(1), record(2)];
        let with_header = chunk_to_blocks(&rows, true);
        let without = chunk_to_blocks(&rows, false);

        assert!(matches!(
            with_header.first(),
            Some(BlockContent::SystemInfo(_))
        ));
        assert!(!matches!(
            without.first(),
            Some(BlockContent::SystemInfo(_))
        ));
        // The data rows are otherwise identical (header is the only difference).
        assert_eq!(with_header.len(), without.len() + 1);
    }

    /// below N=256, stream never EOFs) is flushed by the **temporal** tick, not
    /// by size. Deterministic via `tokio::time::pause`/`advance` — no real sleep.
    #[tokio::test(start_paused = true)]
    async fn slow_producer_flushes_on_interval_before_reaching_n() {
        use futures::channel::mpsc;
        use std::sync::Mutex;

        // Sender kept open ⇒ no EOF ⇒ only the interval tick can flush.
        let (tx, rx) = mpsc::unbounded::<Result<Value, ExecError>>();
        for i in 0..3 {
            tx.unbounded_send(Ok(record(i))).unwrap();
        }
        let stream: ValueStream = rx.boxed();

        let collected = Arc::new(Mutex::new(Vec::<BlockContent>::new()));
        let sink = collected.clone();
        let interval = Duration::from_millis(100);
        let handle = tokio::spawn(async move {
            let _ =
                drive_list_stream(stream, interval, move |b| sink.lock().unwrap().push(b)).await;
        });

        // The driver buffers the 3 rows and parks on the select — no size flush
        // (3 << 256), and the temporal interval has not elapsed yet.
        tokio::task::yield_now().await;
        assert!(
            collected.lock().unwrap().is_empty(),
            "below N and before the interval — nothing flushed yet"
        );

        // Advance past the flush interval: the tick fires and flushes the buffer.
        tokio::time::advance(interval + Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        let blocks = collected.lock().unwrap().clone();
        assert!(
            !blocks.is_empty(),
            "the interval tick flushed the buffered rows before reaching N"
        );
        // All 3 rows emitted (header + 3 data lines on the first flush).
        let text_rows = blocks
            .iter()
            .filter(|b| matches!(b, BlockContent::TableRow(_)))
            .count();
        assert_eq!(text_rows, 3, "all 3 buffered rows emitted; got {blocks:?}");

        drop(tx); // EOF ⇒ the driver completes.
        let _ = handle.await;
    }

    /// Non-regression: a fast producer of exactly N rows still flushes by
    /// **size** (Policy C), independent of any timer — verified without
    /// advancing the (paused) clock at all.
    #[tokio::test(start_paused = true)]
    async fn fast_producer_flushes_by_size_without_timer() {
        use std::sync::Mutex;
        let rows: Vec<Result<Value, ExecError>> = (0..DISPLAY_CHUNK_ROWS as i64)
            .map(|i| Ok(record(i)))
            .collect();
        let stream: ValueStream = futures::stream::iter(rows).boxed();

        let collected = Arc::new(Mutex::new(Vec::<BlockContent>::new()));
        let sink = collected.clone();
        drive_list_stream(stream, Duration::from_secs(3600), move |b| {
            sink.lock().unwrap().push(b)
        })
        .await
        .expect("drive");
        // Size flush emitted the header + N data rows (the timer is 1h away and
        // the stream EOF'd, so the size threshold — not the tick — did the work).
        let text_rows = collected
            .lock()
            .unwrap()
            .iter()
            .filter(|b| matches!(b, BlockContent::TableRow(_)))
            .count();
        assert_eq!(text_rows, DISPLAY_CHUNK_ROWS, "all N rows flushed by size");
    }
}
