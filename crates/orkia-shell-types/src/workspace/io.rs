// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use std::path::{Path, PathBuf};

use crate::scope::Scope;

use super::frontmatter::parse_rfc_frontmatter;
use super::types::{IssueSummary, Project, RfcSummary};

// ─── Project scanning ───────────────────────────────────────────────────────

pub(super) fn scan_projects(root: &Path) -> Vec<Project> {
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut projects: Vec<Project> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| load_project(&e.path()))
        .collect();
    projects.sort_by(|a, b| a.name.cmp(&b.name));
    projects
}

pub(super) fn load_project(dir: &Path) -> Option<Project> {
    let toml_path = dir.join("project.toml");
    let fallback_name = || {
        dir.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string()
    };
    let parsed = if toml_path.exists() {
        parse_project_toml(&toml_path).unwrap_or_else(|| ParsedProject {
            name: fallback_name(),
            description: None,
            assigned: Vec::new(),
            scope: None,
        })
    } else {
        ParsedProject {
            name: fallback_name(),
            description: None,
            assigned: Vec::new(),
            scope: None,
        }
    };
    if parsed.name.is_empty() {
        return None;
    }
    Some(Project {
        name: parsed.name,
        description: parsed.description,
        assigned_agents: parsed.assigned,
        rfcs: scan_rfcs(&dir.join("rfcs")),
        issues: scan_issues(&dir.join("issues")),
        path: dir.to_path_buf(),
        scope: parsed.scope,
    })
}

#[derive(Debug, serde::Deserialize)]
struct ProjectTomlFile {
    project: Option<ProjectSection>,
    agents: Option<AgentsSection>,
}

#[derive(Debug, serde::Deserialize)]
struct ProjectSection {
    name: Option<String>,
    description: Option<String>,
    /// Project-level scope override. Inherits from the workspace default
    /// when absent. PR1b: round-trip only.
    #[serde(default)]
    scope: Option<Scope>,
}

#[derive(Debug, serde::Deserialize)]
struct AgentsSection {
    #[serde(default)]
    assigned: Vec<String>,
}

pub(super) struct ParsedProject {
    pub name: String,
    pub description: Option<String>,
    pub assigned: Vec<String>,
    pub scope: Option<Scope>,
}

pub(super) fn parse_project_toml(path: &Path) -> Option<ParsedProject> {
    let content = std::fs::read_to_string(path).ok()?;
    let parsed: ProjectTomlFile = toml::from_str(&content).ok()?;
    let project = parsed.project?;
    let name = project.name?;
    let assigned = parsed.agents.map(|a| a.assigned).unwrap_or_default();
    Some(ParsedProject {
        name,
        description: project.description,
        assigned,
        scope: project.scope,
    })
}

// ─── RFC scanning ───────────────────────────────────────────────────────────

pub(super) fn scan_rfcs(dir: &Path) -> Vec<RfcSummary> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut rfcs: Vec<RfcSummary> = entries
        .flatten()
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("md"))
        .filter_map(|e| load_rfc(&e.path()))
        .collect();
    rfcs.sort_by(|a, b| a.slug.cmp(&b.slug));
    rfcs
}

pub(super) fn load_rfc(path: &Path) -> Option<RfcSummary> {
    let stem = path.file_stem().and_then(|s| s.to_str())?.to_string();
    let content = std::fs::read_to_string(path).ok()?;
    let (fm, _body) = parse_rfc_frontmatter(&content);
    let (title, status, assigned, scope) = match fm {
        Some(f) => (
            f.title.unwrap_or_else(|| stem.clone()),
            f.status.unwrap_or_else(|| "draft".into()),
            f.assigned.unwrap_or_default(),
            f.scope,
        ),
        None => (stem.clone(), "draft".into(), Vec::new(), None),
    };
    Some(RfcSummary {
        slug: stem,
        title,
        status,
        assigned,
        path: path.to_path_buf(),
        scope,
    })
}

// ─── Issue scanning ─────────────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize)]
pub(super) struct IssueTomlFile {
    pub issue: IssueSection,
}

#[derive(Debug, serde::Deserialize)]
pub(super) struct IssueSection {
    pub title: Option<String>,
    pub status: Option<String>,
    pub priority: Option<String>,
    pub assigned: Option<String>,
    /// Issue-level scope override. Same PR1b foundation-only contract.
    #[serde(default)]
    pub scope: Option<Scope>,
}

pub(super) fn scan_issues(dir: &Path) -> Vec<IssueSummary> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut issues: Vec<IssueSummary> = entries
        .flatten()
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("toml"))
        .filter_map(|e| load_issue(&e.path()))
        .collect();
    issues.sort_by_key(|i| i.number);
    issues
}

pub(super) fn load_issue(path: &Path) -> Option<IssueSummary> {
    let stem = path.file_stem().and_then(|s| s.to_str())?;
    let (number, slug) = split_issue_filename(stem)?;
    let content = std::fs::read_to_string(path).ok()?;
    let parsed: IssueTomlFile = toml::from_str(&content).ok()?;
    let assigned = parsed.issue.assigned.filter(|s| !s.is_empty());
    let scope = parsed.issue.scope;
    Some(IssueSummary {
        number,
        slug,
        title: parsed.issue.title.unwrap_or_default(),
        status: parsed.issue.status.unwrap_or_else(|| "todo".into()),
        priority: parsed.issue.priority.unwrap_or_else(|| "medium".into()),
        assigned,
        path: path.to_path_buf(),
        scope,
    })
}

pub(super) fn split_issue_filename(stem: &str) -> Option<(u32, String)> {
    let (num_part, rest) = stem.split_once('-')?;
    let n: u32 = num_part.parse().ok()?;
    Some((n, rest.to_string()))
}

pub(super) fn next_issue_number(project_path: &Path) -> u32 {
    let dir = project_path.join("issues");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return 1,
    };
    let max = entries
        .flatten()
        .filter_map(|e| {
            e.path()
                .file_stem()
                .and_then(|s| s.to_str().map(String::from))
        })
        .filter_map(|stem| split_issue_filename(&stem).map(|(n, _)| n))
        .max()
        .unwrap_or(0);
    max + 1
}

pub(super) fn find_issue_path(project_path: &Path, number: u32) -> Option<PathBuf> {
    let dir = project_path.join("issues");
    let prefix = format!("{number:03}-");
    std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .find(|e| {
            e.path()
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.starts_with(&prefix))
                .unwrap_or(false)
        })
        .map(|e| e.path())
}
