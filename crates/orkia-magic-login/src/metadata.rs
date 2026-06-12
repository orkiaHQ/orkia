// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Persisted session metadata. The schema mirrors the proprietary
//! distribution's `TokenMetadata` field-for-field so a keychain entry written
//! by either binary deserializes in the other.

use chrono::{DateTime, Utc};
use orkia_auth::SessionInfo;
use serde::{Deserialize, Serialize};

/// Profile data stored alongside the bearer token in the keychain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub account_id: String,
    pub username: String,
    pub email: String,
    pub plan: String,
    pub issued_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub workspace_id: Option<String>,
}

/// Project stored metadata into the neutral [`SessionInfo`] the shell
/// renders. Empty `account_id` becomes `None` so reasoning scoping stays
/// fail-closed (matches the env-provider contract).
pub(crate) fn to_session_info(meta: &SessionMetadata) -> SessionInfo {
    SessionInfo {
        display_name: meta.username.clone(),
        email: meta.email.clone(),
        plan: meta.plan.clone(),
        issued_at: meta.issued_at,
        expires_at: meta.expires_at,
        account_id: (!meta.account_id.is_empty()).then(|| meta.account_id.clone()),
        workspace_id: meta.workspace_id.clone(),
    }
}
