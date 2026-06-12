// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `orkia config ...` — read/write keys in `<data_dir>/config.toml`.
//!
//! V1 covers exactly two operations:
//!
//! * `config get default_scope` — print the current value.
//! * `config set default_scope <private|team|public>` — update the file.
//!
//! Any other key is rejected with a usage hint. Other config knobs
//! (data_dir, default_project, ...) remain edit-by-hand for now —
//! adding them here means designing the broader UX for config that is
//! beyond PR2's surface.

use std::path::Path;

use orkia_shell_types::{BlockContent, Scope};

/// Parsed shape of `orkia config ...`. The handler operates on this
/// rather than raw `args` so it stays in step with the rest of the
/// builtin layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigAction {
    /// `config get <key>` — print the current value (`default_scope` only for V1).
    Get { key: String },
    /// `config set <key> <value>` — update the file.
    Set { key: String, value: String },
    /// `config` with no args — show a one-line locator so users can
    /// edit the file by hand for the keys this builtin doesn't cover.
    Locate,
}

/// Parse args (without the leading `config` token). Returns a usage
/// error string when the form doesn't match.
pub fn parse(args: &[String]) -> Result<ConfigAction, String> {
    let sub = match args.first() {
        Some(s) => s.as_str(),
        None => return Ok(ConfigAction::Locate),
    };
    match sub {
        "get" => {
            let key = args
                .get(1)
                .cloned()
                .ok_or_else(|| "usage: orkia config get <key>".to_string())?;
            Ok(ConfigAction::Get { key })
        }
        "set" => {
            let key = args
                .get(1)
                .cloned()
                .ok_or_else(|| "usage: orkia config set <key> <value>".to_string())?;
            let value = args
                .get(2)
                .cloned()
                .ok_or_else(|| "usage: orkia config set <key> <value>".to_string())?;
            Ok(ConfigAction::Set { key, value })
        }
        other => Err(format!("unknown config subcommand: {other}")),
    }
}

/// Outcome of a `config set default_scope` operation. The REPL uses
/// this to (a) render the blocks and (b) emit the
/// `workspace.scope_default_changed` SEAL/journal event with both the
/// previous and current values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultScopeUpdate {
    pub previous: Option<Scope>,
    pub current: Scope,
}

/// Dispatch a parsed [`ConfigAction`]. Returns `(blocks, optional update)`.
/// When `set default_scope` succeeds, the second element carries the
/// before/after value so the caller can wire up the SEAL/journal emit.
pub fn dispatch(
    data_dir: &Path,
    action: ConfigAction,
) -> (Vec<BlockContent>, Option<DefaultScopeUpdate>) {
    match action {
        ConfigAction::Locate => (
            vec![BlockContent::SystemInfo(format!(
                "config file: {}",
                config_path(data_dir).display()
            ))],
            None,
        ),
        ConfigAction::Get { key } if key == "default_scope" => {
            let current = read_default_scope(data_dir);
            let text = match current {
                Some(s) => format!("default_scope = {}", s.as_str()),
                None => "default_scope = (unset; inherits Private)".into(),
            };
            (vec![BlockContent::Text(text)], None)
        }
        ConfigAction::Get { key } => (
            vec![BlockContent::Error(format!("unknown config key: {key}"))],
            None,
        ),
        ConfigAction::Set { key, value } if key == "default_scope" => {
            let parsed = match Scope::parse(&value) {
                Ok(s) => s,
                Err(e) => return (vec![BlockContent::Error(e.to_string())], None),
            };
            let previous = read_default_scope(data_dir);
            match write_default_scope(data_dir, parsed) {
                Ok(()) => (
                    vec![BlockContent::SystemInfo(format!(
                        "\u{2713} default_scope = {}",
                        parsed.as_str()
                    ))],
                    Some(DefaultScopeUpdate {
                        previous,
                        current: parsed,
                    }),
                ),
                Err(e) => (
                    vec![BlockContent::Error(format!(
                        "failed to write config.toml: {e}"
                    ))],
                    None,
                ),
            }
        }
        ConfigAction::Set { key, .. } => (
            vec![BlockContent::Error(format!(
                "config: unknown key '{key}' (only 'default_scope' is supported in V1)"
            ))],
            None,
        ),
    }
}

// Legacy entry point kept for callers that pass raw args without
// going through `parse` / `dispatch`. Maps to a `Locate` action so
// callers see at least the file path. Deprecated for new code.
pub fn config(data_dir: &Path, _args: &[String]) -> Vec<BlockContent> {
    let (blocks, _) = dispatch(data_dir, ConfigAction::Locate);
    blocks
}

// ─── internals ────────────────────────────────────────────────────────────

