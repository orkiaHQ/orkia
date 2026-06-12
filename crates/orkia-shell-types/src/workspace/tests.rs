// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

#[cfg(test)]
mod tests {
    use std::path::Path;
    use tempfile::TempDir;

    use super::super::{Workspace, parse_rfc_frontmatter, slug};

    #[test]
    fn slug_basic() {
        assert_eq!(slug("Hello World"), "hello-world");
        assert_eq!(slug("Fix CORS!"), "fix-cors");
        assert_eq!(slug("   "), "untitled");
        assert_eq!(slug("Multi   Space"), "multi-space");
    }

    #[test]
    fn frontmatter_parses() {
        let content = "+++\ntitle = \"X\"\nstatus = \"active\"\n+++\nbody";
        let (fm, body) = parse_rfc_frontmatter(content);
        let fm = fm.unwrap();
        assert_eq!(fm.title.as_deref(), Some("X"));
        assert_eq!(fm.status.as_deref(), Some("active"));
        assert_eq!(body, "body");
    }

    #[test]
    fn frontmatter_missing() {
        let (fm, body) = parse_rfc_frontmatter("plain body");
        assert!(fm.is_none());
        assert_eq!(body, "plain body");
    }

    #[test]
    fn create_and_load_project() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("projects");
        std::fs::create_dir_all(&root).unwrap();

        let pdir = Workspace::create_project(&root, "Demo Project", Some("desc"), None).unwrap();
        Workspace::create_rfc(&pdir, "Auth Flow", &["faye".into()]).unwrap();
        Workspace::create_issue(&pdir, "Fix CORS", "high", None).unwrap();
        Workspace::create_issue(&pdir, "Add tests", "low", None).unwrap();

