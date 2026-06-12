// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use orkia_shell_types::BlockContent;

/// Execute a system `kill` with the given signal and target. Returns the
/// blocks to display.
pub fn system_kill(target: &str, signal: &str) -> Vec<BlockContent> {
    let flag = format!("-{signal}");
    match std::process::Command::new("kill")
        .args([&flag, target])
        .output()
    {
        Ok(out) if out.status.success() => {
            vec![BlockContent::SystemInfo(format!(
                "kill -{signal} {target}: sent"
            ))]
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            vec![BlockContent::Error(format!(
                "kill -{signal} {target}: {}",
                stderr.trim()
            ))]
        }
        Err(e) => vec![BlockContent::Error(format!("kill: failed to invoke: {e}"))],
    }
}
