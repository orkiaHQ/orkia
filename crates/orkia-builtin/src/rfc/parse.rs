// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::model::RfcAction;
use crate::scope_flag::parse_scope_flag;
use std::path::PathBuf;

/// Fields accepted by `rfc update --<field> <value>`. `scope` is handled
/// out-of-band via the shared `--scope` flag (so it round-trips through
/// `Scope::parse` for validation) but is also accepted here for parity
/// with the other fields.
const UPDATE_FIELDS: &[&str] = &["status", "assigned", "title", "priority", "tags", "scope"];

/// Accept `--yes` or `-y` as the V3 approval gate. Matches the
/// `agent remove --yes` idiom used elsewhere in the shell.
pub(super) fn has_confirm_flag(bools: &[String]) -> bool {
    bools.iter().any(|b| b == "yes" || b == "y")
}

pub fn parse(args: &[String]) -> Result<RfcAction, String> {
    // Strip --scope first so the existing flag splitter doesn't pick
    // it up as a generic value-flag (which would land it in the update
    // path's "first non-project flag" branch by accident).
    let (scope, args) = parse_scope_flag(args).map_err(|e| format!("rfc: invalid --scope: {e}"))?;

    let sub = args.first().map(String::as_str).unwrap_or("list");
    let rest = &args[args.len().min(1)..];
    let (positional, flags, bools) = split_flags(rest);

    match sub {
        "list" | "ls" => Ok(RfcAction::List {
            project: flags.get("project").cloned(),
            status: flags.get("status").cloned(),
        }),
        "show" => {
            let slug = positional
                .first()
                .cloned()
                .ok_or_else(|| "usage: orkia rfc show <slug> [--project <name>]".to_string())?;
            Ok(RfcAction::Show {
                slug,
                project: flags.get("project").cloned(),
            })
        }
        "create" | "new" => {
            let title = positional.first().cloned().ok_or_else(|| {
                "usage: orkia rfc create <title> [--project <name>] [--assigned a,b]".to_string()
            })?;
            let assigned = flags
                .get("assigned")
                .map(|s| {
                    s.split(',')
                        .map(|t| t.trim().to_string())
                        .filter(|t| !t.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            Ok(RfcAction::Create {
                title,
                project: flags.get("project").cloned(),
                assigned,
                scope,
            })
        }
        "edit" => {
            let slug = positional
                .first()
                .cloned()
                .ok_or_else(|| "usage: orkia rfc edit <slug> [--project <name>]".to_string())?;
            Ok(RfcAction::Edit {
                slug,
                project: flags.get("project").cloned(),
            })
        }
        "update" => {
            let slug = positional.first().cloned().ok_or_else(|| {
                "usage: orkia rfc update <slug> [--project <name>] (--<field> <value> | --scope <s>)"
                    .to_string()
            })?;

            // `--scope` is handled out-of-band: parsed via parse_scope_flag
            // above. If present, it is the update; combining it with another
            // field flag is rejected.
            if let Some(s) = scope {
                let other = flags.iter().find(|(k, _)| k.as_str() != "project");
                if other.is_some() {
                    return Err(
                        "rfc update: --scope cannot be combined with other field flags".into(),
                    );
                }
                return Ok(RfcAction::Update {
                    slug,
                    project: flags.get("project").cloned(),
                    field: "scope".into(),
                    value: s.as_str().to_string(),
                });
            }

            let mut field_hit: Option<(String, String)> = None;
            for f in UPDATE_FIELDS {
                if *f == "scope" {
                    continue; // already handled above
                }
                if let Some(v) = flags.get(*f) {
                    if field_hit.is_some() {
                        return Err(
                            "rfc update: specify exactly one of --status/--assigned/--title/--priority/--tags/--scope"
                                .into(),
                        );
                    }
                    field_hit = Some((f.to_string(), v.clone()));
                }
            }
            let (field, value) = field_hit.ok_or_else(|| {
                "rfc update: missing field flag (one of --status/--assigned/--title/--priority/--tags/--scope)"
                    .to_string()
            })?;
            Ok(RfcAction::Update {
                slug,
                project: flags.get("project").cloned(),
                field,
                value,
            })
        }
        "delegate" => {
            let slug = positional.first().cloned().ok_or_else(|| {
                "usage: orkia rfc delegate <slug> --agent <name> [--project <name>]".to_string()
            })?;
            let agent = flags
                .get("agent")
                .cloned()
                .ok_or_else(|| "rfc delegate: --agent <name> required".to_string())?;
            Ok(RfcAction::Delegate {
                slug,
                project: flags.get("project").cloned(),
                agent,
            })
        }
        "remove" | "rm" | "delete" => {
            let slug = positional.first().cloned().ok_or_else(|| {
                "usage: orkia rfc remove <slug> [--project <name>] [--force]".to_string()
            })?;
            Ok(RfcAction::Remove {
                slug,
                project: flags.get("project").cloned(),
                force: bools.contains(&"force".to_string()),
            })
        }
        "constraints" => parse_constraints_subcommand(positional, &flags),
        "state" => Ok(RfcAction::State {
            slug: positional.first().cloned(),
            project: flags.get("project").cloned(),
        }),
        "cd" => {
            let slug = positional
                .first()
                .cloned()
                .ok_or_else(|| "usage: orkia rfc cd <slug> [--project <name>]".to_string())?;
            Ok(RfcAction::Cd {
                slug,
                project: flags.get("project").cloned(),
            })
        }
        "exit" => Ok(RfcAction::ExitScope),
        "promote" => Ok(RfcAction::Promote {
            slug: positional.first().cloned(),
            project: flags.get("project").cloned(),
            confirm: has_confirm_flag(&bools),
        }),
        "complete" => Ok(RfcAction::Complete {
            slug: positional.first().cloned(),
            project: flags.get("project").cloned(),
            confirm: has_confirm_flag(&bools),
        }),
        "abandon" => {
            let reason = flags
                .get("reason")
                .or_else(|| flags.get("r"))
                .cloned()
                .ok_or_else(|| "usage: orkia rfc abandon [<slug>] -r <reason> --yes".to_string())?;
            Ok(RfcAction::Abandon {
                slug: positional.first().cloned(),
                project: flags.get("project").cloned(),
                reason,
                confirm: has_confirm_flag(&bools),
            })
        }
        "seal" => parse_seal_subcommand(positional, &flags, &bools),
        "reopen" => Ok(RfcAction::Reopen {
            slug: positional.first().cloned(),
            project: flags.get("project").cloned(),
            confirm: has_confirm_flag(&bools),
        }),
        "lock-status" => Ok(RfcAction::LockStatus {
            slug: positional.first().cloned(),
            project: flags.get("project").cloned(),
        }),
        "release-lock" => Ok(RfcAction::ReleaseLock {
            slug: positional.first().cloned(),
            project: flags.get("project").cloned(),
        }),
        "ask" => {
            let question = flags
                .get("q")
                .or_else(|| flags.get("question"))
                .cloned()
                .ok_or_else(|| {
                    "usage: orkia rfc ask [<slug>] --q <question> --rationale <why>".to_string()
                })?;
            let rationale = flags
                .get("rationale")
                .cloned()
                .ok_or_else(|| "rfc ask: --rationale required".to_string())?;
            Ok(RfcAction::Ask {
                slug: positional.first().cloned(),
                project: flags.get("project").cloned(),
                question,
                rationale,
            })
        }
        "resolve" => {
            let decision_id = positional.first().cloned().ok_or_else(|| {
                "usage: orkia rfc resolve <decision-id> --answer <text>".to_string()
            })?;
            let answer = flags
                .get("answer")
                .or_else(|| flags.get("a"))
                .cloned()
                .ok_or_else(|| "rfc resolve: --answer required".to_string())?;
            Ok(RfcAction::Resolve {
                slug: positional.get(1).cloned(),
                project: flags.get("project").cloned(),
                decision_id,
                answer,
            })
        }
        "forge" => {
            let rfc_id = positional.first().cloned().ok_or_else(|| {
                "usage: orkia rfc forge <rfc-id> [--project <name>] [--force] [--offline] [--rerun]"
                    .to_string()
            })?;
            Ok(RfcAction::Forge {
                rfc_id,
                project: flags.get("project").cloned(),
                force: bools.contains(&"force".to_string()),
                offline: bools.contains(&"offline".to_string()),
                rerun: bools.contains(&"rerun".to_string()),
                confirmed: has_confirm_flag(&bools),
            })
        }
        "dispatch" => {
            let slug = positional.first().cloned().ok_or_else(|| {
                "usage: orkia rfc dispatch <slug> [--project <name>] [--resume]".to_string()
            })?;
            Ok(RfcAction::Dispatch {
                slug,
                project: flags.get("project").cloned(),
                resume: bools.contains(&"resume".to_string()),
            })
        }
        "dispatch-task" => {
            let rfc_id = positional.first().cloned().ok_or_else(|| {
                "usage: orkia rfc dispatch-task <rfc-id> --task <id> --agent <name> [--project <name>]"
                    .to_string()
            })?;
            let task = flags
                .get("task")
                .cloned()
                .ok_or_else(|| "rfc dispatch-task: --task <id> required".to_string())?;
            let agent = flags
                .get("agent")
                .cloned()
                .ok_or_else(|| "rfc dispatch-task: --agent <name> required".to_string())?;
            Ok(RfcAction::DispatchTask {
                rfc_id,
                project: flags.get("project").cloned(),
                task,
                agent,
            })
        }
        other => Err(format!("unknown rfc subcommand: {other}")),
    }
}

fn parse_constraints_subcommand(
    positional: Vec<String>,
    flags: &std::collections::HashMap<String, String>,
) -> Result<RfcAction, String> {
    let op = positional
        .first()
        .map(String::as_str)
        .ok_or_else(|| "usage: orkia rfc constraints {propose|accept} <slug>".to_string())?;
    let slug = positional
        .get(1)
        .cloned()
        .ok_or_else(|| "usage: orkia rfc constraints {propose|accept} <slug>".to_string())?;
    match op {
        "propose" => Ok(RfcAction::ConstraintsPropose {
            slug,
            project: flags.get("project").cloned(),
        }),
        "accept" => Ok(RfcAction::ConstraintsAccept {
            slug,
            project: flags.get("project").cloned(),
            allowed_paths: csv_flag(flags, "allowed"),
            forbidden_paths: csv_flag(flags, "forbidden"),
            forbidden_commands: csv_flag(flags, "forbid-cmd"),
            risk_ceiling: flags.get("risk").cloned(),
            watch_paths: csv_flag(flags, "watch"),
        }),
        other => Err(format!("unknown rfc constraints subcommand: {other}")),
    }
}

fn csv_flag(flags: &std::collections::HashMap<String, String>, key: &str) -> Vec<String> {
    flags
        .get(key)
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

type ParsedFlags = (
    Vec<String>,
    std::collections::HashMap<String, String>,
    Vec<String>,
);

/// Returns `(positional, value_flags, bool_flags)`.
/// `--foo value` becomes `value_flags["foo"] = value`.
/// `--foo` with no following value (or followed by another `--flag`) becomes a bool.
pub(super) fn split_flags(args: &[String]) -> ParsedFlags {
    let mut positional = Vec::new();
    let mut value_flags = std::collections::HashMap::new();
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

///
/// Forms:
/// - `orkia rfc seal <slug>` — display the document, assembling if absent.
/// - `... --verify` — verify signature + chain.
/// - `... --rebuild` — force re-assembly even if a document exists.
/// - `... --output <path>` — write the document to a custom path.
/// - `orkia rfc seal --export-key <path>` — export the workspace signing key.
/// - `orkia rfc seal --import-key <path>` — import a previously exported key.
pub(super) fn parse_seal_subcommand(
    positional: Vec<String>,
    flags: &std::collections::HashMap<String, String>,
    bools: &[String],
) -> Result<RfcAction, String> {
    if let Some(path) = flags.get("export-key") {
        return Ok(RfcAction::SealExportKey {
            path: PathBuf::from(path),
        });
    }
    if let Some(path) = flags.get("import-key") {
        return Ok(RfcAction::SealImportKey {
            path: PathBuf::from(path),
        });
    }
    let slug = positional.first().cloned().ok_or_else(|| {
        "usage: orkia rfc seal <slug> [--verify] [--rebuild] [--output <path>]".to_string()
    })?;
    Ok(RfcAction::Seal {
        slug,
        project: flags.get("project").cloned(),
        verify: bools.iter().any(|b| b == "verify"),
        rebuild: bools.iter().any(|b| b == "rebuild"),
        output: flags.get("output").map(PathBuf::from),
    })
}
