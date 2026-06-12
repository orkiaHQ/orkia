// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! APC `Orkia` protocol emitter — orkia → agent.
//!
//! Writes a single `\x1b_Orkia;<json>\x1b\\` sequence into an
//! agent's PTY master. The agent's stdin reader, if it speaks the
//! protocol (i.e. it saw `$ORKIA=1` at startup and has registered
//! its own APC parser), receives the [`EventPayload`] as a typed
//! message. If it does not speak the protocol, the bytes form a
//! valid APC sequence which well-behaved terminals and TUI
//! libraries (Ink, crossterm, blessed) silently ignore — no
//! corruption, no crash, no echo.
//!
//! Use this for the **orkia → agent** half of the protocol:
//! `Inject` (deliver a queued user body), `PermissionResolved`
//! (answer an agent's permission request), `UserMessage` (the
//! `tell` builtin), `Context` (push RFCs / project metadata), etc.

use std::io::Write;

use orkia_pty::SharedWriter;

use super::EventPayload;

/// Serialize `payload` as a JSON-bodied Orkia APC sequence and
/// write it to the agent's PTY master. Returns the JSON length on
/// success — useful for tracing / metrics. Errors fall through from
/// either the JSON serialiser or the PTY write.
pub fn emit_orkia_apc(writer: &SharedWriter, payload: &EventPayload) -> std::io::Result<usize> {
    let json = serde_json::to_string(payload)
        .map_err(|e| std::io::Error::other(format!("orkia APC: serialize: {e}")))?;
    // `\x1b_Orkia;<json>\x1b\\` — JSON serialisers escape every
    // byte that could interfere (0x00-0x1F, 0x7F, 0x22, 0x5C), so
    // raw UTF-8 JSON is safe to embed without base64.
    let mut frame = Vec::with_capacity(json.len() + 10);
    frame.extend_from_slice(b"\x1b_Orkia;");
    frame.extend_from_slice(json.as_bytes());
    frame.extend_from_slice(b"\x1b\\");
    let mut w = writer.lock();
    w.write_all(&frame)?;
    w.flush()?;
    Ok(json.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::Arc;

    /// Test sink that captures writes into a shared Vec we can
    /// inspect after the call. Wraps the Vec twice: once as the
    /// `Box<dyn Write>` target (forwarder), once as the inspection
    /// handle.
    struct TestSink {
        buf: Arc<Mutex<Vec<u8>>>,
    }
    impl std::io::Write for TestSink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.buf.lock().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    fn fresh_writer() -> (SharedWriter, Arc<Mutex<Vec<u8>>>) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = TestSink {
            buf: Arc::clone(&buf),
        };
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(sink)));
        (writer, buf)
    }

    #[test]
    fn frame_starts_and_ends_correctly() {
        let (writer, sink) = fresh_writer();
        let payload = EventPayload::PromptReady;
        let len = emit_orkia_apc(&writer, &payload).expect("ok");
        assert_eq!(len, br#"{"type":"PromptReady"}"#.len());
        let bytes = sink.lock().clone();
        assert!(bytes.starts_with(b"\x1b_Orkia;"), "bad prefix: {bytes:?}");
        assert!(bytes.ends_with(b"\x1b\\"), "bad suffix: {bytes:?}");
        let body = &bytes[b"\x1b_Orkia;".len()..bytes.len() - 2];
        assert_eq!(body, br#"{"type":"PromptReady"}"#);
    }

    #[test]
    fn round_trips_through_parser() {
        let (writer, sink) = fresh_writer();
        let payload = EventPayload::ToolUse {
            tool: "Bash".into(),
            target: Some("ls".into()),
            input_summary: None,
        };
        emit_orkia_apc(&writer, &payload).expect("ok");
        let bytes = sink.lock().clone();
        // Strip just `\x1b_` (2 bytes) and `\x1b\\` (2 bytes) — what
        // remains is what the BlockParser would deliver to its
        // on_apc callback: `Orkia;<json>`.
        assert!(bytes.len() > 4);
        let inner = &bytes[2..bytes.len() - 2];
        let parsed = super::super::apc::parse_orkia_apc(inner).expect("decode");
        match parsed {
            EventPayload::ToolUse {
                tool,
                target,
                input_summary,
            } => {
                assert_eq!(tool, "Bash");
                assert_eq!(target.as_deref(), Some("ls"));
                assert!(input_summary.is_none());
            }
            other => panic!("got {other:?}"),
        }
    }
}
