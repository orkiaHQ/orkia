// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Spawn-time context assembly for filesystem agents.
//!
//! `memory.md`, `tools.toml` from the agent's directory, layers on the
//! assigned projects' active RFCs and open issues, then truncates the
//! result to `max_context_tokens`. The bundle is what the shell injects
//! into the child PTY at spawn.

use std::collections::BTreeMap;
use std::path::Path;

use orkia_shell_types::{
    AgentDefinition, AgentToolEntry, AgentToolsFile, IssueSummary, McpServerEntry, RfcSummary,
    Scope, Workspace, resolve_effective_scope,
};
use sha2::{Digest, Sha256};

use crate::agent_dir;

/// Snapshot of the auth/team facts the scope filter needs.
///
/// Built once per agent spawn from the REPL's live state (the auth
/// provider, the team cache, and the workspace default scope), then
/// passed into [`AgentContext::load_with_filter`]. The filter itself
/// has no awareness of REPL types.
#[derive(Debug, Clone, Default)]
pub struct ScopeFilterContext {
    /// True when the user has at least one team membership cached.
    pub has_team_membership: bool,
    /// True when the user holds a valid auth token.
    pub is_authenticated: bool,
    /// Workspace-level `default_scope` from `~/.orkia/config.toml`.
    pub workspace_default: Option<Scope>,
}

impl ScopeFilterContext {
    /// Permissive default — equivalent to the PR1b no-op filter. Used
    /// by callers that don't have auth/team state handy (tests, some
    /// internal call sites). Production REPL builds a real one.
    pub fn permissive() -> Self {
        Self {
            has_team_membership: true,
            is_authenticated: true,
            workspace_default: None,
        }
    }
}

/// Assembled context payload + the discrete inputs used to build it.
/// The spawn site writes `assembled` to the per-job run dir, hashes the
/// inputs for the SEAL record, and turns the MCP server list into
/// `mcp-config.json` for Claude Code.
#[derive(Debug, Clone)]
pub struct AgentContext {
    pub name: String,
    pub assembled: String,
    pub system_prompt: String,
    pub memory: String,
    pub tools: AgentToolsFile,
    pub knowledge_mcp_bridge: bool,
}

impl AgentContext {
    /// Backwards-compatible entry point — uses the permissive filter
    /// (matches PR1b behaviour). Most callers should use
    /// [`Self::load_with_filter`] and pass a real
    /// [`ScopeFilterContext`].
    pub fn load(def: &AgentDefinition, workspace: &Workspace) -> Self {
        Self::load_with_filter(def, workspace, &ScopeFilterContext::permissive())
    }

    pub fn load_with_filter(
        def: &AgentDefinition,
        workspace: &Workspace,
        filter: &ScopeFilterContext,
    ) -> Self {
        let system_prompt = agent_dir::read_optional_file(&def.system_prompt_path());
        let memory = agent_dir::read_optional_file(&def.memory_path());
        let tools = agent_dir::load_tools(&def.tools_path());

        let mut rfcs: Vec<RfcContent> = Vec::new();
        let mut issues: Vec<IssueLine> = Vec::new();

        if def.include_rfcs || def.include_issues {
            for project_name in &def.assigned_projects {
                let Some(project) = workspace.project(project_name) else {
                    continue;
                };
                if def.include_rfcs {
                    for rfc in &project.rfcs {
                        if rfc.status != "active" {
                            continue;
                        }
                        if !scope_filter_allows_rfc(filter, project.scope, rfc) {
                            continue;
                        }
                        let body = std::fs::read_to_string(&rfc.path).unwrap_or_default();
                        rfcs.push(RfcContent {
                            title: rfc.title.clone(),
                            body,
                        });
                    }
                }
                if def.include_issues {
                    for issue in &project.issues {
                        if issue.status == "done" {
                            continue;
                        }
                        if !scope_filter_allows_issue(filter, project.scope, issue) {
                            continue;
                        }
                        issues.push(IssueLine {
                            status: issue.status.clone(),
                            priority: issue.priority.clone(),
                            title: issue.title.clone(),
                        });
                    }
                }
            }
        }

        let raw = assemble(&system_prompt, &memory, &rfcs, &issues);
        let assembled = truncate_to_tokens(&raw, def.max_context_tokens);

        Self {
            name: def.name.clone(),
            assembled,
            system_prompt,
            memory,
            tools,
            knowledge_mcp_bridge: false,
        }
    }

