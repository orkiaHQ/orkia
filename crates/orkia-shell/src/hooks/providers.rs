// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Hook config generators for each supported agent provider.
//!
//! Each generator returns the JSON value for the `hooks` key in the
//! provider's settings file. `merge_hooks_config` writes that value
//! into the file at the given path, preserving every other top-level
//! key already present.

use std::path::{Path, PathBuf};

use orkia_shell_types::ProviderId;

use crate::error::ShellError;

/// Build the hooks JSON for Claude Code, pointing each event at
/// `orkia bridge --source claude` (the journal/SEAL recorder, which always
/// exits 0 and cannot gate). Hooks covered: SessionStart, PreToolUse,
/// PostToolUse, PermissionRequest, Stop, Notification, UserPromptSubmit.
///
/// `PreToolUse` event ALSO runs the mediation hook `orkia-sh hook`, which
/// returns the PreToolUse `permissionDecision` that actually denies a
/// disallowed Bash command on macOS and emits the `cage.verdict`. Off the cage
/// `mediate` is false and the bridge stands alone — a config with no `[cage]`
/// block behaves exactly as before.
pub fn claude_hooks_config(mediate: bool) -> serde_json::Value {
    let bridge = bridge_command("claude");
    let cmd = serde_json::json!([{"type": "command", "command": bridge.as_str()}]);
    // When caged, PreToolUse also runs the mediation hook (the deny gate + the
    // `cage.verdict`), and PostToolUse runs it too (emits `command.outcome` — the
    // result-quality trust signal; the macOS counterpart to Linux's fork+wait).
    let mediated = serde_json::json!([
        {"type": "command", "command": bridge.as_str()},
        {"type": "command", "command": mediation_hook_command()},
    ]);
    let (pretool, posttool) = if mediate {
        (mediated.clone(), mediated)
    } else {
        (cmd.clone(), cmd.clone())
    };
    // Claude's settings schema requires each event to map to an array of
    // matcher groups — `[{ "matcher": <str>, "hooks": [{type, command}, …] }]`
    // — not a bare `[{type, command}]` array. An empty matcher matches all
    // tools. Older Claude tolerated the flat form; current Claude rejects it
    // ("Expected array, but received undefined" at `<event>.0.hooks`) and the
    // agent derails on boot trying to repair settings.json instead of running.
    let group = |hooks: serde_json::Value| serde_json::json!([{"matcher": "", "hooks": hooks}]);
    serde_json::json!({
        "SessionStart": group(cmd.clone()),
        "PreToolUse": group(pretool),
        "PostToolUse": group(posttool),
        "PermissionRequest": group(cmd.clone()),
        "Stop": group(cmd.clone()),
        "Notification": group(cmd.clone()),
        "UserPromptSubmit": group(cmd),
    })
}

/// Resolve the `orkia bridge --source <provider>` command line written into an
/// agent's hook config. Uses the ABSOLUTE path of the currently running `orkia`
/// executable (`bridge` is a subcommand of `orkia` itself) so the hook always
/// invokes the SAME binary that spawned the agent — never "whatever `orkia`
/// happens to be on the agent's `PATH`," which can be a stale or version-skewed
/// install. Without this, a detached runtime's per-job hook routing (keyed on
/// the PATH `orkia` predates the socket-override support: the old shim ignores
/// the env and posts to the global hub, the runtime's state machine never sees
/// `Stop`, and a one-shot detached agent idles forever instead of tearing down.
/// Falls back to a bare `orkia` (PATH-resolved) only when `current_exe` is
/// unavailable. Mirrors [`mediation_hook_command`]'s exe-relative resolution.
/// `--scope job` marks this entry as the sole forwarder for the
/// session: the bridge self-suppresses any invocation WITHOUT the flag
/// when `ORKIA_SOCKET_PATH` is set (a user-level global settings hook
/// firing on an orkia-managed session would otherwise duplicate every
/// journal/SEAL record and race the final-response ledger).
fn bridge_command(source: &str) -> String {
    if let Ok(exe) = std::env::current_exe() {
        return format!("{} bridge --source {source} --scope job", exe.display());
    }
    format!("orkia bridge --source {source} --scope job")
}

