// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! The SEAL streaming source feeding `orkia-stream`:
//!
//! * [`seal::SealSource`] — tails `.jsonl` chain files under `~/.orkia/`.

pub mod seal;
