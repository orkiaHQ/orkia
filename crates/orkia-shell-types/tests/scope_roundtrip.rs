// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Round-trip tests for the `scope` field across every TOML model
//! that PR1b extended (workspace config is `ShellConfig` in
//! `orkia-shell`, not this crate, so its round-trip lives in
//! `orkia-shell/tests`). For each model: write with `Scope::Team`,
//! parse back, then write with `None` and confirm the field is absent.

use orkia_shell_types::scope::Scope;
use orkia_shell_types::workspace::{RfcFrontmatter, Workspace};
use tempfile::tempdir;

#[test]
fn project_toml_scope_roundtrips_team() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("projects");
    std::fs::create_dir_all(&root).unwrap();
    let pdir = Workspace::create_project(&root, "demo", Some("desc"), Some(Scope::Team)).unwrap();

    let toml = std::fs::read_to_string(pdir.join("project.toml")).unwrap();
    assert!(
        toml.contains("scope = \"team\""),
        "project.toml missing scope line:\n{toml}"
    );

    let ws = Workspace::load(dir.path());
    let project = &ws.projects[0];
    assert_eq!(project.scope, Some(Scope::Team));
}

#[test]
fn project_toml_omits_scope_when_none() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("projects");
    std::fs::create_dir_all(&root).unwrap();
    let pdir = Workspace::create_project(&root, "demo", None, None).unwrap();

    let toml = std::fs::read_to_string(pdir.join("project.toml")).unwrap();
    assert!(
        !toml.contains("scope"),
        "scope must be omitted when None:\n{toml}"
    );

    let ws = Workspace::load(dir.path());
    assert_eq!(ws.projects[0].scope, None);
}

#[test]
fn issue_toml_scope_roundtrips_team() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("projects");
    std::fs::create_dir_all(&root).unwrap();
    let pdir = Workspace::create_project(&root, "demo", None, None).unwrap();
    Workspace::create_issue(&pdir, "Fix CORS", "high", Some(Scope::Team)).unwrap();

    let issue_path = std::fs::read_dir(pdir.join("issues"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let toml = std::fs::read_to_string(&issue_path).unwrap();
    assert!(
        toml.contains("scope = \"team\""),
        "issue.toml missing scope line:\n{toml}"
    );

    let ws = Workspace::load(dir.path());
    assert_eq!(ws.projects[0].issues[0].scope, Some(Scope::Team));
}

#[test]
fn issue_toml_omits_scope_when_none() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("projects");
    std::fs::create_dir_all(&root).unwrap();
    let pdir = Workspace::create_project(&root, "demo", None, None).unwrap();
    Workspace::create_issue(&pdir, "Fix CORS", "high", None).unwrap();

    let issue_path = std::fs::read_dir(pdir.join("issues"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let toml = std::fs::read_to_string(&issue_path).unwrap();
    assert!(
        !toml.contains("scope"),
        "scope must be omitted when None:\n{toml}"
    );
    let ws = Workspace::load(dir.path());
    assert_eq!(ws.projects[0].issues[0].scope, None);
}

#[test]
fn rfc_frontmatter_mirror_roundtrips_team() {
    // The legacy mirror in orkia-shell-types::workspace lives alongside
    // the canonical type in orkia-rfc-core. Both must accept the same
    // wire form. Construct a minimal TOML body and round-trip it.
    let toml_src = "title = \"PKCE\"\nstatus = \"active\"\nscope = \"team\"\n";
    let parsed: RfcFrontmatter = toml::from_str(toml_src).unwrap();
    assert_eq!(parsed.scope, Some(Scope::Team));

    let rendered = toml::to_string(&parsed).unwrap();
    let back: RfcFrontmatter = toml::from_str(&rendered).unwrap();
    assert_eq!(back.scope, Some(Scope::Team));
}

#[test]
fn rfc_frontmatter_mirror_omits_scope_when_none() {
    let toml_src = "title = \"PKCE\"\nstatus = \"active\"\n";
    let parsed: RfcFrontmatter = toml::from_str(toml_src).unwrap();
    assert_eq!(parsed.scope, None);

    let rendered = toml::to_string(&parsed).unwrap();
    assert!(
        !rendered.contains("scope"),
        "scope must be omitted in serialized form when None:\n{rendered}"
    );
}

#[test]
fn rfc_frontmatter_canonical_roundtrips_scope_with_rfc_core() {
    use chrono::{FixedOffset, TimeZone};
    use orkia_rfc_core::frontmatter::{parse_frontmatter, render_frontmatter};
    use orkia_rfc_core::{ContentHash, RfcFrontmatter as CoreFrontmatter, RfcId, RfcState, Scope};

    let ts = FixedOffset::east_opt(0)
        .unwrap()
        .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
        .unwrap();
    let fm = CoreFrontmatter {
        id: RfcId::new("scoped"),
        state: RfcState::DraftEmpty,
        version: 1,
        created_at: ts,
        updated_at: ts,
        content_hash: ContentHash("sha256:abc".into()),
        agents: vec![],
        locked_by: None,
        locked_at: None,
        title: Some("Scoped RFC".into()),
        status: Some("draft".into()),
        assigned: None,
        kind: None,
        forge: None,
        operator: None,
        scope: Some(Scope::Private),
        dispatch: None,
    };
    let rendered = render_frontmatter(&fm, "# body\n").unwrap();
    assert!(rendered.contains("scope = \"private\""));

    let (back, _body) = parse_frontmatter(&rendered).unwrap();
    assert_eq!(back.scope, Some(Scope::Private));
}