/// Resolve the `orkia-sh hook` command line for the PreToolUse mediation entry.
/// Prefers the `orkia-sh` binary installed next to the running `orkia` exe
/// (covers `cargo run` and installed layouts); falls back to a bare `orkia-sh`
/// resolved via the agent's `PATH` (the cage prepends the shim's dir on macOS).
fn mediation_hook_command() -> String {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join("orkia-sh");
        if candidate.is_file() {
            return format!("{} hook", candidate.display());
        }
    }
    "orkia-sh hook".to_string()
}

/// Build the hooks JSON for Codex.
///
/// Recorder-only: every entry points at `orkia bridge` (exits 0, cannot gate).
/// Unlike Claude there is no `mediate` variant — Codex's hook deny-semantics are
/// unverified, so V1 does not wire a deny-capable mediation hook for it. On
/// Linux Codex is gated by the sole-shell `-c` shim regardless (it reaches
/// `orkia-sh` by construction); on macOS the guarantee is the Seatbelt
/// exec-deny. (Real-capture for a deny-capable Codex hook is owed.)
pub fn codex_hooks_config() -> serde_json::Value {
    let bridge = bridge_command("codex");
    let cmd = |matcher: Option<&str>| {
        let mut group = serde_json::json!({
            "hooks": [{"type": "command", "command": bridge.as_str()}]
        });
        if let Some(matcher) = matcher {
            group["matcher"] = serde_json::Value::String(matcher.into());
        }
        group
    };
    serde_json::json!({
        "hooks": {
            "SessionStart": [cmd(Some("startup|resume"))],
            "PreToolUse": [cmd(Some("*"))],
            "PostToolUse": [cmd(Some("*"))],
            "PermissionRequest": [cmd(Some("*"))],
            "Stop": [cmd(None)],
            "UserPromptSubmit": [cmd(None)],
        }
    })
}

/// Build the hooks JSON for Gemini CLI. Gemini uses `BeforeTool` /
/// `AfterTool` / `SessionEnd` which the bridge normalizes back to
/// `PreToolUse` / `PostToolUse` / `Stop` via `normalize_event_name`.
///
/// Recorder-only, same posture as [`codex_hooks_config`]: bridge-only, no
/// deny-capable mediation entry in V1 (Gemini's `BeforeTool` deny-semantics are
/// unverified). Linux gating is the sole-shell shim; macOS is the Seatbelt
/// exec-deny. (Real-capture for a deny-capable Gemini hook is owed.)
pub fn gemini_hooks_config() -> serde_json::Value {
    let bridge = bridge_command("gemini");
    let cmd_matched =
        serde_json::json!([{"type": "command", "command": bridge.as_str(), "matcher": "*"}]);
    let cmd = serde_json::json!([{"type": "command", "command": bridge.as_str()}]);
    serde_json::json!({
        "BeforeTool": cmd_matched,
        "AfterTool": cmd_matched,
        "SessionStart": cmd,
        "SessionEnd": cmd,
        "Notification": cmd,
    })
}

/// Merge a `hooks` value into the JSON settings file at `path`,
/// preserving every other top-level key. Creates parent directories
/// and the file itself if missing.
///
/// The settings file is expected to be a JSON object at top level
/// (Claude / Gemini settings.json, Codex hooks.json contains an array
/// at top level — see `write_hooks_array`). On parse failure the
/// existing file is preserved and an error returned; callers should
/// surface it as a non-fatal warning.
pub fn merge_hooks_config(path: &Path, hooks: &serde_json::Value) -> Result<(), ShellError> {
    let mut root = load_settings_object(path)?;
    root.insert("hooks".into(), hooks.clone());
    write_settings_object(path, root)
}

