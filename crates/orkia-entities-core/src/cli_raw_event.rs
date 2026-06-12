// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CliRawEventCore {
    pub id: Uuid,
    pub session_id: String,
    pub workspace_id: Uuid,
    pub account_id: Option<Uuid>,
    pub event_name: String,
    pub raw_json: String,
    pub received_at: DateTime<FixedOffset>,
}
