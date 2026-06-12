// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use std::path::PathBuf;

use crate::scope::Scope;

// ─── Public types ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct Workspace {
    pub projects: Vec<Project>,
    pub root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Project {
    pub name: String,
    pub description: Option<String>,
    pub assigned_agents: Vec<String>,
    pub rfcs: Vec<RfcSummary>,
    pub issues: Vec<IssueSummary>,
    pub path: PathBuf,
    /// Project-level visibility scope override loaded from `project.toml`.
    /// `None` means the project inherits the workspace's `default_scope`
    /// (which itself defaults to [`Scope::Private`]). PR1b ships the
    /// field for round-trip support; no reader consults it yet.
    pub scope: Option<Scope>,
}

#[derive(Debug, Clone)]
pub struct RfcSummary {
    pub slug: String,
    pub title: String,
    pub status: String,
    pub assigned: Vec<String>,
    pub path: PathBuf,
    /// RFC-level scope override read from the frontmatter. `None` means
    /// inherit from the project. Same PR1b foundation-only contract as
    /// [`Project::scope`].
    pub scope: Option<Scope>,
}

#[derive(Debug, Clone)]
pub struct IssueSummary {
    pub number: u32,
    pub slug: String,
    pub title: String,
    pub status: String,
    pub priority: String,
    pub assigned: Option<String>,
    pub path: PathBuf,
    /// Issue-level scope override. Same PR1b foundation-only contract.
    pub scope: Option<Scope>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct RfcFrontmatter {
    pub title: Option<String>,
    pub status: Option<String>,
    #[serde(default)]
    pub assigned: Option<Vec<String>>,
    pub created_at: Option<String>,
    /// Visibility scope declared in the RFC's TOML frontmatter. Round-trips
    /// alongside the canonical mirror in `orkia_rfc_core::RfcFrontmatter`;
    /// keep them in sync (R2). PR1b parses; no behavior depends on it yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<Scope>,
}
