// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Per-app SEAL chain for Forge V2.
//!
//! Every privileged bridge call (agent.invoke, network.fetch,
//! notification.send) appends a record to `<app-dir>/seal/events.jsonl`.
//! Records are hash-chained — each one carries the SHA-256 of the prior
//! — and signed with a per-app ECDSA P-256 key stored at
//! `<app-dir>/seal/signing.pem`. The result: tampering with any byte
//! after the fact invalidates either the chain hash or the signature.
//!
//! ## Wire shape
//!
//! ```jsonl
//! {"id":1,"ts":"2026-05-23T...","prev_hash":"sha256:000..","kind":"app.window.opened","data":{...},"hash":"sha256:abc..","sig":"hex.."}
//! ```
//!
//! The `hash` field is the SHA-256 of the *canonical* JSON of the record
//! with `hash` and `sig` removed. Canonical = serde_json default object
//! ordering with our field order; this crate writes records itself so we
//! control determinism end-to-end. (We do not need full JCS for V2
//! because the writer and verifier are the same crate.)
//!
//! ## Single-app, single-writer
//!
//! Each app has its own key + its own JSONL. The writer is
//! `Mutex`-guarded inside [`SealWriter`] to serialize concurrent appends
//! from Tauri commands that may fire in parallel from JS.

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod key;
pub mod record;
pub mod verifier;
pub mod writer;

pub use key::{SealKey, SealKeyError};
pub use record::SealRecord;
pub use verifier::{VerifyError, VerifyReport, verify_chain};
pub use writer::{SealWriter, SealWriterError};
