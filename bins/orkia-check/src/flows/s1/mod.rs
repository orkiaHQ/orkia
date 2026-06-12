// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! S1 job-control flows (F101–F106): multi-agent ps, fg/bg, wait/disown,
//! natural completion, crash recovery, daemon cockpit contract.

mod f101;
mod f102;
mod f103;
mod f104;
mod f105;
mod f106;

pub(crate) use f101::flow_f101;
pub(crate) use f102::flow_f102;
pub(crate) use f103::flow_f103;
pub(crate) use f104::flow_f104;
pub(crate) use f105::flow_f105;
pub(crate) use f106::flow_f106;

use orkia_e2e_harness::OrkiaSession;

pub(super) const F101_RELATED: &[&str] = &["shell"];

pub(crate) async fn count_lifecycle_events(session: &OrkiaSession) -> usize {
    let Some(shell) = session.shell() else {
        return 0;
    };
    shell
        .journal
        .all()
        .await
        .iter()
        .skip(session.journal_cursor())
        .filter(|e| e.event_type() == Some("lifecycle"))
        .count()
}

pub(crate) async fn recent_lifecycle_events_after(session: &OrkiaSession, n: usize) -> Vec<String> {
    let Some(shell) = session.shell() else {
        return Vec::new();
    };
    shell
        .journal
        .all()
        .await
        .iter()
        .skip(session.journal_cursor())
        .filter(|e| e.event_type() == Some("lifecycle"))
        .skip(n)
        .map(|e| serde_json::to_string(&e.raw).unwrap_or_default())
        .collect()
}
