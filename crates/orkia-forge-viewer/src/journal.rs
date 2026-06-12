// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Minimal NDJSON client for the journal socket. The viewer process
//! opens a Unix socket connection at startup and writes one JSON object
//! per line — the shell's journal listener accepts the same shape it
//! gets from any other client.
//!
//! V0 only emits `app.window.opened` and `app.window.closed`. V2 will
//! add `app.error` and receive notifications.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("connect {0:?}: {1}")]
    Connect(PathBuf, std::io::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialize: {0}")]
    Serialize(#[from] serde_json::Error),
}

pub struct JournalClient {
    socket_path: PathBuf,
    app_id: String,
    stream: Option<UnixStream>,
}

/// Wire format matching `orkia_shell::journal::types::JournalEnvelope`.
/// We don't depend on the shell crate from the viewer (the viewer is a
/// standalone binary), so we mirror the contract here. Reciprocal:
/// changes to the envelope schema must update both sides.
#[derive(Debug, Serialize)]
struct Envelope<'a, T: Serialize> {
    #[serde(rename = "type")]
    event_type: &'static str,
    timestamp: String,
    source: &'a str,
    session_id: &'a str,
    event: &'a str,
    #[serde(flatten)]
    data: T,
}

impl JournalClient {
    /// Best-effort connect. If the socket is missing we keep the client
    /// and silently drop events — the journal is informational, not
    /// load-bearing for the user's app session.
    pub fn connect(socket_path: &Path, app_id: &str) -> Self {
        let stream = UnixStream::connect(socket_path).ok();
        Self {
            socket_path: socket_path.to_path_buf(),
            app_id: app_id.to_string(),
            stream,
        }
    }

    pub fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    pub fn emit<T: Serialize>(&mut self, event: &str, data: T) -> Result<(), JournalError> {
        let Some(stream) = self.stream.as_mut() else {
            return Ok(());
        };
        let env = Envelope {
            event_type: "lifecycle",
            timestamp: chrono::Utc::now().to_rfc3339(),
            source: "forge-viewer",
            session_id: &self.app_id,
            event,
            data,
        };
        let mut line = serde_json::to_vec(&env)?;
        line.push(b'\n');
        stream.write_all(&line)?;
        stream.flush()?;
        Ok(())
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_socket_is_disconnected_but_safe() {
        let mut c = JournalClient::connect(
            std::path::Path::new("/nonexistent/orkia.sock"),
            "orkia.forge.x",
        );
        assert!(!c.is_connected());
        // emit must not error on a disconnected client — drop is silent.
        c.emit("app.window.opened", serde_json::json!({"window": "main"}))
            .unwrap();
    }

    #[test]
    fn emit_writes_line_when_connected() {
        use std::io::Read;
        use std::os::unix::net::UnixListener;
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let sock = tmp.path().join("orkia.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = String::new();
            s.read_to_string(&mut buf).ok();
            buf
        });
        let mut c = JournalClient::connect(&sock, "orkia.forge.x");
        assert!(c.is_connected());
        c.emit("app.window.opened", serde_json::json!({"window": "main"}))
            .unwrap();
        // Close to release the reader.
        drop(c);
        let read = handle.join().unwrap();
        assert!(read.contains("\"type\":\"lifecycle\""));
        assert!(read.contains("\"event\":\"app.window.opened\""));
        assert!(read.contains("\"session_id\":\"orkia.forge.x\""));
        assert!(read.contains("\"timestamp\":"));
        assert!(read.ends_with('\n'));
    }
}
