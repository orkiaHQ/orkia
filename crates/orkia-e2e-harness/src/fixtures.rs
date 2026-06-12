// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Mirror of the fixture constants seeded by `orkia-server seed-test`.
//!
//! Duplicated rather than imported because the harness lives in the
//! `orkia/` workspace and the seed lives in the proprietary workspace. If you change
//! one side, change the other. The compose `seed` service is the only
//! producer of these rows.

/// Deterministic test account.
pub const TEST_ACCOUNT_ID: &str = "00000000-0000-0000-0000-0000000000a1";
/// Deterministic test workspace; scope for `reset_e2e_test_state()`.
pub const TEST_WORKSPACE_ID: &str = "00000000-0000-0000-0000-0000000000a2";
/// Deterministic test team (preserved by reset).
pub const TEST_TEAM_ID: &str = "00000000-0000-0000-0000-0000000000a3";
/// Deterministic default project (preserved by reset).
pub const TEST_PROJECT_ID: &str = "00000000-0000-0000-0000-0000000000a4";

pub const TEST_WORKSPACE_NAME: &str = "e2e-test";
