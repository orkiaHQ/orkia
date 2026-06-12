// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

use std::collections::HashMap;

#[derive(Debug, Clone)]
pub enum AppAction {
    List,
    Run {
        name: String,
    },
    Edit {
        name: String,
    },
    Remove {
        name: String,
        confirm: Option<String>,
    },
    Inspect {
        name: String,
    },
    /// `orkia app usage` — current month/hour counts + recent builds.
    Usage,
    /// `orkia app plan` — plan info + upgrade link.
    Plan,
    /// V2: `orkia app perms <name>` — show manifest permissions + last-hour usage.
    Perms {
        name: String,
    },
    /// V2: `orkia app seal <name> [--since 1h] [--verify]` — inspect the
    /// per-app SEAL chain.
    Seal {
        name: String,
        since: Option<String>,
        verify: bool,
    },
    /// V2: `orkia app agent <name>` — show agent state + recent invocations.
    Agent {
        name: String,
    },
}

pub fn parse(args: &[String]) -> Result<AppAction, String> {
    let sub = args.first().map(String::as_str).unwrap_or("list");
    let rest = &args[args.len().min(1)..];
    let (positional, flags, bools) = split_flags(rest);

    match sub {
        "list" | "ls" => Ok(AppAction::List),
        "run" => {
            let name = positional
                .first()
                .cloned()
                .ok_or_else(|| "usage: orkia app run <app-name>".to_string())?;
            Ok(AppAction::Run { name })
        }
        "edit" => {
            let name = positional
                .first()
                .cloned()
                .ok_or_else(|| "usage: orkia app edit <app-name>".to_string())?;
            Ok(AppAction::Edit { name })
        }
        "remove" | "rm" | "delete" => {
            let name = positional.first().cloned().ok_or_else(|| {
                "usage: orkia app remove <app-name> [--confirm <name>]".to_string()
            })?;
            Ok(AppAction::Remove {
                name,
                confirm: flags.get("confirm").cloned(),
            })
        }
        "inspect" | "show" => {
            let name = positional
                .first()
                .cloned()
                .ok_or_else(|| "usage: orkia app inspect <app-name>".to_string())?;
            Ok(AppAction::Inspect { name })
        }
        "usage" => Ok(AppAction::Usage),
        "plan" => Ok(AppAction::Plan),
        "perms" | "permissions" => {
            let name = positional
                .first()
                .cloned()
                .ok_or_else(|| "usage: orkia app perms <app-name>".to_string())?;
            Ok(AppAction::Perms { name })
        }
        "seal" => {
            let name = positional.first().cloned().ok_or_else(|| {
                "usage: orkia app seal <app-name> [--since 1h] [--verify]".to_string()
            })?;
            Ok(AppAction::Seal {
                name,
                since: flags.get("since").cloned(),
                verify: bools.contains(&"verify".to_string()),
            })
        }
        "agent" => {
            let name = positional
                .first()
                .cloned()
                .ok_or_else(|| "usage: orkia app agent <app-name>".to_string())?;
            Ok(AppAction::Agent { name })
        }
        other => Err(format!("unknown app subcommand: {other}")),
    }
}

fn split_flags(args: &[String]) -> (Vec<String>, HashMap<String, String>, Vec<String>) {
    let mut positional = Vec::new();
    let mut value_flags = HashMap::new();
    let mut bool_flags = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(name) = a.strip_prefix("--") {
            let next = args.get(i + 1);
            let is_value = next.is_some_and(|v| !v.starts_with("--"));
            if is_value {
                if let Some(v) = next {
                    value_flags.insert(name.to_string(), v.clone());
                }
                i += 2;
            } else {
                bool_flags.push(name.to_string());
                i += 1;
            }
        } else {
            positional.push(a.clone());
            i += 1;
        }
    }
    (positional, value_flags, bool_flags)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn list_default() {
        assert!(matches!(parse(&[]).unwrap(), AppAction::List));
        assert!(matches!(parse(&s(&["list"])).unwrap(), AppAction::List));
    }

    #[test]
    fn run_requires_name() {
        assert!(parse(&s(&["run"])).is_err());
        match parse(&s(&["run", "hello"])).unwrap() {
            AppAction::Run { name } => assert_eq!(name, "hello"),
            _ => panic!(),
        }
    }

    #[test]
    fn remove_with_confirm() {
        match parse(&s(&["remove", "hello", "--confirm", "hello"])).unwrap() {
            AppAction::Remove { name, confirm } => {
                assert_eq!(name, "hello");
                assert_eq!(confirm.as_deref(), Some("hello"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn unknown_sub_errors() {
        assert!(parse(&s(&["banana"])).is_err());
    }
}
