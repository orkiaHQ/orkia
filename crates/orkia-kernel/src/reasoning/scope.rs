// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! Per-job project/RFC attribution. The REPL owns the authoritative job →
//! project/RFC mapping (it resolves them at spawn); the off-loop consumer
//! reads it by `job_id` to stamp each turn. This mirrors the existing
//! `seal::JobProjects` shared map — a small, lock-protected lookup, not a
//! shared mutable data structure. The consumer never writes it.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use uuid::Uuid;

use orkia_reasoning_core::dto::RfcRef;

/// The optional project + RFC a job is running under. Both absent is normal
/// (ad-hoc shell agents have neither).
#[derive(Debug, Clone, Default)]
pub struct JobScope {
    pub project_id: Option<Uuid>,
    pub rfc_ref: Option<RfcRef>,
}

/// Shared `job_id → JobScope` map. The REPL inserts at spawn and removes at
/// reap; the consumer only reads.
pub type JobScopes = Arc<RwLock<HashMap<u32, JobScope>>>;

/// Construct an empty shared map.
pub fn new_job_scopes() -> JobScopes {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Read the scope for `job_id`. A poisoned lock fails closed to an empty scope
/// (never panics).
pub fn scope_for(scopes: &JobScopes, job_id: u32) -> JobScope {
    match scopes.read() {
        Ok(map) => map.get(&job_id).cloned().unwrap_or_default(),
        Err(_) => JobScope::default(),
    }
}