        let ws = Workspace::load(tmp.path());
        assert_eq!(ws.projects.len(), 1);
        let p = &ws.projects[0];
        assert_eq!(p.name, "Demo Project");
        assert_eq!(p.rfcs.len(), 1);
        assert_eq!(p.rfcs[0].title, "Auth Flow");
        assert_eq!(p.issues.len(), 2);
        assert_eq!(p.issues[0].number, 1);
        assert_eq!(p.issues[1].number, 2);
    }

    #[test]
    fn issue_update_status() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("projects");
        std::fs::create_dir_all(&root).unwrap();
        let pdir = Workspace::create_project(&root, "x", None, None).unwrap();
        Workspace::create_issue(&pdir, "T", "low", None).unwrap();
        Workspace::update_issue(&pdir, 1, "status", "done").unwrap();
        let ws = Workspace::load(tmp.path());
        assert_eq!(ws.projects[0].issues[0].status, "done");
    }

    #[test]
    fn titles_with_quotes_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("projects");
        std::fs::create_dir_all(&root).unwrap();
        let pdir = Workspace::create_project(&root, "x", None, None).unwrap();
        Workspace::create_issue(&pdir, "fix \"weird\" bug", "high", None).unwrap();
        let ws = Workspace::load(tmp.path());
        assert_eq!(ws.projects[0].issues.len(), 1);
        assert_eq!(ws.projects[0].issues[0].title, "fix \"weird\" bug");
    }

    #[test]
    fn auto_increment_skips_to_next() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("projects");
        std::fs::create_dir_all(&root).unwrap();
        let pdir = Workspace::create_project(&root, "x", None, None).unwrap();
        Workspace::create_issue(&pdir, "a", "low", None).unwrap();
        Workspace::create_issue(&pdir, "b", "low", None).unwrap();
        Workspace::create_issue(&pdir, "c", "low", None).unwrap();
        let ws = Workspace::load(tmp.path());
        let nums: Vec<u32> = ws.projects[0].issues.iter().map(|i| i.number).collect();
        assert_eq!(nums, vec![1, 2, 3]);
    }

    #[test]
    fn update_rfc_scalar_and_array_fields() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("projects");
        std::fs::create_dir_all(&root).unwrap();
        let pdir = Workspace::create_project(&root, "proj", None, None).unwrap();
        Workspace::create_rfc(&pdir, "Auth PKCE", &["faye".into()]).unwrap();

        let (_, old_status) =
            Workspace::update_rfc(&pdir, "auth-pkce", "status", "active").unwrap();
        assert_eq!(old_status, "draft");

        let (_, old_assigned) =
            Workspace::update_rfc(&pdir, "auth-pkce", "assigned", "sage,faye").unwrap();
        assert_eq!(old_assigned, "faye");

        let ws = Workspace::load(tmp.path());
        let rfc = &ws.projects[0].rfcs[0];
        assert_eq!(rfc.status, "active");
        assert_eq!(rfc.assigned, vec!["sage".to_string(), "faye".to_string()]);
    }

    #[test]
    fn resolve_project_name_precedence() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("projects");
        std::fs::create_dir_all(&root).unwrap();
        Workspace::create_project(&root, "alpha", None, None).unwrap();
        Workspace::create_project(&root, "beta", None, None).unwrap();
        let ws = Workspace::load(tmp.path());

        let cwd = root.join("beta").join("src");
        // Flag wins over everything.
        assert_eq!(
            ws.resolve_project_name(Some("alpha"), &cwd, Some("beta")),
            Some("alpha".into())
        );
        // Config default wins over cwd when matching.
        assert_eq!(
            ws.resolve_project_name(None, &cwd, Some("alpha")),
            Some("alpha".into())
        );
        // cwd ancestor fallback.
        assert_eq!(
            ws.resolve_project_name(None, &cwd, None),
            Some("beta".into())
        );
        // Bogus config default is ignored, falls through to cwd.
        assert_eq!(
            ws.resolve_project_name(None, &cwd, Some("ghost")),
            Some("beta".into())
        );
        // No flag, no default, no cwd match → None.
        assert_eq!(ws.resolve_project_name(None, Path::new("/tmp"), None), None);
    }

    #[test]
    fn create_rfc_frontmatter_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("projects");
        std::fs::create_dir_all(&root).unwrap();
        let pdir = Workspace::create_project(&root, "proj", None, None).unwrap();
        let rfc_path =
            Workspace::create_rfc(&pdir, "Auth PKCE", &["faye".into(), "sage".into()]).unwrap();
        let content = std::fs::read_to_string(&rfc_path).unwrap();
        let (fm, _body) = parse_rfc_frontmatter(&content);
        let fm = fm.expect("frontmatter must parse");
        assert_eq!(fm.title.as_deref(), Some("Auth PKCE"));
        assert_eq!(fm.status.as_deref(), Some("draft"));
        assert_eq!(
            fm.assigned.as_deref(),
            Some(["faye".to_string(), "sage".to_string()].as_slice())
        );
    }

    /// Closes Gap #2 from the verification report: a file created via the
    /// legacy `Workspace::create_rfc` must parse with the state-machine
    /// frontmatter parser too. Before delegation, the new fields were
    /// missing and `orkia_rfc_core::parse_frontmatter` rejected the file.
    #[test]
    fn legacy_create_is_state_machine_compatible() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("projects");
        std::fs::create_dir_all(&root).unwrap();
        let pdir = Workspace::create_project(&root, "proj", None, None).unwrap();
        let rfc_path = Workspace::create_rfc(&pdir, "Auth PKCE", &["faye".into()]).unwrap();
        let content = std::fs::read_to_string(&rfc_path).unwrap();
        let (fm, _body) =
            orkia_rfc_core::parse_frontmatter(&content).expect("state-machine parser");
        assert_eq!(fm.state, orkia_rfc_core::RfcState::DraftEmpty);
        assert_eq!(fm.version, 1);
        assert_eq!(fm.title.as_deref(), Some("Auth PKCE"));
        assert!(fm.content_hash.as_str().starts_with("sha256:"));
        // Legacy mirrors still populated for the workspace loader.
        assert_eq!(fm.status.as_deref(), Some("draft"));
        assert_eq!(
            fm.assigned.as_deref(),
            Some(["faye".to_string()].as_slice())
        );
    }
}
