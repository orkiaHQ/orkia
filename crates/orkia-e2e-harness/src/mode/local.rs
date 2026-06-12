// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Local boot path.
//!
//! task in the harness for <3s boot). That would require linking
//! orkia-server as a library across the public ↔ proprietary workspace
//! boundary — out of scope for V1.
//!
//! compose code path. The user is expected to bring the stack up
//! out-of-band (`docker-compose -f docker-compose.test.yml up -d`)
//! and `Mode::Local` reuses the same attach logic as `Mode::Compose`.
//! The only behavioral difference between the modes is the [`Mode`]
//! tag carried on the session, which `orkia-check`'s JSON report
//! surfaces verbatim.

use crate::session::OrkiaSession;

pub async fn start_local() -> crate::Result<OrkiaSession> {
    // Same boot, same defaults. V2 should replace this with a true
    // in-process backend once orkia-server is library-shaped.
    let s = crate::mode::compose::start_compose().await?;
    Ok(OrkiaSession::override_mode(s, crate::Mode::Local))
}
