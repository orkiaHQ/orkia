// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::enums::InviteStatus;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceInviteCore {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub invited_by_account_id: Uuid,
    pub email: String,
    pub role: String,
    pub nonce: String,
    pub status: InviteStatus,
    pub expires_at: DateTime<FixedOffset>,
    pub accepted_at: Option<DateTime<FixedOffset>>,
    pub created_at: DateTime<FixedOffset>,
    pub updated_at: DateTime<FixedOffset>,
}
