// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//!
//! V0 exposes five verbs:
//!  - `list` — enumerate `~/.orkia/forge/*` with status
//!  - `run` — spawn the viewer (handled by the REPL; we only produce the
//!    spawn descriptor here)
//!  - `edit` — open the app dir in `$EDITOR`
//!  - `remove` — delete the app dir with typed-name confirmation
//!  - `inspect` — print manifest, paths, SEAL count
//!
//! `run` lives in the REPL because spawning a child + registering a
//! `JobKind::ForgeApp` needs the JobController. This crate produces the

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod discover;
pub mod handlers;
pub mod parse;

pub use discover::{ForgeApp, default_app_root, discover_all, load_app};
pub use handlers::{AppRunSpec, agent, edit, inspect, list, perms, prepare_run, remove, seal};
pub use parse::{AppAction, parse};