    /// SHA-256 (first 16 hex) of the system prompt — used in SEAL spawn records.
    pub fn system_prompt_hash(&self) -> String {
        short_sha(self.system_prompt.as_bytes())
    }

    /// SHA-256 (first 16 hex) of the memory blob.
    pub fn memory_hash(&self) -> String {
        short_sha(self.memory.as_bytes())
    }

    pub fn tools_count(&self) -> usize {
        self.tools.mcp.len() + self.tools.tool.len() + usize::from(self.knowledge_mcp_bridge)
    }
}

pub fn knowledge_bridge_entry(socket_path: &Path, job_id: u32) -> McpServerEntry {
    let mut env = BTreeMap::new();
    env.insert(
        "ORKIA_SOCKET_PATH".into(),
        socket_path.to_string_lossy().into_owned(),
    );
    env.insert("ORKIA_JOB_ID".into(), job_id.to_string());
    McpServerEntry {
        name: "orkia-knowledge".into(),
        url: "stdio://orkia mcp-bridge".into(),
        args: Vec::new(),
        env,
        description: Some("Orkia premium knowledge graph tools".into()),
    }
}

/// Scope-based filter hook for context injection. PR2 implementation.
///
/// Resolves the effective scope of the RFC by walking the inheritance
/// chain workspace → project → rfc (per `resolve_effective_scope`)
///
/// * `Private` — always visible to local agents (the user's own files).
/// * `Team` — visible only if the user holds at least one team
///   membership; otherwise the artifact is declarative and stays out
///   of the agent's prompt until membership is acquired.
/// * `Public` — visible to authenticated users. The filter is for
///   *what the agent sees*, not what the user sees on disk.
fn scope_filter_allows_rfc(
    filter: &ScopeFilterContext,
    project_scope: Option<Scope>,
    rfc: &RfcSummary,
) -> bool {
    let effective =
        resolve_effective_scope(filter.workspace_default, project_scope, rfc.scope, None);
    decide(filter, effective)
}

fn scope_filter_allows_issue(
    filter: &ScopeFilterContext,
    project_scope: Option<Scope>,
    issue: &IssueSummary,
) -> bool {
    let effective =
        resolve_effective_scope(filter.workspace_default, project_scope, None, issue.scope);
    decide(filter, effective)
}

fn decide(filter: &ScopeFilterContext, effective: Scope) -> bool {
    match effective {
        Scope::Private => true,
        Scope::Team => filter.has_team_membership,
        Scope::Public => filter.is_authenticated,
    }
}

#[derive(Debug, Clone)]
struct RfcContent {
    title: String,
    body: String,
}

#[derive(Debug, Clone)]
struct IssueLine {
    status: String,
    priority: String,
    title: String,
}

fn assemble(prompt: &str, memory: &str, rfcs: &[RfcContent], issues: &[IssueLine]) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !prompt.is_empty() {
        parts.push(prompt.to_string());
    }
    if !memory.trim().is_empty() {
        parts.push(format!("\n---\n# Memory\n{memory}"));
    }
    if !rfcs.is_empty() {
        parts.push("\n---\n# Active RFCs".into());
        for rfc in rfcs {
            parts.push(format!("## {}\n{}", rfc.title, rfc.body));
        }
    }
    if !issues.is_empty() {
        parts.push("\n---\n# Open Issues".into());
        for issue in issues {
            parts.push(format!(
                "- [{}] {} ({})",
                issue.status, issue.title, issue.priority
            ));
        }
    }
    parts.join("\n")
}

