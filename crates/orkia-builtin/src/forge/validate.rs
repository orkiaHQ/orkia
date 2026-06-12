// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use orkia_forge_types::{Permissions, WindowConfig, validate_app_name};
use orkia_rfc_core::RfcRecord;
use orkia_rfc_core::frontmatter::{ForgeFrontmatterBlock, ForgePermissions, ForgeWindow};
use orkia_shell_types::BuilderError;

/// Output of `validate()`: the RFC has been confirmed to be a forge-app and
/// the `[forge]` block has been promoted to the manifest-shaped types used
/// downstream by the scaffolder.
#[derive(Debug, Clone)]
pub struct ValidatedForge {
    pub name: String,
    pub description: String,
    pub icon: String,
    pub window: WindowConfig,
    pub permissions: Permissions,
}

const MIN_WIDTH: u32 = 200;
const MAX_WIDTH: u32 = 4000;
const MIN_HEIGHT: u32 = 150;
const MAX_HEIGHT: u32 = 4000;

pub fn validate(rfc: &RfcRecord) -> Result<ValidatedForge, BuilderError> {
    let kind = rfc.fm.kind.as_deref();
    if kind != Some("forge-app") {
        return Err(BuilderError::InvalidRfc(format!(
            "RFC kind must be \"forge-app\", got {}",
            kind.map(|k| format!("\"{k}\""))
                .unwrap_or_else(|| "<unset>".into())
        )));
    }
    let block = rfc.fm.forge.as_ref().ok_or_else(|| {
        BuilderError::InvalidRfc("[forge] block missing from RFC frontmatter".into())
    })?;

    validate_app_name(&block.name)
        .map_err(|e| BuilderError::InvalidRfc(format!("forge.name: {e}")))?;
    validate_window(&block.window)?;
    if let Some(agent) = &block.agent {
        validate_agent_tools(agent)?;
    }

    Ok(promote(block))
}

/// agents (those run with the full runtime tool surface, which a Forge
/// app's embedded agent must not have access to).
fn validate_agent_tools(
    agent: &orkia_rfc_core::frontmatter::ForgeAgentBlock,
) -> Result<(), BuilderError> {
    if agent.tools.shell {
        return Err(BuilderError::InvalidRfc(
            "forge.agent.tools.shell = true is not allowed in V2".into(),
        ));
    }
    if agent.tools.filesystem {
        return Err(BuilderError::InvalidRfc(
            "forge.agent.tools.filesystem = true is not allowed in V2".into(),
        ));
    }
    if agent.archetype.trim().is_empty() {
        return Err(BuilderError::InvalidRfc(
            "forge.agent.archetype must not be empty".into(),
        ));
    }
    if agent.system_prompt.trim().is_empty() {
        return Err(BuilderError::InvalidRfc(
            "forge.agent.system_prompt must not be empty".into(),
        ));
    }
    Ok(())
}

fn validate_window(w: &ForgeWindow) -> Result<(), BuilderError> {
    if !(MIN_WIDTH..=MAX_WIDTH).contains(&w.width) {
        return Err(BuilderError::InvalidRfc(format!(
            "forge.window.width must be in [{MIN_WIDTH}, {MAX_WIDTH}], got {}",
            w.width
        )));
    }
    if !(MIN_HEIGHT..=MAX_HEIGHT).contains(&w.height) {
        return Err(BuilderError::InvalidRfc(format!(
            "forge.window.height must be in [{MIN_HEIGHT}, {MAX_HEIGHT}], got {}",
            w.height
        )));
    }
    if w.title.is_empty() {
        return Err(BuilderError::InvalidRfc(
            "forge.window.title must not be empty".into(),
        ));
    }
    Ok(())
}

fn promote(block: &ForgeFrontmatterBlock) -> ValidatedForge {
    ValidatedForge {
        name: block.name.clone(),
        description: block.description.clone(),
        icon: block.icon.clone().unwrap_or_else(|| "default".into()),
        window: WindowConfig {
            title: block.window.title.clone(),
            width: block.window.width,
            height: block.window.height,
            resizable: block.window.resizable,
        },
        permissions: promote_permissions(&block.permissions),
    }
}

