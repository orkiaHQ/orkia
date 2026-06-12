// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `invite` builtin — parse `invite {create, ls, revoke, accept}`.

#[derive(Debug, Clone, PartialEq)]
pub enum InviteAction {
    /// `invite create <email> [--role <r>] [--ttl <days>]`.
    Create {
        email: String,
        role: String,
        ttl_days: i64,
    },
    /// `invite ls [--status <s>]`.
    List { status: Option<String> },
    /// `invite revoke <nonce>`.
    Revoke { nonce: String },
    /// `invite accept <nonce>` — callable unauthenticated; shell
    /// updates session afterwards.
    Accept { nonce: String },
}

pub fn parse(args: &[String]) -> Result<InviteAction, String> {
    let sub = args.first().map(String::as_str).unwrap_or("ls");
    match sub {
        "create" | "new" => {
            let email = args.get(1).cloned().ok_or_else(|| {
                "usage: invite create <email> [--role R] [--ttl DAYS]".to_string()
            })?;
            let mut role = "member".to_string();
            let mut ttl_days: i64 = 7;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--role" => {
                        role = value_for(args, i, "--role")?;
                        i += 2;
                    }
                    "--ttl" => {
                        let v = value_for(args, i, "--ttl")?;
                        ttl_days = v
                            .parse()
                            .map_err(|e| format!("invite: invalid --ttl '{v}': {e}"))?;
                        i += 2;
                    }
                    other => return Err(format!("invite create: unknown flag '{other}'")),
                }
            }
            Ok(InviteAction::Create {
                email,
                role,
                ttl_days,
            })
        }
        "ls" | "list" => {
            let mut status = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--status" => {
                        status = Some(value_for(args, i, "--status")?);
                        i += 2;
                    }
                    other => return Err(format!("invite ls: unknown flag '{other}'")),
                }
            }
            Ok(InviteAction::List { status })
        }
        "revoke" => {
            let nonce = args
                .get(1)
                .cloned()
                .ok_or_else(|| "usage: invite revoke <nonce>".to_string())?;
            Ok(InviteAction::Revoke { nonce })
        }
        "accept" => {
            let nonce = args
                .get(1)
                .cloned()
                .ok_or_else(|| "usage: invite accept <nonce>".to_string())?;
            Ok(InviteAction::Accept { nonce })
        }
        other => Err(format!("unknown invite subcommand: {other}")),
    }
}

fn value_for(args: &[String], i: usize, flag: &str) -> Result<String, String> {
    args.get(i + 1)
        .cloned()
        .ok_or_else(|| format!("invite: missing value for {flag}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn create_with_defaults() {
        let got = parse(&s(&["create", "u@e.com"])).unwrap();
        assert_eq!(
            got,
            InviteAction::Create {
                email: "u@e.com".into(),
                role: "member".into(),
                ttl_days: 7,
            }
        );
    }

    #[test]
    fn create_with_role_and_ttl() {
        let got = parse(&s(&["create", "u@e.com", "--role", "admin", "--ttl", "30"])).unwrap();
        assert_eq!(
            got,
            InviteAction::Create {
                email: "u@e.com".into(),
                role: "admin".into(),
                ttl_days: 30,
            }
        );
    }

    #[test]
    fn accept_requires_nonce() {
        assert!(parse(&s(&["accept"])).is_err());
        let got = parse(&s(&["accept", "abc"])).unwrap();
        assert_eq!(
            got,
            InviteAction::Accept {
                nonce: "abc".into()
            }
        );
    }
}