/// Crude byte-budget truncation. Tokens are approximated as 4 chars
/// conservative — small local models get squashed to fit.
pub fn truncate_to_tokens(text: &str, max_tokens: usize) -> String {
    if max_tokens == 0 {
        return text.to_string();
    }
    let budget = max_tokens.saturating_mul(4);
    if text.len() <= budget {
        return text.to_string();
    }
    let mut end = budget;
    while !text.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    let mut truncated = text[..end].to_string();
    truncated.push_str("\n\n[…truncated to fit max_context_tokens]\n");
    truncated
}

fn short_sha(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let full = hex::encode(hasher.finalize());
    full.chars().take(16).collect()
}

/// The MCP server map from a tools manifest, in the canonical
/// `mcpServers` JSON shape — shared verbatim by Claude's `--mcp-config`
/// file, Gemini's project-scope settings, and the Codex `-c` override
/// renderer. Returns `None` when no MCP servers are configured.
pub fn mcp_servers_map(
    tools: &AgentToolsFile,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    if tools.mcp.is_empty() {
        return None;
    }
    let mut servers = serde_json::Map::new();
    for server in &tools.mcp {
        servers.insert(server.name.clone(), mcp_entry_json(server));
    }
    Some(servers)
}

/// Render the MCP server list from a tools manifest into the JSON shape
/// Claude Code accepts via `--mcp-config`. Returns `None` when no MCP
/// servers are configured.
pub fn render_mcp_config(tools: &AgentToolsFile) -> Option<String> {
    let servers = mcp_servers_map(tools)?;
    let payload = serde_json::json!({ "mcpServers": servers });
    serde_json::to_string_pretty(&payload).ok()
}

fn mcp_entry_json(server: &McpServerEntry) -> serde_json::Value {
    let mut value = if let Some(rest) = server.url.strip_prefix("stdio://") {
        let mut parts = rest.split_whitespace();
        let command = parts.next().unwrap_or("").to_string();
        let mut args: Vec<String> = parts.map(String::from).collect();
        args.extend(server.args.iter().cloned());
        serde_json::json!({
            "command": command,
            "args": args,
        })
    } else {
        let mut payload = serde_json::Map::new();
        payload.insert("url".into(), serde_json::Value::String(server.url.clone()));
        if let Some(desc) = &server.description {
            payload.insert(
                "description".into(),
                serde_json::Value::String(desc.clone()),
            );
        }
        serde_json::Value::Object(payload)
    };
    if !server.env.is_empty()
        && let Some(obj) = value.as_object_mut()
    {
        obj.insert("env".into(), serde_json::json!(server.env));
    }
    value
}

/// Render the optional `[[tool]]` entries — currently only surfaced as
/// a markdown footer the agent can read via `ORKIA_AGENT_CONTEXT`.
pub fn render_tool_section(tools: &[AgentToolEntry]) -> String {
    let mut out = String::from("\n---\n# Tools\n");
    for t in tools {
        let desc = t.description.as_deref().unwrap_or("");
        out.push_str(&format!("- `{}` — `{}` ({desc})\n", t.name, t.command));
    }
    out
}

/// Compute a sensible filename inside the per-job run dir.
pub fn context_filename() -> &'static str {
    "context.md"
}
pub fn mcp_config_filename() -> &'static str {
    "mcp-config.json"
}

/// The written `mcp-config.json`: its path plus the parsed `mcpServers`
/// map, so spawn planners can render provider-specific delivery (Codex
/// `-c` overrides, Gemini settings merge) without re-reading the file.
pub struct McpConfigArtifact {
    pub path: std::path::PathBuf,
    pub servers: serde_json::Map<String, serde_json::Value>,
}

