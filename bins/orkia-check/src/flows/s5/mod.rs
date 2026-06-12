// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! S5 scheduling + apps flows (F501–F507).
//!
//! Audit findings baked in:
//!   * `every` persists to the SYSTEM crontab (`Command::new("crontab")`,
//!     crontab.rs:56,87). PART 0 puts a `crontab` shim on the sandbox PATH
//!     redirecting to an isolated spool — only storage is redirected, the
//!     cron-line generation / tags / pause toggle run for real.
//!   * `every` strings: create "✓ Scheduled:", list shows the AGENT column
//!     (+ "no scheduled jobs" when empty), "✓ Paused:" / "✓ Resumed:" /
//!     "✓ Removed:". Spool tag line is "# orkia:<agent>:<slug>".
//!   * Forge manifest is a `[forge]` table (orkia-forge-types/manifest.rs);
//!     apps live at `$HOME/.orkia/forge/<name>/` (default_app_root).
//!   * `app inspect` shows version/permissions (NOT description). `app perms`
//!     prints "N domain(s) allowed" + "• <host>". `app seal --verify` →
//!     "N events verified" / "verify failed". `app usage` (free plan) →
//!     "requires an Orkia premium plan". `app run` w/o viewer →
//!     "orkia-forge-viewer binary not found".
//!   * `orkia_forge_seal::SealWriter` is standalone (open+append, auto-keys)
//!     → F504 does the full seed→verify→tamper→verify round-trip.
//!   * `contribute` delegates to the kernel daemon (absent in compose) →
//!     "kernel not running". The real egress gate is proprietary (deferred).

mod f501;
mod f502;
mod f503;
mod f504;
mod f505;
mod f506;
mod f507;

pub(crate) use f501::flow_f501;
pub(crate) use f502::flow_f502;
pub(crate) use f503::flow_f503;
pub(crate) use f504::flow_f504;
pub(crate) use f505::flow_f505;
pub(crate) use f506::flow_f506;
pub(crate) use f507::flow_f507;

/// Minimal valid Forge manifest (`[forge]` table). `network` is set so
/// `app perms` has a domain to render.
pub(super) fn test_manifest() -> &'static str {
    r#"[forge]
name = "test-app"
description = "E2E fixture"
version = "0.1.0"
api_version = 1
rfc_id = "test-rfc"
rfc_hash = "sha256:0"
created_at = "2026-01-01T00:00:00Z"
icon = "default"

[forge.window]
title = "Test App"
width = 480
height = 320
resizable = true

[forge.permissions]
storage = true
agent = false
network = ["https://api.example.com"]
notification = false
"#
}

pub(super) const S5_SCHED_RELATED: &[&str] = &["every"];
pub(super) const S5_APP_RELATED: &[&str] = &["forge-apps", "forge-seal"];
