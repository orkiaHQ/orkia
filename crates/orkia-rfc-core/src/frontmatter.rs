// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};

use crate::error::RfcError;
use crate::hash::ContentHash;
use crate::id::{AgentId, RfcId};
use crate::scope::Scope;
use crate::state::RfcState;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RfcFrontmatter {
    pub id: RfcId,
    pub state: RfcState,
    pub version: u32,
    pub created_at: DateTime<FixedOffset>,
    pub updated_at: DateTime<FixedOffset>,
    pub content_hash: ContentHash,
    #[serde(default)]
    pub agents: Vec<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked_by: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked_at: Option<DateTime<FixedOffset>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Legacy mirror of `state` as a free-form string ("draft", "active", …).
    /// Written so the workspace's legacy frontmatter loader
    /// (`orkia-shell-types::parse_rfc_frontmatter`) continues to populate
    /// `RfcSummary.status` without needing to parse `RfcState`. The state
    /// machine itself ignores this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Legacy mirror of agent assignments. Separate from `agents` (which
    /// tracks who has participated). Consumed only by the workspace UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned: Option<Vec<String>>,
    /// Discriminator added in V0 of Forge. `Some("forge-app")` opts the RFC
    /// into the Forge pipeline; `None` keeps legacy RFC behavior. Future
    /// kinds will land here too (e.g. `"task"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// `[forge]` block. Only meaningful when `kind == Some("forge-app")`,
    /// but kept Option so RFCs may declare kind first and fill the block
    /// during drafting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forge: Option<ForgeFrontmatterBlock>,
    /// RFC-level visibility scope override. `None` means inherit from
    /// the project (which itself may inherit from the workspace default).
    /// Mirrored into the legacy `orkia_shell_types::workspace::RfcFrontmatter`
    /// so the workspace UI sees the same value. Keep in sync (R2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<Scope>,
    /// Optional operator policy for action-grounded drift detection. These
    /// constraints are accepted RFC state; proposal-only drafts must not be
    /// enforced until written here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator: Option<OperatorFrontmatterBlock>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct OperatorFrontmatterBlock {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraints: Option<OperatorConstraints>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct OperatorConstraints {
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    #[serde(default)]
    pub forbidden_paths: Vec<String>,
    #[serde(default)]
    pub forbidden_commands: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk_ceiling: Option<String>,
    #[serde(default)]
    pub watch_paths: Vec<String>,
}

