// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Shared viewer pieces for Orkia Forge apps.
//!
//! V0 ships three load-bearing modules:
//!
//! * [`storage`] — per-app SQLite KV at `<app-dir>/data/storage.db`.
//! * [`bridge`] — dispatcher for `BridgeMessage` from `orkia-forge-types`.
//! * [`journal`] — minimal NDJSON client that emits `app.window.{opened,closed}`
//!   to `~/.orkia/run/orkia.sock`.
//!
//! What's intentionally not here yet: the actual webview shell. Tauri 2.x
//! requires platform-specific build dependencies (webkitgtk on Linux, etc.)
//! that would impose a large cost on every workspace build. The three
//! modules above are exactly the parts that the eventual webview shell
//! needs to call into — they are stable, testable today, and the webview
//! integration is purely additive on top.

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod bridge;
pub mod config;
pub mod journal;
pub mod network;
pub mod notify;
pub mod storage;

pub use bridge::dispatch;
pub use config::ViewerConfig;
pub use journal::JournalClient;
pub use network::{FetchArgs, MAX_RESPONSE_BYTES, fetch};
pub use notify::{NotificationRateLimiter, send as send_notification};
pub use storage::Storage;
