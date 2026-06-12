// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use orkia_shell_types::Scope;
use std::path::PathBuf;

pub enum RfcAction {
    List {
        project: Option<String>,
        status: Option<String>,
    },
    Show {
        slug: String,
        project: Option<String>,
    },
    Create {
        title: String,
        project: Option<String>,
        assigned: Vec<String>,
        scope: Option<Scope>,
    },
    Edit {
        slug: String,
        project: Option<String>,
    },
    Update {
        slug: String,
        project: Option<String>,
        field: String,
        value: String,
    },
    Delegate {
        slug: String,
        project: Option<String>,
        agent: String,
    },
    Remove {
        slug: String,
        project: Option<String>,
        force: bool,
    },
    ConstraintsPropose {
        slug: String,
        project: Option<String>,
    },
    ConstraintsAccept {
        slug: String,
        project: Option<String>,
        allowed_paths: Vec<String>,
        forbidden_paths: Vec<String>,
        forbidden_commands: Vec<String>,
        risk_ceiling: Option<String>,
        watch_paths: Vec<String>,
    },

    /// Show state, version, locks, pending counts.
    State {
        slug: Option<String>,
        project: Option<String>,
    },
    /// Enter the RFC scope (prompt context switches).
    Cd {
        slug: String,
        project: Option<String>,
    },
    /// Leave the RFC scope.
    ExitScope,
    /// Promote DraftActive → Active. Requires explicit `--yes` confirmation
    /// preview of the transition and an `Approval` block.
    Promote {
        slug: Option<String>,
        project: Option<String>,
        confirm: bool,
    },
    /// Mark Active → Completed. Requires explicit `--yes` confirmation.
    Complete {
        slug: Option<String>,
        project: Option<String>,
        confirm: bool,
    },
    /// Mark Active/DraftActive → Abandoned. Requires explicit `--yes`.
    Abandon {
        slug: Option<String>,
        project: Option<String>,
        reason: String,
        confirm: bool,
    },
    /// Reopen. Requires explicit `--yes`. Archives v_n, creates v_n+1.
    Reopen {
        slug: Option<String>,
        project: Option<String>,
        confirm: bool,
    },
    /// Show who holds the write lock.
    LockStatus {
        slug: Option<String>,
        project: Option<String>,
    },
    /// Force-release the write lock (human override).
    ReleaseLock {
        slug: Option<String>,
        project: Option<String>,
    },
    /// Agent-or-human clarification ask. Records the decision and, when
    /// invoked by an agent over MCP, captures the asking PTY's job id so
    /// the resolution can be injected back into the agent's stdin.
    Ask {
        slug: Option<String>,
        project: Option<String>,
        question: String,
        rationale: String,
    },
    /// Resolve a clarification with an answer. If the original asker was an
    /// agent (PTY-bound), the answer is also injected into its stdin.
    Resolve {
        slug: Option<String>,
        project: Option<String>,
        decision_id: String,
        answer: String,
    },
    /// Assemble (or re-assemble + display) the SEAL v1 document for the
    Seal {
        slug: String,
        project: Option<String>,
        verify: bool,
        rebuild: bool,
        output: Option<PathBuf>,
    },
    /// Export the workspace SEAL v1 signing key to a file. Companion of
    SealExportKey { path: PathBuf },
    /// Import a previously exported SEAL v1 signing key.
    SealImportKey { path: PathBuf },
    /// Generate a Forge app scaffold from an RFC with `kind = "forge-app"`.
    /// V0 always runs the local `ScaffoldBuilder`; `--offline` is accepted
    /// but ignored, `--rerun` is accepted but returns "not yet" until V1.
    Forge {
        rfc_id: String,
        project: Option<String>,
        force: bool,
        offline: bool,
        rerun: bool,
        /// `--yes` — pre-confirmation for `--rerun` against an unchanged RFC.
        /// for guaranteed-identical output, so we require explicit
        /// confirmation in that case.
        confirmed: bool,
    },
}

impl std::fmt::Debug for RfcAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::List { .. } => write!(f, "List"),
            Self::Show { .. } => write!(f, "Show"),
            Self::Create { .. } => write!(f, "Create"),
            Self::Edit { .. } => write!(f, "Edit"),
            Self::Update { .. } => write!(f, "Update"),
            Self::Delegate { .. } => write!(f, "Delegate"),
            Self::Remove { .. } => write!(f, "Remove"),
            Self::ConstraintsPropose { .. } => write!(f, "ConstraintsPropose"),
            Self::ConstraintsAccept { .. } => write!(f, "ConstraintsAccept"),
            Self::State { .. } => write!(f, "State"),
            Self::Cd { .. } => write!(f, "Cd"),
            Self::ExitScope => write!(f, "ExitScope"),
            Self::Promote { .. } => write!(f, "Promote"),
            Self::Complete { .. } => write!(f, "Complete"),
            Self::Abandon { .. } => write!(f, "Abandon"),
            Self::Reopen { .. } => write!(f, "Reopen"),
            Self::LockStatus { .. } => write!(f, "LockStatus"),
            Self::ReleaseLock { .. } => write!(f, "ReleaseLock"),
            Self::Ask { .. } => write!(f, "Ask"),
            Self::Resolve { .. } => write!(f, "Resolve"),
            Self::Forge { .. } => write!(f, "Forge"),
            Self::Seal { .. } => write!(f, "Seal"),
            Self::SealExportKey { .. } => write!(f, "SealExportKey"),
            Self::SealImportKey { .. } => write!(f, "SealImportKey"),
        }
    }
}
