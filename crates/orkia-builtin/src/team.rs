// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `team` builtin — parse `team {ls, create, rm, show, refresh}`
//!
//! Stateless: the parser only validates argv shape. The shell side
//! dispatches the action against a [`TeamClient`] (see
//! `orkia-shell-types::team`).

#[derive(Debug, Clone, PartialEq)]
pub enum TeamAction {
    /// `team ls` — list teams in current workspace.
    List,
    /// `team show <identifier-or-id>`.
    Show {
        target: String,
    },
    /// `team create <identifier> [--name N] [--description D] [--color C]`.
    Create {
        identifier: String,
        name: Option<String>,
        description: Option<String>,
        color: Option<String>,
    },
    /// `team rm <identifier-or-id> [--yes]`.
    Remove {
        target: String,
        confirmed: bool,
    },
    /// `team refresh` — force a re-bootstrap.
    Refresh,
    /// `team cd <identifier-or-id|--clear>` — set/clear current_team.
    /// top-level `$cd` builtin exists; `cd` is bash's.
    Cd {
        target: String,
    },
    Pwd,
    Join {
        nonce: String,
    },
}

pub fn parse(args: &[String]) -> Result<TeamAction, String> {
    let sub = args.first().map(String::as_str).unwrap_or("ls");
    match sub {
        "ls" | "list" => Ok(TeamAction::List),
        "show" => {
            let target = args
                .get(1)
                .cloned()
                .ok_or_else(|| "usage: team show <identifier-or-id>".to_string())?;
            Ok(TeamAction::Show { target })
        }
        "create" | "new" => {
            let identifier = args.get(1).cloned().ok_or_else(|| {
                "usage: team create <identifier> [--name N] [--description D] [--color C]"
                    .to_string()
            })?;
            let (mut name, mut description, mut color) = (None, None, None);
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--name" => {
                        name = Some(value_for(args, i, "--name")?);
                        i += 2;
                    }
                    "--description" => {
                        description = Some(value_for(args, i, "--description")?);
                        i += 2;
                    }
                    "--color" => {
                        color = Some(value_for(args, i, "--color")?);
                        i += 2;
                    }
                    other => return Err(format!("team create: unknown flag '{other}'")),
                }
            }
            Ok(TeamAction::Create {
                identifier,
                name,
                description,
                color,
            })
        }
        "rm" | "remove" | "delete" => {
            let target = args
                .get(1)
                .cloned()
                .ok_or_else(|| "usage: team rm <identifier-or-id> [--yes]".to_string())?;
            let confirmed = args.iter().skip(2).any(|a| a == "--yes" || a == "-y");
            Ok(TeamAction::Remove { target, confirmed })
        }
        "refresh" => Ok(TeamAction::Refresh),
        "cd" => {
            let target = args
                .get(1)
                .cloned()
                .ok_or_else(|| "usage: team cd <identifier-or-id|--clear>".to_string())?;
            Ok(TeamAction::Cd { target })
        }
        "pwd" => Ok(TeamAction::Pwd),
        "join" => {
            let nonce = args
                .get(1)
                .cloned()
                .ok_or_else(|| "usage: team join <nonce>".to_string())?;
            Ok(TeamAction::Join { nonce })
        }
        other => Err(format!("unknown team subcommand: {other}")),
    }
}

fn value_for(args: &[String], i: usize, flag: &str) -> Result<String, String> {
    args.get(i + 1)
        .cloned()
        .ok_or_else(|| format!("team: missing value for {flag}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn parses_list_default() {
        assert_eq!(parse(&[]).unwrap(), TeamAction::List);
        assert_eq!(parse(&s(&["ls"])).unwrap(), TeamAction::List);
        assert_eq!(parse(&s(&["list"])).unwrap(), TeamAction::List);
    }

    #[test]
    fn parses_create_with_flags() {
        let got = parse(&s(&[
            "create",
            "eng",
            "--name",
            "Engineering",
            "--color",
            "#FF0000",
        ]))
        .unwrap();
        assert_eq!(
            got,
            TeamAction::Create {
                identifier: "eng".into(),
                name: Some("Engineering".into()),
                description: None,
                color: Some("#FF0000".into()),
            }
        );
    }

    #[test]
    fn rm_with_yes_marks_confirmed() {
        let got = parse(&s(&["rm", "eng", "--yes"])).unwrap();
        assert_eq!(
            got,
            TeamAction::Remove {
                target: "eng".into(),
                confirmed: true,
            }
        );
    }

    #[test]
    fn unknown_subcommand_errors() {
        assert!(parse(&s(&["frobnicate"])).is_err());
    }

    #[test]
    fn parses_join_with_nonce() {
        let got = parse(&s(&["join", "abc123"])).unwrap();
        assert_eq!(
            got,
            TeamAction::Join {
                nonce: "abc123".into()
            }
        );
    }

    #[test]
    fn join_without_nonce_errors() {
        assert!(parse(&s(&["join"])).is_err());
    }
}
