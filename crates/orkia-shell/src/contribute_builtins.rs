// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `$contribute` shell builtin — feedback opt-in surface.
//!
//!
//! - `$contribute`         — status (granted? counts? policy?)
//! - `$contribute on`      — multi-screen consent flow
//! - `$contribute off`     — revoke + stop future capture
//! - `$contribute history` — counts of what's queued / posted
//! - `$contribute purge`   — local truncate + server deletion request
//!
//! Everything is delegated to the kernel daemon via the existing
//! IPC. The shell side is rendering + reading user input.

use orkia_shell_types::{BlockContent, KernelContributeOutcome};

/// The exact phrase required to grant consent. Defined in-crate so
/// the shell has no dependency on the kernel-side consent module.
const CONSENT_PHRASE: &str = "I consent";

/// Multi-screen text shown by `$contribute on` before the user
/// types the consent phrase.
const CONSENT_SCREEN: &str = r#"
Cognitive feedback contribution

What gets sent:
  - The text you type before pressing Enter
  - The agent/intent the kernel chose for it
  - Your correction (if you correct it)
  - Anonymized hardware + kernel version

What does NOT get sent:
  - File contents, paths, or directory listings
  - Agent outputs, command outputs, or terminal scrollback
  - Email, names, secrets, tokens, API keys

PII scrubbing is applied locally before upload.
Data is batched every 24h, retained 365 days, never shared
or sold outside Orkia's training pipeline.

To grant consent in this shell, run:
    contribute on -- I consent

(The `--` separator + exact phrase are required.)
"#;

fn header(label: impl Into<String>) -> BlockContent {
    BlockContent::SystemInfo(format!(" {}", label.into()))
}

fn line(text: impl Into<String>) -> BlockContent {
    BlockContent::Text(text.into())
}

/// Top-level dispatch for `$contribute <subcommand>`.
pub fn dispatch(args: &[String]) -> Vec<BlockContent> {
    let sub = args.first().map(String::as_str).unwrap_or("");
    match sub {
        "" | "status" => status(),
        "on" => on(args.get(1..).unwrap_or(&[])),
        "off" => off(),
        "history" => history(),
        "purge" => purge(),
        other => vec![header(format!("✗ unknown subcommand: contribute {other}"))],
    }
}

fn status() -> Vec<BlockContent> {
    let Some(rpc) = orkia_kernel_client::discover() else {
        return vec![header(
            "contribute: kernel not running — `$login` to install",
        )];
    };
    match rpc.contribute_status() {
        Ok(s) => {
            let mut out = vec![header(format!(
                "contribute: {}",
                if s.granted { "ON" } else { "OFF" }
            ))];
            out.push(line(format!("  kernel-id:        {}", s.kernel_id)));
            out.push(line(format!("  events buffered:  {}", s.journal_count)));
            out.push(line(format!("  posted last 24h:  {}", s.posted_last_24h)));
            if s.policy_disabled {
                out.push(line("  policy:           disabled-by-policy (enterprise)"));
            }
            if let Some(secs) = s.grace_remaining_seconds {
                if secs > 0 {
                    let hours = secs / 3600;
                    out.push(line(format!(
                        "  ⚠ cognitive features expire in {hours}h — re-subscribe to continue",
                    )));
                } else {
                    out.push(line("  ⚠ cognitive features expired"));
                }
            }
            if !s.granted {
                out.push(line("  → run `contribute on` to opt in"));
            }
            out
        }
        Err(e) => vec![header(format!("✗ {e}"))],
    }
}

fn on(args: &[String]) -> Vec<BlockContent> {
    // own a multi-screen prompt, so we use the `-- <phrase>`
    // convention: `contribute on` alone surfaces the screen,
    // `contribute on -- I consent` actually grants.
    let separator = args.iter().position(|a| a == "--");
    let phrase = separator.and_then(|i| {
        let rest = args.get(i + 1..)?;
        if rest.is_empty() {
            None
        } else {
            Some(rest.join(" "))
        }
    });

    let Some(p) = phrase else {
        let mut out = vec![header("consent screen")];
        for ln in CONSENT_SCREEN.lines() {
            out.push(line(ln));
        }
        return out;
    };

    if p.trim() != CONSENT_PHRASE {
        return vec![header(
            "✗ phrase did not match — expected exactly 'I consent'",
        )];
    }

    let Some(rpc) = orkia_kernel_client::discover() else {
        return vec![header("✗ kernel not running — `$login` first")];
    };
    match rpc.contribute_set(true, Some(&p)) {
        Ok(KernelContributeOutcome::Ok) => vec![
            header("✓ feedback contribution enabled"),
            line("  first batch will ship within 24h (or immediately if events are queued)"),
            line("  run `$contribute off` any time to revoke"),
        ],
        Ok(KernelContributeOutcome::PhraseMismatch) => vec![header("✗ kernel rejected the phrase")],
        Ok(KernelContributeOutcome::DisabledByPolicy) => {
            vec![header("✗ disabled by enterprise policy")]
        }
        Ok(KernelContributeOutcome::Unsupported) => {
            vec![header("✗ kernel build does not support contribution")]
        }
        Ok(KernelContributeOutcome::Error { message }) => vec![header(format!("✗ {message}"))],
        Err(e) => vec![header(format!("✗ {e}"))],
    }
}

fn off() -> Vec<BlockContent> {
    let Some(rpc) = orkia_kernel_client::discover() else {
        return vec![header("✗ kernel not running")];
    };
    match rpc.contribute_set(false, None) {
        Ok(KernelContributeOutcome::Ok) => vec![header("✓ feedback contribution disabled")],
        Ok(other) => vec![header(format!("✗ kernel returned: {other:?}"))],
        Err(e) => vec![header(format!("✗ {e}"))],
    }
}

fn history() -> Vec<BlockContent> {
    // exposes its cursor through the IPC.
    let Some(rpc) = orkia_kernel_client::discover() else {
        return vec![header("✗ kernel not running")];
    };
    match rpc.contribute_status() {
        Ok(s) => vec![
            header("contribute history (kernel-local counters)"),
            line(format!("  events buffered now:  {}", s.journal_count)),
            line(format!("  events posted in 24h: {}", s.posted_last_24h)),
            line("  per-batch details:    ~/.orkia/kernel/journal/exporter-cursor.json"),
            line("  scrub stats file:     ~/.orkia/kernel/journal/scrub-stats.jsonl"),
        ],
        Err(e) => vec![header(format!("✗ {e}"))],
    }
}

fn purge() -> Vec<BlockContent> {
    let Some(rpc) = orkia_kernel_client::discover() else {
        return vec![header("✗ kernel not running")];
    };
    match rpc.contribute_purge() {
        Ok(KernelContributeOutcome::Ok) => vec![
            header("✓ local feedback journal cleared"),
            line("  server-side deletion requested (completes within 30 days)"),
        ],
        Ok(other) => vec![header(format!("✗ kernel returned: {other:?}"))],
        Err(e) => vec![header(format!("✗ {e}"))],
    }
}
