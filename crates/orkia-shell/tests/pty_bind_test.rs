// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! End-to-end smoke for the orkia/brush PTY bridge.
//!
//! Opens a raw PTY pair via `orkia_pty::open_pair`, hands the slave to brush
//! via `engine::pty::bind_pty_to_shell`, runs a command, and reads the bytes
//! back from the master side — proving children inherit the PTY slave fd.

use std::io::Read;
use std::time::Duration;

use orkia_shell::ShellEngine;
use orkia_shell::engine::pty::bind_pty_to_shell;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn brush_output_reaches_master_via_pty() {
    let pty = orkia_pty::open_pair(80, 24).expect("open_pair");
    let orkia_pty::AdoptedPty {
        mut reader, slave, ..
    } = pty;

    let mut engine = ShellEngine::new().await.expect("engine");
    bind_pty_to_shell(engine.shell_mut(), slave).expect("bind");

    // Drain in a background thread so the PTY write side doesn't fill.
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            if tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });

    let r = engine
        .execute("printf 'orkia-pty-marker\\n'")
        .await
        .expect("execute");
    assert_eq!(r.exit_code, 0);

    // Collect output for up to 1s.
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    let mut buf = Vec::<u8>::new();
    while std::time::Instant::now() < deadline {
        if let Ok(b) = rx.recv_timeout(Duration::from_millis(100)) {
            buf.extend_from_slice(&b);
            if buf
                .windows(b"orkia-pty-marker".len())
                .any(|w| w == b"orkia-pty-marker")
            {
                break;
            }
        }
    }

    let s = String::from_utf8_lossy(&buf);
    assert!(
        s.contains("orkia-pty-marker"),
        "expected marker in PTY output, got {s:?}"
    );
}
