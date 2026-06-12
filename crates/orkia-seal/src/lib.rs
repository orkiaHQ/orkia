// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! # orkia-seal
//!
//! This crate is a **pointer**, not an implementation.
//!
//! "SEAL" in Orkia refers to a family of audit primitives. The canonical
//! SEAL v1 assembler is implemented by the proprietary distribution.
//!
//! For an overview of the four audit ledgers used internally, see
//! [`SEAL-FAMILY.md`].
//!
//! ## Where the implementations live
//!
//! - **SEAL v1 (the standard)** — provided by the proprietary distribution;
//!   see the public specification for the wire format
//! - **Shell Audit Log** — `orkia/crates/orkia-shell/src/seal`
//! - **Forge App Provenance** — `orkia/crates/orkia-forge-seal`
//! - **Workspace Audit Ledger** — provided by the private workspace backend
//!
//! ## Why a pointer crate?
//!
//! See `SEAL-ADR.md` for the rationale. TL;DR: the four ledgers have
//! intentionally different threat models and crypto guarantees. SEAL v1
//! is the format we publish as a compliance standard.
//!
//! [`SEAL-FAMILY.md`]: https://github.com/orkiaHQ/orkia/blob/main/SEAL-FAMILY.md

// Intentionally empty.