/// Merge MCP server entries into the `mcpServers` key of the JSON
/// settings file at `path` (Gemini project-scope delivery — see
/// `_killix/P1.8-MCP-VENDOR-MECHANISMS.md`). Unlike the hooks key,
/// which orkia owns and replaces wholesale, the merge here is
/// per-entry: orkia's server names are inserted/overwritten with fresh
/// per-job values, foreign user-added entries survive.
pub fn merge_mcp_servers(
    path: &Path,
    servers: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), ShellError> {
    let mut root = load_settings_object(path)?;
    let existing = root
        .entry("mcpServers")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !existing.is_object() {
        *existing = serde_json::Value::Object(serde_json::Map::new());
    }
    if let Some(map) = existing.as_object_mut() {
        for (name, entry) in servers {
            map.insert(name.clone(), entry.clone());
        }
    }
    write_settings_object(path, root)
}

/// Load the JSON object at `path`, creating parent directories so the
/// subsequent write succeeds. Missing or empty file → empty object; a
/// non-object root or a parse failure refuses (the existing file is
/// preserved, the caller surfaces a warning).
fn load_settings_object(
    path: &Path,
) -> Result<serde_json::Map<String, serde_json::Value>, ShellError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ShellError::Other(format!("hooks: mkdir {parent:?}: {e}")))?;
    }
    if !path.exists() {
        return Ok(serde_json::Map::new());
    }
    // Cap the read at the hook-payload budget. The settings file
    // is owned by the user's agent (Claude / Codex / Gemini), which
    // is itself an untrusted process — without a cap a misbehaving
    // agent could fill the file and OOM the orkia config-merge
    // path. 256 KiB is well above any realistic settings.json.
    let text = read_bounded_to_string(
        path,
        orkia_shell_types::input_limits::HOOK_PAYLOAD_MAX_BYTES,
    )
    .map_err(|e| ShellError::Other(format!("hooks: read {path:?}: {e}")))?;
    if text.trim().is_empty() {
        return Ok(serde_json::Map::new());
    }
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(serde_json::Value::Object(m)) => Ok(m),
        Ok(_) => Err(ShellError::Other(format!(
            "hooks: {path:?} is not a JSON object; refusing to overwrite"
        ))),
        Err(e) => Err(ShellError::Other(format!(
            "hooks: parse {path:?}: {e}; refusing to overwrite"
        ))),
    }
}

fn write_settings_object(
    path: &Path,
    root: serde_json::Map<String, serde_json::Value>,
) -> Result<(), ShellError> {
    let body = serde_json::to_string_pretty(&serde_json::Value::Object(root))
        .map_err(|e| ShellError::Other(format!("hooks: serialize: {e}")))?;
    std::fs::write(path, body)
        .map_err(|e| ShellError::Other(format!("hooks: write {path:?}: {e}")))?;
    Ok(())
}

/// Write the Codex hooks config verbatim to `path`. Codex's hook file is
/// separate from config.toml, so there is no merge target — we replace the file.
/// Parent dirs are created if missing.
pub fn write_hooks_array(path: &Path, hooks: &serde_json::Value) -> Result<(), ShellError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ShellError::Other(format!("hooks: mkdir {parent:?}: {e}")))?;
    }
    let body = serde_json::to_string_pretty(hooks)
        .map_err(|e| ShellError::Other(format!("hooks: serialize: {e}")))?;
    std::fs::write(path, body)
        .map_err(|e| ShellError::Other(format!("hooks: write {path:?}: {e}")))?;
    Ok(())
}

