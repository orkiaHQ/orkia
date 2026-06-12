// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::enums::RfcMessageAuthorType;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RfcMessageMentionCore {
    pub id: Uuid,
    pub message_id: Uuid,
    pub mention_type: RfcMessageAuthorType,
    pub account_id: Option<Uuid>,
    pub agent_id: Option<Uuid>,
    pub resolved: bool,
    pub created_at: DateTime<FixedOffset>,
}