/// Persist the assembled context (and optional MCP config) into the
/// per-job run directory. Returns the context path and the written MCP
/// config artifact, when any. Errors writing the MCP file are surfaced
/// via tracing rather than aborting the spawn — the context file is the
/// critical artifact.
pub fn write_to_run_dir(
    run_dir: &Path,
    context: &AgentContext,
    socket_path: &Path,
    job_id: u32,
) -> std::io::Result<(std::path::PathBuf, Option<McpConfigArtifact>)> {
    let context_path = run_dir.join(context_filename());
    let mut payload = context.assembled.clone();
    if !context.tools.tool.is_empty() {
        payload.push_str(&render_tool_section(&context.tools.tool));
    }
    std::fs::write(&context_path, &payload)?;
    let mut tools = context.tools.clone();
    if context.knowledge_mcp_bridge {
        tools.mcp.push(knowledge_bridge_entry(socket_path, job_id));
    }
    let mcp = mcp_servers_map(&tools).and_then(|servers| {
        let path = run_dir.join(mcp_config_filename());
        let payload = serde_json::json!({ "mcpServers": servers.clone() });
        let json = serde_json::to_string_pretty(&payload).ok()?;
        if let Err(e) = std::fs::write(&path, json) {
            tracing::warn!(error = %e, "failed to write mcp-config.json");
            None
        } else {
            Some(McpConfigArtifact { path, servers })
        }
    });
    Ok((context_path, mcp))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn dummy_rfc(scope: Option<Scope>) -> RfcSummary {
        RfcSummary {
            slug: "x".into(),
            title: "X".into(),
            status: "active".into(),
            assigned: Vec::new(),
            path: PathBuf::from("/tmp/x.md"),
            scope,
        }
    }

    fn dummy_issue(scope: Option<Scope>) -> IssueSummary {
        IssueSummary {
            number: 1,
            slug: "x".into(),
            title: "X".into(),
            status: "todo".into(),
            priority: "low".into(),
            assigned: None,
            path: PathBuf::from("/tmp/x.toml"),
            scope,
        }
    }

    #[test]
    fn private_always_visible() {
        let filter = ScopeFilterContext {
            has_team_membership: false,
            is_authenticated: false,
            workspace_default: None,
        };
        assert!(scope_filter_allows_rfc(
            &filter,
            None,
            &dummy_rfc(Some(Scope::Private))
        ));
        assert!(scope_filter_allows_issue(
            &filter,
            None,
            &dummy_issue(Some(Scope::Private))
        ));
        // No declaration anywhere → default Private.
        assert!(scope_filter_allows_rfc(&filter, None, &dummy_rfc(None)));
    }

    #[test]
    fn team_requires_membership() {
        let no_team = ScopeFilterContext {
            has_team_membership: false,
            is_authenticated: true,
            workspace_default: None,
        };
        assert!(!scope_filter_allows_rfc(
            &no_team,
            None,
            &dummy_rfc(Some(Scope::Team))
        ));
        assert!(!scope_filter_allows_issue(
            &no_team,
            None,
            &dummy_issue(Some(Scope::Team))
        ));

        let with_team = ScopeFilterContext {
            has_team_membership: true,
            ..no_team
        };
        assert!(scope_filter_allows_rfc(
            &with_team,
            None,
            &dummy_rfc(Some(Scope::Team))
        ));
    }

    #[test]
    fn public_requires_auth() {
        let unauth = ScopeFilterContext {
            has_team_membership: true,
            is_authenticated: false,
            workspace_default: None,
        };
        assert!(!scope_filter_allows_rfc(
            &unauth,
            None,
            &dummy_rfc(Some(Scope::Public))
        ));

        let authed = ScopeFilterContext {
            is_authenticated: true,
            ..unauth
        };
        assert!(scope_filter_allows_rfc(
            &authed,
            None,
            &dummy_rfc(Some(Scope::Public))
        ));
    }

    #[test]
    fn workspace_default_applies_when_artifact_and_project_silent() {
        let filter = ScopeFilterContext {
            has_team_membership: false,
            is_authenticated: false,
            workspace_default: Some(Scope::Team),
        };
        // No project or rfc scope → workspace default = Team → blocked w/o membership.
        assert!(!scope_filter_allows_rfc(&filter, None, &dummy_rfc(None)));
    }

    #[test]
    fn project_scope_overrides_workspace_default() {
        let filter = ScopeFilterContext {
            has_team_membership: false,
            is_authenticated: false,
            workspace_default: Some(Scope::Team),
        };
        // Project says Private → Private wins → visible.
        assert!(scope_filter_allows_rfc(
            &filter,
            Some(Scope::Private),
            &dummy_rfc(None)
        ));
    }

    #[test]
    fn assemble_includes_all_sections() {
        let rfcs = vec![RfcContent {
            title: "Auth".into(),
            body: "body".into(),
        }];
        let issues = vec![IssueLine {
            status: "todo".into(),
            priority: "high".into(),
            title: "Fix CORS".into(),
        }];
        let out = assemble("# Prompt", "- one", &rfcs, &issues);
        assert!(out.contains("# Prompt"));
        assert!(out.contains("# Memory"));
        assert!(out.contains("# Active RFCs"));
        assert!(out.contains("## Auth"));
        assert!(out.contains("# Open Issues"));
        assert!(out.contains("[todo] Fix CORS (high)"));
    }

    #[test]
    fn assemble_omits_empty_sections() {
        let out = assemble("prompt", "", &[], &[]);
        assert_eq!(out, "prompt");
    }

    #[test]
    fn truncate_respects_budget() {
        let text = "a".repeat(10_000);
        let out = truncate_to_tokens(&text, 100); // 100 * 4 = 400 budget
        assert!(out.len() < 1_000);
        assert!(out.ends_with("[…truncated to fit max_context_tokens]\n"));
    }

    #[test]
    fn truncate_passthrough_when_short() {
        let out = truncate_to_tokens("hello", 100);
        assert_eq!(out, "hello");
    }

    #[test]
    fn render_mcp_config_handles_http_and_stdio() {
        let tools = AgentToolsFile {
            mcp: vec![
                McpServerEntry {
                    name: "github".into(),
                    url: "https://mcp.github.com/sse".into(),
                    args: vec![],
                    env: BTreeMap::new(),
                    description: Some("GitHub".into()),
                },
                McpServerEntry {
                    name: "fs".into(),
                    url: "stdio://npx @anthropic/mcp-filesystem".into(),
                    args: vec!["/tmp".into()],
                    env: BTreeMap::new(),
                    description: None,
                },
            ],
            tool: vec![],
        };
        let rendered = render_mcp_config(&tools).unwrap();
        assert!(rendered.contains("\"mcpServers\""));
        assert!(rendered.contains("\"github\""));
        assert!(rendered.contains("\"url\""));
        assert!(rendered.contains("\"fs\""));
        assert!(rendered.contains("\"command\""));
        assert!(rendered.contains("/tmp"));
    }

    #[test]
    fn render_mcp_config_includes_stdio_env() {
        let mut tools = AgentToolsFile::default();
        tools
            .mcp
            .push(knowledge_bridge_entry(Path::new("/tmp/orkia.sock"), 42));
        let rendered = render_mcp_config(&tools).unwrap();
        assert!(rendered.contains("\"orkia-knowledge\""));
        assert!(rendered.contains("\"ORKIA_SOCKET_PATH\""));
        assert!(rendered.contains("/tmp/orkia.sock"));
        assert!(rendered.contains("\"ORKIA_JOB_ID\""));
        assert!(rendered.contains("\"42\""));
    }

    #[test]
    fn render_mcp_config_returns_none_when_empty() {
        let tools = AgentToolsFile::default();
        assert!(render_mcp_config(&tools).is_none());
    }
}
