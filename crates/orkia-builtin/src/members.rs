// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `members` builtin — parse `members {ls, add, rm, role}`.

#[derive(Debug, Clone, PartialEq)]
pub enum MembersAction {
    /// `members ls [--team <id>]`. Without `--team`, lists workspace
    /// members; with `--team`, lists that team's members. The shell
    /// substitutes the current_team when `--team` is omitted.
    List { team: Option<String> },
    /// `members add <email-or-account-id> --role <r> [--team <id>] [--agent <name>] [--project <id>]`.
    Add {
        target: String,
        role: String,
        team: Option<String>,
        agent: Option<String>,
        project: Option<String>,
    },
    /// `members rm <id> [--team <id>] [--agent <name>] [--project <id>]`.
    Remove {
        target: String,
        team: Option<String>,
        agent: Option<String>,
        project: Option<String>,
    },
    /// `members role <id> <new-role> [--team <id>]`.
    Role {
        target: String,
        new_role: String,
        team: Option<String>,
    },
}

pub fn parse(args: &[String]) -> Result<MembersAction, String> {
    let sub = args.first().map(String::as_str).unwrap_or("ls");
    match sub {
        "ls" | "list" => {
            let team = take_flag(&args[1..], "--team")?;
            Ok(MembersAction::List { team })
        }
        "add" => {
            let target = args
                .get(1)
                .cloned()
                .ok_or_else(|| "usage: members add <email|account-id> --role R [--team T] [--agent N] [--project P]".to_string())?;
            let rest = &args[2..];
            let role = take_flag(rest, "--role")?
                .ok_or_else(|| "members add: --role is required".to_string())?;
            Ok(MembersAction::Add {
                target,
                role,
                team: take_flag(rest, "--team")?,
                agent: take_flag(rest, "--agent")?,
                project: take_flag(rest, "--project")?,
            })
        }
        "rm" | "remove" => {
            let target = args
                .get(1)
                .cloned()
                .ok_or_else(|| "usage: members rm <id> [--team T] [--project P]".to_string())?;
            let rest = &args[2..];
            Ok(MembersAction::Remove {
                target,
                team: take_flag(rest, "--team")?,
                agent: take_flag(rest, "--agent")?,
                project: take_flag(rest, "--project")?,
            })
        }
        "role" => {
            let target = args
                .get(1)
                .cloned()
                .ok_or_else(|| "usage: members role <id> <new-role> [--team T]".to_string())?;
            let new_role = args
                .get(2)
                .cloned()
                .ok_or_else(|| "members role: missing new role".to_string())?;
            let rest = &args[3..];
            Ok(MembersAction::Role {
                target,
                new_role,
                team: take_flag(rest, "--team")?,
            })
        }
        other => Err(format!("unknown members subcommand: {other}")),
    }
}

fn take_flag(args: &[String], flag: &str) -> Result<Option<String>, String> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag {
            let v = args
                .get(i + 1)
                .cloned()
                .ok_or_else(|| format!("members: missing value for {flag}"))?;
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
    fn ls_without_team_is_workspace_scoped() {
        assert_eq!(
            parse(&s(&["ls"])).unwrap(),
            MembersAction::List { team: None }
        );
    }

    #[test]
    fn add_requires_role() {
        assert!(parse(&s(&["add", "u@e.com"])).is_err());
        let got = parse(&s(&["add", "u@e.com", "--role", "member"])).unwrap();
        assert_eq!(
            got,
            MembersAction::Add {
                target: "u@e.com".into(),
                role: "member".into(),
                team: None,
                agent: None,
                project: None,
            }
        );
    }
}
