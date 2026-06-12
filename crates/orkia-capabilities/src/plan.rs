// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Plan tier values and the canonical mapping to [`Capability`] sets.
//!
//! The mapping is intentionally a single function. New tiers add a
//! branch; new capabilities are added to the relevant branch (or
//! cascaded via the `Free → SoloPro → Team → Enterprise` chain).

use crate::{Capability, CapabilitySet};

/// Plan tiers recognised by the shell. Unknown plan strings degrade
/// to [`Plan::Free`] so a server that returns a future tier name
/// never crashes the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    Free,
    SoloPro,
    Team,
    Enterprise,
}

impl Plan {
    /// Parse the plan string stored in `TokenMetadata.plan`. Comparison
    /// is case-insensitive and tolerant of `-` / `_` / ` ` separators
    /// because the backend serialisation has shifted historically.
    pub fn parse(raw: &str) -> Self {
        let n: String = raw
            .chars()
            .filter(|c| c.is_alphanumeric())
            .collect::<String>()
            .to_lowercase();
        match n.as_str() {
            "solopro" | "pro" => Self::SoloPro,
            "team" => Self::Team,
            "enterprise" | "ent" => Self::Enterprise,
            _ => Self::Free,
        }
    }
}

///
/// | Plan          | Capabilities                                          |
/// |---------------|-------------------------------------------------------|
/// | `Free`        | (none)                                                |
/// | `SoloPro`     | CognitiveRouting, ContextCompression, CognitiveRouter, ForgeBuild |
/// | `Team`        | + TeamPipeline                                        |
/// | `Enterprise`  | + SealAuditExtended                                   |
pub fn capabilities_for_plan(plan: Plan) -> CapabilitySet {
    let mut caps: Vec<Capability> = Vec::new();
    if matches!(plan, Plan::SoloPro | Plan::Team | Plan::Enterprise) {
        caps.push(Capability::CognitiveRouting);
        caps.push(Capability::ContextCompression);
        caps.push(Capability::CognitiveRouter);
        caps.push(Capability::ForgeBuild);
    }
    if matches!(plan, Plan::Team | Plan::Enterprise) {
        caps.push(Capability::TeamPipeline);
    }
    if matches!(plan, Plan::Enterprise) {
        caps.push(Capability::SealAuditExtended);
    }
    CapabilitySet::from_capabilities(caps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_variants() {
        assert_eq!(Plan::parse("solo-pro"), Plan::SoloPro);
        assert_eq!(Plan::parse("Solo_Pro"), Plan::SoloPro);
        assert_eq!(Plan::parse("PRO"), Plan::SoloPro);
        assert_eq!(Plan::parse("team"), Plan::Team);
        assert_eq!(Plan::parse("Enterprise"), Plan::Enterprise);
        assert_eq!(Plan::parse("ENT"), Plan::Enterprise);
        assert_eq!(Plan::parse(""), Plan::Free);
        assert_eq!(Plan::parse("free"), Plan::Free);
        assert_eq!(Plan::parse("god-tier-2099"), Plan::Free);
    }
}
