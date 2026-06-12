// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::enums::IssueShareAccess;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IssueShareCore {
    pub id: Uuid,
    pub issue_id: Uuid,
    pub target_workspace_id: Uuid,
    pub target_project_id: Option<Uuid>,
    pub shared_by_account_id: Uuid,
    pub access: IssueShareAccess,
    pub shared_at: DateTime<FixedOffset>,
}
