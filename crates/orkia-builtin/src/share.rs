// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `share` builtin — parse `share {project, issue, unshare, ls}`.

#[derive(Debug, Clone, PartialEq)]
pub enum ShareAction {
    /// `share project <project> <target-ws> [--access A]`.
    Project {
        project: String,
        target_workspace: String,
        access: String,
    },
    /// `share issue <issue> <target-ws> [--access A]`.
    Issue {
        issue: String,
        target_workspace: String,
        access: String,
    },
    /// `share unshare project|issue <id> <target-ws>`.
    Unshare {
        kind: UnshareKind,
        id: String,
        target_workspace: String,
    },
    /// `share ls`.
    List,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UnshareKind {
    Project,
    Issue,
}

pub fn parse(args: &[String]) -> Result<ShareAction, String> {
    let sub = args.first().map(String::as_str).unwrap_or("ls");
    match sub {
        "ls" | "list" => Ok(ShareAction::List),
        "project" => {
            let project = args.get(1).cloned().ok_or_else(|| {
                "usage: share project <project> <target-ws> [--access A]".to_string()
            })?;
            let target_workspace = args
                .get(2)
                .cloned()
                .ok_or_else(|| "share project: missing target workspace".to_string())?;
            let access = take_flag(&args[3..], "--access")?.unwrap_or_else(|| "read".into());
            Ok(ShareAction::Project {
                project,
                target_workspace,
                access,
            })
        }
        "issue" => {
            let issue = args
                .get(1)
                .cloned()
                .ok_or_else(|| "usage: share issue <issue> <target-ws> [--access A]".to_string())?;
            let target_workspace = args
                .get(2)
                .cloned()
                .ok_or_else(|| "share issue: missing target workspace".to_string())?;
            let access = take_flag(&args[3..], "--access")?.unwrap_or_else(|| "read".into());
            Ok(ShareAction::Issue {
                issue,
                target_workspace,
                access,
            })
        }
        "unshare" => {
            let kind_str = args
                .get(1)
                .map(String::as_str)
                .ok_or_else(|| "usage: share unshare project|issue <id> <target-ws>".to_string())?;
            let kind = match kind_str {
                "project" => UnshareKind::Project,
                "issue" => UnshareKind::Issue,
                other => return Err(format!("share unshare: unknown kind '{other}'")),
            };
            let id = args
                .get(2)
                .cloned()
                .ok_or_else(|| "share unshare: missing id".to_string())?;
            let target_workspace = args
                .get(3)
                .cloned()
                .ok_or_else(|| "share unshare: missing target workspace".to_string())?;
            Ok(ShareAction::Unshare {
                kind,
                id,
                target_workspace,
            })
        }
        other => Err(format!("unknown share subcommand: {other}")),
    }
}

fn take_flag(args: &[String], flag: &str) -> Result<Option<String>, String> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag {
            let v = args
                .get(i + 1)
                .cloned()
                .ok_or_else(|| format!("share: missing value for {flag}"))?;
            return Ok(Some(v));
        }
        i += 1;
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn share_project_defaults_to_read_access() {
        let got = parse(&s(&["project", "P", "WS"])).unwrap();
        assert_eq!(
            got,
            ShareAction::Project {
                project: "P".into(),
                target_workspace: "WS".into(),
                access: "read".into(),
            }
        );
    }

    #[test]
    fn unshare_requires_kind_id_ws() {
        assert!(parse(&s(&["unshare"])).is_err());
        let got = parse(&s(&["unshare", "project", "P", "WS"])).unwrap();
        assert_eq!(
            got,
            ShareAction::Unshare {
                kind: UnshareKind::Project,
                id: "P".into(),
                target_workspace: "WS".into(),
            }
        );
    }
}
