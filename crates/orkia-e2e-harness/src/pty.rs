// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! PTY wrapper.
//!
//! `OrkiaSession` drives the shell through `orkia_test_harness::PtyDriver`
//! directly. This module is reserved for harness-specific PTY helpers
//! that don't belong in the W1 harness. Empty in S0.