/// Install the appropriate provider hooks in the given project root.
/// Returns the path written so the caller can log it. Providers with
/// no known hook format (`Kimi`, `Generic` — see
/// `RuntimeCapabilities::hooks_capture`) return `None` (no-op).
pub fn install_hooks(
    project_root: &Path,
    provider: ProviderId,
    mediate: bool,
) -> Result<Option<PathBuf>, ShellError> {
    match provider {
        ProviderId::Claude => {
            let path = project_root.join(".claude").join("settings.json");
            merge_hooks_config(&path, &claude_hooks_config(mediate))?;
            Ok(Some(path))
        }
        ProviderId::Codex => {
            let path = project_root.join(".codex").join("hooks.json");
            write_hooks_array(&path, &codex_hooks_config())?;
            Ok(Some(path))
        }
        ProviderId::Gemini => {
            let path = project_root.join(".gemini").join("settings.json");
            merge_hooks_config(&path, &gemini_hooks_config())?;
            Ok(Some(path))
        }
        // No known hook config format. The capability table agrees
        // (`hooks_capture: false`) — flipping it requires a real-agent
        // demos scenario AND a renderer here.
        ProviderId::Kimi | ProviderId::Generic => Ok(None),
    }
}

/// Read at most `cap` bytes from `path` into a UTF-8 string. Returns
/// `Err(io::Error { kind: InvalidData, ... })` if the file exceeds the
/// cap — partial JSON is meaningless to the merge path. Used at the
/// hooks-config trust boundary; the writer is the user's agent process.
fn read_bounded_to_string(path: &Path, cap: usize) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut buf = String::with_capacity(cap.min(8 * 1024));
    let n = file
        .by_ref()
        .take(cap as u64 + 1)
        .read_to_string(&mut buf)?;
    if n > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} exceeds {cap}-byte cap", path.display()),
        ));
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn claude_config_has_all_expected_hooks() {
        let cfg = claude_hooks_config(false);
        let obj = cfg.as_object().expect("object");
        for k in [
            "SessionStart",
            "PreToolUse",
            "PostToolUse",
            "PermissionRequest",
            "Stop",
            "Notification",
            "UserPromptSubmit",
        ] {
            assert!(obj.contains_key(k), "missing hook: {k}");
        }
        // Each event is an array of matcher groups; each group's `hooks`
        // array holds the {type, command} entries.
        let pretool = &obj["PreToolUse"];
        let groups = pretool.as_array().expect("array");
        assert_eq!(groups[0]["matcher"], "");
        let arr = groups[0]["hooks"].as_array().expect("hooks array");
        assert_eq!(arr[0]["type"], "command");
        // The bridge command is the absolute `current_exe()` path (so the hook
        // always invokes the spawning binary, never a PATH-resolved stale one),
        // hence assert the stable suffix rather than the full path.
        assert!(
            arr[0]["command"]
                .as_str()
                .expect("str")
                .ends_with("bridge --source claude --scope job")
        );
    }

    #[test]
    fn claude_config_no_mediate_pretooluse_is_bridge_only() {
        let cfg = claude_hooks_config(false);
        let arr = cfg["PreToolUse"][0]["hooks"].as_array().expect("array");
        // Off the cage: PreToolUse runs the bridge alone — unchanged behaviour.
        assert_eq!(arr.len(), 1);
        assert!(
            arr[0]["command"]
                .as_str()
                .expect("str")
                .ends_with("bridge --source claude --scope job")
        );
    }

    #[test]
    fn claude_config_mediate_adds_orkia_sh_hook_to_pre_and_post_tool_use() {
        let cfg = claude_hooks_config(true);
        // Caged: the bridge still records first, then the mediation hook runs.
        // PreToolUse gates (deny + cage.verdict); PostToolUse records command.outcome.
        for event in ["PreToolUse", "PostToolUse"] {
            let arr = cfg[event][0]["hooks"].as_array().expect("array");
            assert_eq!(arr.len(), 2, "{event}");
            assert!(
                arr[0]["command"]
                    .as_str()
                    .expect("str")
                    .ends_with("bridge --source claude --scope job")
            );
            let hook = arr[1]["command"].as_str().expect("str");
            assert!(hook.ends_with("orkia-sh hook"), "{event}: {hook}");
        }
        // Other events stay bridge-only so we don't double-fire the recorder.
        assert_eq!(cfg["Stop"][0]["hooks"].as_array().expect("array").len(), 1);
        assert_eq!(
            cfg["SessionStart"][0]["hooks"]
                .as_array()
                .expect("array")
                .len(),
            1
        );
    }

    #[test]
    fn codex_config_is_hooks_object() {
        let cfg = codex_hooks_config();
        let hooks = cfg["hooks"].as_object().expect("hooks object");
        assert!(hooks.contains_key("PreToolUse"));
        assert!(hooks.contains_key("Stop"));
        for groups in hooks.values() {
            for group in groups.as_array().expect("event groups") {
                for hook in group["hooks"].as_array().expect("hook commands") {
                    assert!(
                        hook["command"]
                            .as_str()
                            .expect("str")
                            .ends_with("bridge --source codex --scope job")
                    );
                }
            }
        }
    }

    #[test]
    fn gemini_config_uses_provider_event_names() {
        let cfg = gemini_hooks_config();
        let obj = cfg.as_object().expect("object");
        assert!(obj.contains_key("BeforeTool"));
        assert!(obj.contains_key("AfterTool"));
        assert!(obj.contains_key("SessionEnd"));
        assert!(
            obj["BeforeTool"][0]["command"]
                .as_str()
                .expect("str")
                .ends_with("bridge --source gemini --scope job")
        );
    }

    #[test]
    fn generated_bridge_commands_are_job_scoped() {
        // Every generated bridge entry must carry `--scope job` — it is
        // what lets the bridge suppress the user's GLOBAL settings hook
        // (same binary, no flag) on orkia-managed sessions instead of
        // duplicating every journal/SEAL record.
        let claude = claude_hooks_config(false).to_string();
        assert!(
            !claude.contains("--source claude\""),
            "claude entry missing --scope job"
        );
        assert!(claude.contains("--source claude --scope job"));
    }

    #[test]
    fn codex_gemini_configs_are_recorder_only_no_mediation_hook() {
        // V1 wires a deny-capable mediation hook (`orkia-sh hook`) for Claude
        // only. Codex/Gemini must stay bridge-only — every command they emit
        // points at `orkia bridge` and nothing references `orkia-sh`. This
        // locks the macOS posture: their per-command gate is NOT a hook (it is
        // the Seatbelt exec-deny / the Linux sole-shell shim).
        let codex = codex_hooks_config().to_string();
        assert!(!codex.contains("orkia-sh"), "codex grew a mediation hook");
        assert!(codex.contains("bridge --source codex"));

        let gemini = gemini_hooks_config().to_string();
        assert!(!gemini.contains("orkia-sh"), "gemini grew a mediation hook");
        assert!(gemini.contains("bridge --source gemini"));
    }

    #[test]
    fn merge_preserves_existing_keys() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        let pre = serde_json::json!({
            "permissions": {"allow": ["Read"]},
            "model": "claude-sonnet-4-6",
            "hooks": {"OldHook": [{"type": "command", "command": "old"}]},
        });
        std::fs::write(&path, serde_json::to_string_pretty(&pre).expect("ser")).expect("write");

        merge_hooks_config(&path, &claude_hooks_config(false)).expect("merge");

        let body = std::fs::read_to_string(&path).expect("read");
        let v: serde_json::Value = serde_json::from_str(&body).expect("parse");
        // Other keys preserved.
        assert_eq!(v["permissions"]["allow"][0], "Read");
        assert_eq!(v["model"], "claude-sonnet-4-6");
        // hooks replaced with the new value.
        assert!(v["hooks"].get("PreToolUse").is_some());
        assert!(v["hooks"].get("OldHook").is_none());
    }

    #[test]
    fn merge_creates_file_if_missing() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join(".claude").join("settings.json");
        merge_hooks_config(&path, &claude_hooks_config(false)).expect("merge");
        assert!(path.exists());
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("parse");
        assert!(v["hooks"].get("PreToolUse").is_some());
    }

    #[test]
    fn merge_refuses_non_object_root() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "[1,2,3]").expect("write");
        let err = merge_hooks_config(&path, &claude_hooks_config(false)).unwrap_err();
        match err {
            ShellError::Other(msg) => assert!(msg.contains("not a JSON object")),
            other => panic!("unexpected error: {other:?}"),
        }
        // Original content untouched.
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "[1,2,3]");
    }

    #[test]
    fn merge_handles_empty_file() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "").expect("write empty");
        merge_hooks_config(&path, &claude_hooks_config(false)).expect("merge");
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("parse");
        assert!(v["hooks"].get("PreToolUse").is_some());
    }

    #[test]
    fn install_hooks_writes_correct_path_per_provider() {
        let dir = tempdir().expect("tempdir");
        let p = install_hooks(dir.path(), ProviderId::Claude, false).expect("install claude");
        assert_eq!(p.unwrap(), dir.path().join(".claude").join("settings.json"));

        let p = install_hooks(dir.path(), ProviderId::Codex, false).expect("install codex");
        assert_eq!(p.unwrap(), dir.path().join(".codex").join("hooks.json"));

        let p = install_hooks(dir.path(), ProviderId::Gemini, false).expect("install gemini");
        assert_eq!(p.unwrap(), dir.path().join(".gemini").join("settings.json"));

        let p = install_hooks(dir.path(), ProviderId::Generic, false).expect("install generic");
        assert!(p.is_none());
    }

    #[test]
    fn merge_mcp_servers_preserves_foreign_entries_and_other_keys() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        let pre = serde_json::json!({
            "hooks": {"BeforeTool": []},
            "mcpServers": {
                "user-server": {"command": "their-tool"},
                "orkia-knowledge": {"command": "stale", "env": {"ORKIA_JOB_ID": "7"}},
            },
        });
        std::fs::write(&path, serde_json::to_string_pretty(&pre).expect("ser")).expect("write");

        let fresh = serde_json::json!({
            "orkia-knowledge": {"command": "orkia", "env": {"ORKIA_JOB_ID": "42"}},
        });
        let serde_json::Value::Object(servers) = fresh else {
            unreachable!()
        };
        merge_mcp_servers(&path, &servers).expect("merge");

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("parse");
        // The user's own server and the hooks key survive.
        assert_eq!(v["mcpServers"]["user-server"]["command"], "their-tool");
        assert!(v["hooks"].get("BeforeTool").is_some());
        // orkia's entry is overwritten with the fresh per-job value.
        assert_eq!(v["mcpServers"]["orkia-knowledge"]["command"], "orkia");
        assert_eq!(
            v["mcpServers"]["orkia-knowledge"]["env"]["ORKIA_JOB_ID"],
            "42"
        );
    }

    #[test]
    fn merge_mcp_servers_creates_file_and_replaces_non_object_key() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join(".gemini").join("settings.json");
        let fresh = serde_json::json!({"orkia-pipe": {"command": "orkia", "args": ["mcp-pipe"]}});
        let serde_json::Value::Object(servers) = fresh else {
            unreachable!()
        };
        merge_mcp_servers(&path, &servers).expect("merge creates file");
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("parse");
        assert_eq!(v["mcpServers"]["orkia-pipe"]["args"][0], "mcp-pipe");

        // A corrupt (non-object) mcpServers value is replaced, not merged into.
        std::fs::write(&path, r#"{"mcpServers": [1,2]}"#).expect("write corrupt");
        merge_mcp_servers(&path, &servers).expect("merge over corrupt key");
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("parse");
        assert_eq!(v["mcpServers"]["orkia-pipe"]["command"], "orkia");
    }

    #[test]
    fn install_hooks_kimi_is_noop() {
        // Kimi has no known hook format (`hooks_capture: false`): the
        // install must write NOTHING — not an empty config some kimi
        // version might choke on.
        let dir = tempdir().expect("tempdir");
        let p = install_hooks(dir.path(), ProviderId::Kimi, false).expect("install kimi");
        assert!(p.is_none());
        assert_eq!(
            std::fs::read_dir(dir.path()).expect("read dir").count(),
            0,
            "kimi install must leave the project root untouched"
        );
    }
}