fn promote_permissions(p: &ForgePermissions) -> Permissions {
    Permissions {
        storage: p.storage,
        agent: p.agent,
        network: p.network.clone(),
        notification: p.notification,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, TimeZone};
    use orkia_rfc_core::frontmatter::{ForgePermissions, ForgeWindow};
    use orkia_rfc_core::{ContentHash, RfcFrontmatter, RfcId, RfcState};

    fn make_rfc(kind: Option<&str>, forge: Option<ForgeFrontmatterBlock>) -> RfcRecord {
        let ts = FixedOffset::east_opt(0)
            .unwrap()
            .with_ymd_and_hms(2026, 5, 22, 14, 0, 0)
            .unwrap();
        RfcRecord {
            fm: RfcFrontmatter {
                id: RfcId::new("hello"),
                state: RfcState::DraftEmpty,
                version: 1,
                created_at: ts,
                updated_at: ts,
                content_hash: ContentHash("sha256:0".into()),
                agents: vec![],
                locked_by: None,
                locked_at: None,
                title: None,
                status: None,
                assigned: None,
                kind: kind.map(str::to_string),
                forge,
                scope: None,
                operator: None,
            },
            body: String::new(),
        }
    }

    fn ok_block() -> ForgeFrontmatterBlock {
        ForgeFrontmatterBlock {
            name: "hello-orkia".into(),
            description: "demo".into(),
            icon: None,
            window: ForgeWindow {
                title: "Hello".into(),
                width: 480,
                height: 320,
                resizable: true,
            },
            permissions: ForgePermissions::default(),
            agent: None,
        }
    }

    fn ok_agent() -> orkia_rfc_core::frontmatter::ForgeAgentBlock {
        orkia_rfc_core::frontmatter::ForgeAgentBlock {
            archetype: "data-fetcher".into(),
            description: Some("scrapes pricing pages".into()),
            model: None,
            max_invocations_per_hour: Some(60),
            max_cost_cents_per_invocation: Some(50),
            system_prompt: "You are a price scraper.".into(),
            tools: orkia_rfc_core::frontmatter::ForgeAgentTools {
                fetch: true,
                shell: false,
                filesystem: false,
            },
        }
    }

    #[test]
    fn accepts_well_formed_agent_block() {
        let mut b = ok_block();
        b.agent = Some(ok_agent());
        let rfc = make_rfc(Some("forge-app"), Some(b));
        validate(&rfc).unwrap();
    }

    #[test]
    fn rejects_agent_with_shell_tool() {
        let mut b = ok_block();
        let mut a = ok_agent();
        a.tools.shell = true;
        b.agent = Some(a);
        let rfc = make_rfc(Some("forge-app"), Some(b));
        let err = validate(&rfc).unwrap_err();
        match err {
            BuilderError::InvalidRfc(msg) => assert!(msg.contains("shell")),
            _ => panic!("wrong error"),
        }
    }

    #[test]
    fn rejects_agent_with_filesystem_tool() {
        let mut b = ok_block();
        let mut a = ok_agent();
        a.tools.filesystem = true;
        b.agent = Some(a);
        let rfc = make_rfc(Some("forge-app"), Some(b));
        let err = validate(&rfc).unwrap_err();
        match err {
            BuilderError::InvalidRfc(msg) => assert!(msg.contains("filesystem")),
            _ => panic!("wrong error"),
        }
    }

    #[test]
    fn rejects_agent_with_empty_system_prompt() {
        let mut b = ok_block();
        let mut a = ok_agent();
        a.system_prompt = "   ".into();
        b.agent = Some(a);
        let rfc = make_rfc(Some("forge-app"), Some(b));
        assert!(matches!(validate(&rfc), Err(BuilderError::InvalidRfc(_))));
    }

    #[test]
    fn rejects_missing_kind() {
        let rfc = make_rfc(None, Some(ok_block()));
        let err = validate(&rfc).unwrap_err();
        assert!(matches!(err, BuilderError::InvalidRfc(_)));
    }

    #[test]
    fn rejects_wrong_kind() {
        let rfc = make_rfc(Some("task"), Some(ok_block()));
        assert!(matches!(validate(&rfc), Err(BuilderError::InvalidRfc(_))));
    }

    #[test]
    fn rejects_missing_block() {
        let rfc = make_rfc(Some("forge-app"), None);
        assert!(matches!(validate(&rfc), Err(BuilderError::InvalidRfc(_))));
    }

    #[test]
    fn rejects_bad_name() {
        let mut b = ok_block();
        b.name = "Bad_Name".into();
        let rfc = make_rfc(Some("forge-app"), Some(b));
        assert!(matches!(validate(&rfc), Err(BuilderError::InvalidRfc(_))));
    }

    #[test]
    fn rejects_out_of_range_width() {
        let mut b = ok_block();
        b.window.width = 100;
        let rfc = make_rfc(Some("forge-app"), Some(b));
        assert!(matches!(validate(&rfc), Err(BuilderError::InvalidRfc(_))));
    }

    #[test]
    fn rejects_out_of_range_height() {
        let mut b = ok_block();
        b.window.height = 9999;
        let rfc = make_rfc(Some("forge-app"), Some(b));
        assert!(matches!(validate(&rfc), Err(BuilderError::InvalidRfc(_))));
    }

    #[test]
    fn rejects_empty_title() {
        let mut b = ok_block();
        b.window.title = String::new();
        let rfc = make_rfc(Some("forge-app"), Some(b));
        assert!(matches!(validate(&rfc), Err(BuilderError::InvalidRfc(_))));
    }

    #[test]
    fn accepts_canonical() {
        let rfc = make_rfc(Some("forge-app"), Some(ok_block()));
        let v = validate(&rfc).unwrap();
        assert_eq!(v.name, "hello-orkia");
        assert_eq!(v.window.width, 480);
        assert!(v.permissions.storage);
    }
}
