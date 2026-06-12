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
pub struct RfcVersionCore {
    pub id: Uuid,
    pub rfc_id: Uuid,
    pub version_number: i32,
    pub content: serde_json::Value,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<FixedOffset>,
    pub structured_metadata: Option<serde_json::Value>,
    pub structured_extraction_status: String,
    pub structured_extraction_error: Option<String>,
    pub structured_extracted_at: Option<DateTime<FixedOffset>>,
}
