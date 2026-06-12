// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use serde::{Deserialize, Serialize};

// --- Enums stored as String in the backend entity (snake_case DB values) ---

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IssueStatus {
    #[serde(rename = "backlog")]
    Backlog,
    #[serde(rename = "todo")]
    Todo,
    #[serde(rename = "in_progress")]
    InProgress,
    #[serde(rename = "review")]
    Review,
    #[serde(rename = "done")]
    Done,
    #[serde(rename = "blocked")]
    Blocked,
    #[serde(rename = "queued")]
    Queued,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "canceled")]
    Canceled,
    #[serde(rename = "rejected")]
    Rejected,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IssuePriority {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "low")]
    Low,
    #[serde(rename = "medium")]
    Medium,
    #[serde(rename = "high")]
    High,
    #[serde(rename = "critical")]
    Critical,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventType {
    #[serde(rename = "status_change")]
    StatusChange,
    #[serde(rename = "priority_change")]
    PriorityChange,
    #[serde(rename = "assignment")]
    Assignment,
    #[serde(rename = "routing")]
    Routing,
    #[serde(rename = "approval_requested")]
    ApprovalRequested,
    #[serde(rename = "approval_resolved")]
    ApprovalResolved,
    #[serde(rename = "label_change")]
    LabelChange,
    #[serde(rename = "description_change")]
    DescriptionChange,
    #[serde(rename = "branch_created")]
    BranchCreated,
    #[serde(rename = "branch_merged")]
    BranchMerged,
    #[serde(rename = "branch_abandoned")]
    BranchAbandoned,
    #[serde(rename = "comment_added")]
    CommentAdded,
    #[serde(rename = "seal_created")]
    SealCreated,
    #[serde(rename = "share_created")]
    ShareCreated,
}

// --- Enums used as typed fields in backend entities (PascalCase via serde) ---

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BranchStatus {
    Draft,
    InProgress,
    Merged,
    Abandoned,
}

// Sync wire contract: the backend's storage `ApprovalStatus` stores
// snake_case (enum values like "pending") and the local
// `approval.status` column + `list_approvals_pending` filter expect
// snake_case too. Without this the mirror stores "Pending" and the
// pending query never matches (caught by the e2e approvals round-trip).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Declined,
    Expired,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentStatus {
    Idle,
    Working,
    Attention,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentRuntimeMode {
    ClaudeCode,
    LlmModel,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutonomyLevel {
    Full,
    Supervised,
    Restricted,
    Locked,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectCloneAccess {
    Read,
    Write,
    Admin,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectMemberRole {
    Admin,
    Member,
    Viewer,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IssueShareAccess {
    Read,
    Write,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InviteStatus {
    Pending,
    Accepted,
    Rejected,
    Revoked,
    Expired,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArchetypeOrigin {
    Orkia,
    User,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArchetypeVisibility {
    Workspace,
    Org,
    Public,
}

// Sync wire contract: the backend serialises these snake_case (its
// storage enums use `#[serde(rename_all = "snake_case")]`). These
// mirrors must match or sync
// deserialization fails (caught by the e2e rfc round-trip).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RfcStatus {
    Draft,
    InReview,
    Approved,
    Rejected,
    Delivered,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RfcMessageAuthorType {
    Human,
    Orkia,
    Agent,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RfcMessageType {
    Message,
    Classification,
    Decomposition,
    Routing,
    Launch,
    Version,
    Relaunch,
    RefineRequest,
    RefineResponse,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RfcMessageValidatorStatus {
    Pending,
    Running,
    Validated,
    Revised,
    MaxRetriesReached,
    Skipped,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BillingPlan {
    Free,
    Starter,
    Team,
    Org,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrgRole {
    Owner,
    Admin,
    Member,
}