fn config_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("config.toml")
}

/// Read the workspace's `default_scope` from `<data_dir>/config.toml`.
/// Returns `None` if unset or unparseable. Public so other crates can
/// resolve the workspace scope when validating overrides.
pub fn read_default_scope(data_dir: &Path) -> Option<Scope> {
    let path = config_path(data_dir);
    let body = std::fs::read_to_string(path).ok()?;
    for line in body.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("default_scope") {
            let rest = rest.trim();
            let rest = rest.strip_prefix('=')?.trim();
            let value = rest.trim_matches('"').trim_matches('\'');
            return Scope::parse(value).ok();
        }
    }
    None
}

/// Upsert `default_scope = "<value>"` in `<data_dir>/config.toml`.
/// The config file is a flat TOML table (no `[section]` headers) so
/// the rewrite is line-based: replace the existing line if present,
/// append a new line if not. Creates the file when missing.
fn write_default_scope(data_dir: &Path, scope: Scope) -> std::io::Result<()> {
    let path = config_path(data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let target = format!("default_scope = \"{}\"", scope.as_str());
    let mut out = String::with_capacity(existing.len() + target.len() + 1);
    let mut replaced = false;
    for line in existing.lines() {
        let trimmed = line.trim_start();
        if !replaced
            && trimmed.starts_with("default_scope")
            && trimmed["default_scope".len()..]
                .trim_start()
                .starts_with('=')
        {
            out.push_str(&target);
            out.push('\n');
            replaced = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !replaced {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&target);
        out.push('\n');
    }
    std::fs::write(&path, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn parse_set_default_scope() {
        let got = parse(&s(&["set", "default_scope", "team"])).unwrap();
        assert_eq!(
            got,
            ConfigAction::Set {
                key: "default_scope".into(),
                value: "team".into()
            }
        );
    }

    #[test]
    fn parse_get_default_scope() {
        let got = parse(&s(&["get", "default_scope"])).unwrap();
        assert_eq!(
            got,
            ConfigAction::Get {
                key: "default_scope".into()
            }
        );
    }

    #[test]
    fn parse_no_args_locates() {
        assert_eq!(parse(&[]).unwrap(), ConfigAction::Locate);
    }

    #[test]
    fn parse_rejects_unknown_subcommand() {
        assert!(parse(&s(&["frobnicate"])).is_err());
    }

    #[test]
    fn dispatch_set_writes_file_and_returns_update() {
        let dir = tempdir().unwrap();
        let (blocks, update) = dispatch(
            dir.path(),
            ConfigAction::Set {
                key: "default_scope".into(),
                value: "public".into(),
            },
        );
        assert!(!blocks.is_empty());
        let upd = update.expect("update");
        assert_eq!(upd.previous, None);
        assert_eq!(upd.current, Scope::Public);

        let body = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(body.contains("default_scope = \"public\""), "{body}");
    }

    #[test]
    fn dispatch_set_replaces_existing_line() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "default_project = \"x\"\ndefault_scope = \"private\"\n",
        )
        .unwrap();
        let (_, update) = dispatch(
            dir.path(),
            ConfigAction::Set {
                key: "default_scope".into(),
                value: "team".into(),
            },
        );
        let upd = update.unwrap();
        assert_eq!(upd.previous, Some(Scope::Private));
        assert_eq!(upd.current, Scope::Team);
        let body = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(body.contains("default_scope = \"team\""), "{body}");
        assert!(body.contains("default_project = \"x\""), "{body}");
        assert_eq!(body.matches("default_scope").count(), 1, "{body}");
    }

    #[test]
    fn dispatch_get_unset_renders_inherit() {
        let dir = tempdir().unwrap();
        let (blocks, update) = dispatch(
            dir.path(),
            ConfigAction::Get {
                key: "default_scope".into(),
            },
        );
        assert!(update.is_none());
        match &blocks[0] {
            BlockContent::Text(s) => assert!(s.contains("unset"), "{s}"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn dispatch_rejects_unknown_key() {
        let dir = tempdir().unwrap();
        let (blocks, _) = dispatch(
            dir.path(),
            ConfigAction::Set {
                key: "wallpaper".into(),
                value: "blue".into(),
            },
        );
        match &blocks[0] {
            BlockContent::Error(s) => assert!(s.contains("unknown key"), "{s}"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn dispatch_rejects_invalid_scope_value() {
        let dir = tempdir().unwrap();
        let (blocks, update) = dispatch(
            dir.path(),
            ConfigAction::Set {
                key: "default_scope".into(),
                value: "internal".into(),
            },
        );
        assert!(update.is_none());
        match &blocks[0] {
            BlockContent::Error(_) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }
}
