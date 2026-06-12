// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::enums::ProjectCloneAccess;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProjectCloneCore {
    pub id: Uuid,
    pub project_id: Uuid,
    pub workspace_id: Uuid,
    pub access: ProjectCloneAccess,
    pub cloned_at: DateTime<FixedOffset>,
}
