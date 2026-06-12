// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]

pub mod enums;
pub mod wire;

mod account;
mod agent;
mod approval;
mod archetype;
mod cli_raw_event;
mod issue;
mod issue_branch;
mod issue_comment;
mod issue_event;
mod issue_share;
mod project;
mod project_clone;
mod rejection;
mod rfc;
mod rfc_message;
mod rfc_message_mention;
mod rfc_version;
mod seal_record;
mod shared_session_excerpt;
mod workspace;
mod workspace_invite;

pub use account::AccountCore;
pub use agent::AgentCore;
pub use approval::ApprovalCore;
pub use archetype::ArchetypeCore;
pub use cli_raw_event::CliRawEventCore;
pub use issue::IssueCore;
pub use issue_branch::IssueBranchCore;
pub use issue_comment::IssueCommentCore;
pub use issue_event::IssueEventCore;
pub use issue_share::IssueShareCore;
pub use project::ProjectCore;
pub use project_clone::ProjectCloneCore;
pub use rejection::MutationRejectionCode;
pub use rfc::RfcCore;
pub use rfc_message::RfcMessageCore;
pub use rfc_message_mention::RfcMessageMentionCore;
pub use rfc_version::RfcVersionCore;
pub use seal_record::SealRecordCore;
pub use shared_session_excerpt::SharedSessionExcerptCore;
pub use wire::BootstrapFailureKind;
pub use workspace::WorkspaceCore;
pub use workspace_invite::WorkspaceInviteCore;

// defines its own `RfcState` enum with six kebab-case states. It is the
// authoritative state for shell-side RFC orchestration.
//
// `enums::RfcStatus` remains the *wire/DB contract* used when syncing
// to an external backend (its snake_case variants are the on-wire
// names). The two types are intentionally separate: the wire contract
// evolves independently of the fs state machine.
pub use orkia_rfc_core::RfcState;