/// Mirror of the `[forge]` table inside an RFC's TOML frontmatter. This
/// crate intentionally does not depend on `orkia-forge-types`; instead the
/// downstream `ScaffoldBuilder` translates this into a `ForgeManifest`
/// once it stamps `rfc_hash`, `created_at`, etc.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForgeFrontmatterBlock {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub icon: Option<String>,
    pub window: ForgeWindow,
    #[serde(default)]
    pub permissions: ForgePermissions,
    /// V2: optional embedded agent. When present, the scaffolder writes
    /// `<app-dir>/agent/archetype.toml` + `system-prompt.md` and the
    /// app can call `window.orkia.v1.agent.invoke()` at runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<ForgeAgentBlock>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForgeAgentBlock {
    pub archetype: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub max_invocations_per_hour: Option<u32>,
    #[serde(default)]
    pub max_cost_cents_per_invocation: Option<u32>,
    pub system_prompt: String,
    #[serde(default)]
    pub tools: ForgeAgentTools,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ForgeAgentTools {
    #[serde(default)]
    pub fetch: bool,
    #[serde(default)]
    pub shell: bool,
    #[serde(default)]
    pub filesystem: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForgeWindow {
    pub title: String,
    pub width: u32,
    pub height: u32,
    #[serde(default = "fm_default_true")]
    pub resizable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForgePermissions {
    #[serde(default = "fm_default_true")]
    pub storage: bool,
    #[serde(default)]
    pub agent: bool,
    #[serde(default)]
    pub network: Vec<String>,
    #[serde(default)]
    pub notification: bool,
}

impl Default for ForgePermissions {
    fn default() -> Self {
        Self {
            storage: true,
            agent: false,
            network: Vec::new(),
            notification: false,
        }
    }
}

const fn fm_default_true() -> bool {
    true
}

const DELIM: &str = "+++";

/// Splits a file into (frontmatter_toml, body_markdown). Returns
/// `RfcError::Frontmatter` if delimiters are missing or malformed.
pub fn parse_frontmatter(content: &str) -> Result<(RfcFrontmatter, String), RfcError> {
    let rest = content
        .strip_prefix(DELIM)
        .ok_or_else(|| RfcError::Frontmatter {
            message: "missing opening +++".into(),
        })?;
    let rest = rest.strip_prefix('\n').unwrap_or(rest);
    let close = rest
        .find(&format!("\n{DELIM}"))
        .ok_or_else(|| RfcError::Frontmatter {
            message: "missing closing +++".into(),
        })?;
    let toml_src = &rest[..close];
    let after = &rest[close + 1 + DELIM.len()..];
    let body = after.strip_prefix('\n').unwrap_or(after).to_string();
    let fm: RfcFrontmatter = toml::from_str(toml_src).map_err(|e| RfcError::Frontmatter {
        message: e.to_string(),
    })?;
    Ok((fm, body))
}

/// Renders a full file as `+++\n<toml>\n+++\n<body>`.
pub fn render_frontmatter(fm: &RfcFrontmatter, body: &str) -> Result<String, RfcError> {
    let toml_src = toml::to_string_pretty(fm).map_err(|e| RfcError::Frontmatter {
        message: e.to_string(),
    })?;
    let body_norm = if body.starts_with('\n') {
        body.to_string()
    } else {
        format!("\n{body}")
    };
    Ok(format!("{DELIM}\n{toml_src}{DELIM}{body_norm}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn fixture() -> RfcFrontmatter {
        let ts = FixedOffset::east_opt(0)
            .and_then(|tz| tz.with_ymd_and_hms(2026, 5, 22, 14, 0, 0).single())
            .expect("fixture timestamp");
        RfcFrontmatter {
            id: RfcId::new("auth-pkce"),
            state: RfcState::DraftEmpty,
            version: 1,
            created_at: ts,
            updated_at: ts,
            content_hash: ContentHash("sha256:abc".into()),
            agents: vec![],
            locked_by: None,
            locked_at: None,
            title: Some("PKCE".into()),
            status: None,
            assigned: None,
            kind: None,
            forge: None,
            scope: None,
            operator: None,
        }
    }

    #[test]
    fn round_trip() {
        let fm = fixture();
        let body = "# RFC\n\nhello\n";
        let rendered = render_frontmatter(&fm, body).expect("render");
        assert!(rendered.starts_with("+++\n"));
        let (parsed, parsed_body) = parse_frontmatter(&rendered).expect("parse");
        assert_eq!(parsed.id, fm.id);
        assert_eq!(parsed.state, fm.state);
        assert_eq!(parsed.version, fm.version);
        assert_eq!(parsed_body, body);
    }

    #[test]
    fn parse_operator_constraints() {
        let src = "+++\n\
id = \"x\"\n\
state = \"draft-active\"\n\
version = 2\n\
created_at = \"2026-05-22T14:00:00+00:00\"\n\
updated_at = \"2026-05-22T14:00:00+00:00\"\n\
content_hash = \"sha256:0\"\n\
\n\
[operator.constraints]\n\
allowed_paths = [\"orkia/**\"]\n\
forbidden_paths = [\"orkia-private/**\"]\n\
forbidden_commands = [\"git push*\"]\n\
risk_ceiling = \"high\"\n\
watch_paths = [\"orkia/crates/orkia-shell/**\"]\n\
+++\n\
body\n";
        let (fm, _) = parse_frontmatter(src).expect("parse");
        let constraints = fm
            .operator
            .and_then(|o| o.constraints)
            .expect("operator constraints");
        assert_eq!(constraints.allowed_paths, vec!["orkia/**"]);
        assert_eq!(constraints.risk_ceiling.as_deref(), Some("high"));
    }

    #[test]
    fn parse_kebab_case_state() {
        let src = "+++\nid = \"x\"\nstate = \"draft-active\"\nversion = 2\ncreated_at = \"2026-05-22T14:00:00+00:00\"\nupdated_at = \"2026-05-22T14:00:00+00:00\"\ncontent_hash = \"sha256:0\"\n+++\nbody\n";
        let (fm, body) = parse_frontmatter(src).expect("parse");
        assert_eq!(fm.state, RfcState::DraftActive);
        assert_eq!(fm.version, 2);
        assert_eq!(body, "body\n");
    }

    #[test]
    fn parse_forge_app_kind_with_block() {
        let src = "+++\n\
id = \"hello-orkia\"\n\
state = \"draft-empty\"\n\
version = 1\n\
created_at = \"2026-05-22T14:00:00+00:00\"\n\
updated_at = \"2026-05-22T14:00:00+00:00\"\n\
content_hash = \"sha256:0\"\n\
kind = \"forge-app\"\n\
\n\
[forge]\n\
name = \"hello-orkia\"\n\
description = \"hi\"\n\
\n\
[forge.window]\n\
title = \"Hello\"\n\
width = 480\n\
height = 320\n\
\n\
[forge.permissions]\n\
storage = true\n\
agent = false\n\
network = []\n\
notification = false\n\
+++\nbody\n";
        let (fm, _body) = parse_frontmatter(src).expect("parse");
        assert_eq!(fm.kind.as_deref(), Some("forge-app"));
        let forge = fm.forge.expect("forge block");
        assert_eq!(forge.name, "hello-orkia");
        assert_eq!(forge.window.title, "Hello");
        assert_eq!(forge.window.width, 480);
        assert!(forge.window.resizable); // default
        assert!(forge.permissions.storage);
        assert!(!forge.permissions.agent);
    }

    #[test]
    fn legacy_rfc_without_kind_still_parses() {
        // Identical to parse_kebab_case_state — guards backward compat.
        let src = "+++\nid = \"x\"\nstate = \"draft-active\"\nversion = 2\ncreated_at = \"2026-05-22T14:00:00+00:00\"\nupdated_at = \"2026-05-22T14:00:00+00:00\"\ncontent_hash = \"sha256:0\"\n+++\nbody\n";
        let (fm, _) = parse_frontmatter(src).expect("parse");
        assert!(fm.kind.is_none());
        assert!(fm.forge.is_none());
    }

    #[test]
    fn missing_delim_errors() {
        assert!(parse_frontmatter("no frontmatter here").is_err());
        assert!(parse_frontmatter("+++\nfoo = 1\n").is_err());
    }
}
