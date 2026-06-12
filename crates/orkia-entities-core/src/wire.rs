// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BootstrapMeta {
    pub sync_seq: i64,
    pub schema_version: i32,
    pub workspace_id: Uuid,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EntityLine {
    pub t: String,
    pub d: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeltaLine {
    pub seq: i64,
    pub t: String,
    pub a: String,
    pub d: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeltaEnd {
    pub count: usize,
    pub last_seq: i64,
    pub has_more: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrorLine {
    pub t: String,
    pub code: String,
    pub message: Option<String>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootstrapFailureKind {
    Network,
    Unauthenticated,
    Parse,
    SchemaVersionMismatch,
    ServerError,
}
