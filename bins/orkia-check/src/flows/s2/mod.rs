// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! S2 RFC + SEAL extended flows (F201–F205).
//!
//!   * `rfc note` does NOT exist — F202/F205 use `rfc ask` to generate
//!     `rfc.ask` lifecycle events.
//!   * SEAL v1 suffix is `.seal-completed.jsonl` / `.seal-abandoned.jsonl`.
//!   * `rfc seal <slug> --verify` prints "VALID (N events, root=…)" / "INVALID (reason)".

mod f201;
mod f202;
mod f203;
mod f204;
mod f205;

pub(crate) use f201::flow_f201;
pub(crate) use f202::flow_f202;
pub(crate) use f203::flow_f203;
pub(crate) use f204::flow_f204;
pub(crate) use f205::flow_f205;
