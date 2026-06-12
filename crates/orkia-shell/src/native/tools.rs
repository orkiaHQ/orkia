// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Tool registry + executor for the native runtime.
//!
//! V1 tools: `shell` (policy-gated command execution inside the
//! session task — never the REPL's BrushSession) and
//! `recall_knowledge` (in-process knowledge-graph read, offered only
//! when premium intelligence is active).
//!
//! Policy posture: the gate runs **before** execution. `Deny`
//! and `Ask` both return a structured tool error to the model and
//! never execute (`Ask` = deny in V1, decision `ask_denied_v1`). A
//! missing or corrupt policy.toml denies every `shell` call.
//! Tool names and inputs come from a remote LLM: unknown names
//! and malformed inputs are tool errors, never panics.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use orkia_shell_types::{NativeToolDef, Policy, PolicyDecision};
use serde_json::{Value, json};

pub(crate) const SHELL_TOOL: &str = "shell";
pub(crate) const RECALL_TOOL: &str = "recall_knowledge";

/// Cap on tool output returned to the model. Keeps a runaway `find /`
/// from blowing up the transcript (and the kernel relay's request).
const OUTPUT_CAP_BYTES: usize = 64 * 1024;

/// Hard ceiling for one `shell` invocation; the process is killed on
/// expiry so an unattended session can't hang on a stuck command.
const SHELL_TIMEOUT: Duration = Duration::from_secs(120);

pub struct ToolExecutor {
    /// `None` = policy.toml missing or unparseable → deny-by-default
    /// for `shell`. Loaded once at session spawn.
    policy: Option<Policy>,
    working_dir: Option<PathBuf>,
    /// Reasoning-store path; `Some` only when premium intelligence is
    /// active — `recall_knowledge` is not offered otherwise.
    knowledge_store: Option<PathBuf>,
}

/// Build a session's (or stage's) tool executor from the agent's
/// on-disk policy. Shared by the REPL dispatch path and the native
/// pipeline stage so both load the same policy.toml with the same
/// fail-closed posture: missing or unparseable ⇒ `None` ⇒ the
/// executor denies every `shell` call.
pub fn build_tool_executor(
    data_dir: &std::path::Path,
    agent: &str,
    working_dir: Option<PathBuf>,
    knowledge_bridge: bool,
) -> ToolExecutor {
    use orkia_shell_types::{PolicyContext, PolicyProvider};
    let policy_path = crate::agent_dir::agent_policy_path(data_dir, agent);
    let policy = match crate::toml_policy::TomlPolicyLoader::new(policy_path)
        .resolve(&PolicyContext::new(agent, "."))
    {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(
                agent,
                "native: policy unavailable ({e}); shell tool will deny everything",
            );
            None
        }
    };
    let knowledge_store = knowledge_bridge.then(|| crate::reasoning_builtins::store_path(data_dir));
    ToolExecutor::new(policy, working_dir, knowledge_store)
}

/// One executed (or refused) tool call.
pub struct ToolOutcome {
    pub content: String,
    pub is_error: bool,
    /// Present when the policy gate fired — drives the `cage.verdict`
    /// event naming the rule.
    pub verdict: Option<VerdictNote>,
    /// Knowledge-graph node ids served on the read path; the caller
    /// journals the `KnowledgeAccess` envelope (the executor never
    /// writes accounting).
    pub accessed_node_ids: Vec<String>,
}

impl ToolOutcome {
    fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            verdict: None,
            accessed_node_ids: Vec::new(),
        }
    }
}

pub struct VerdictNote {
    /// `"allow"`, `"deny"`, or `"ask_denied_v1"`.
    pub decision: &'static str,
    pub capability: Option<String>,
    pub rule: Option<String>,
    pub command: String,
}

impl ToolExecutor {
    pub fn new(
        policy: Option<Policy>,
        working_dir: Option<PathBuf>,
        knowledge_store: Option<PathBuf>,
    ) -> Self {
        Self {
            policy,
            working_dir,
            knowledge_store,
        }
    }

    /// The tool definitions offered to the model for every completion.
    pub fn defs(&self) -> Vec<NativeToolDef> {
        let mut defs = vec![NativeToolDef {
            name: SHELL_TOOL.into(),
            description: "Run a shell command in the session working directory and return \
                          its output. Commands are checked against the agent policy before \
                          execution; denied commands return an error naming the rule."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The command to run" }
                },
                "required": ["command"],
            }),
        }];
        if self.knowledge_store.is_some() {
            defs.push(NativeToolDef {
                name: RECALL_TOOL.into(),
                description: "Recall relevant facts and decisions from the orkia knowledge \
                              graph for a query."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "limit": { "type": "integer" }
                    },
                    "required": ["query"],
                }),
            });
        }
        defs
    }

    /// Execute one model-requested tool call. Never panics on hostile
    /// input; refusals and failures are tool errors the model
    /// sees and can react to.
    pub async fn execute(&self, name: &str, input: &Value) -> ToolOutcome {
        match name {
            SHELL_TOOL => self.execute_shell(input).await,
            RECALL_TOOL if self.knowledge_store.is_some() => self.execute_recall(input),
            other => ToolOutcome::error(format!("unknown tool: {other}")),
        }
    }

    async fn execute_shell(&self, input: &Value) -> ToolOutcome {
        let Some(command) = input.get("command").and_then(Value::as_str) else {
            return ToolOutcome::error("shell: missing required string field \"command\"");
        };
        match self.gate(command) {
            Gate::Allowed(note) => {
                let mut outcome = run_shell(command, self.working_dir.as_deref()).await;
                outcome.verdict = Some(note);
                outcome
            }
            Gate::Refused(note) => {
                let rule = note.rule.as_deref().unwrap_or("default").to_string();
                let mut outcome = ToolOutcome::error(format!(
                    "denied by policy (rule: {rule}) — the command was not executed"
                ));
                outcome.verdict = Some(note);
                outcome
            }
        }
    }

    /// Policy gate. `Ask` resolves as deny in V1 (no interactive
    /// approval path on a native session yet); a missing policy denies.
    fn gate(&self, command: &str) -> Gate {
        let Some(policy) = self.policy.as_ref() else {
            return Gate::Refused(VerdictNote {
                decision: "deny",
                capability: None,
                rule: Some("no-policy".into()),
                command: command.to_string(),
            });
        };
        match policy.evaluate_match(command) {
            PolicyDecision::Allow { capability, rule } => Gate::Allowed(VerdictNote {
                decision: "allow",
                capability: capability.map(str::to_string),
                rule: rule.map(str::to_string),
                command: command.to_string(),
            }),
            PolicyDecision::Deny { capability, rule } => Gate::Refused(VerdictNote {
                decision: "deny",
                capability: capability.map(str::to_string),
                rule: rule.map(str::to_string),
                command: command.to_string(),
            }),
            PolicyDecision::Ask(adj) => Gate::Refused(VerdictNote {
                decision: "ask_denied_v1",
                capability: adj.capability.map(str::to_string),
                rule: adj.rule.map(str::to_string),
                command: command.to_string(),
            }),
        }
    }

    fn execute_recall(&self, input: &Value) -> ToolOutcome {
        let Some(store_path) = self.knowledge_store.as_ref() else {
            return ToolOutcome::error("recall_knowledge is not available");
        };
        let store = match orkia_reasoning_store::ReasoningStore::open(store_path) {
            Ok(s) => s,
            Err(e) => return ToolOutcome::error(format!("knowledge store unavailable: {e}")),
        };
        let frame = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "recall",
            "params": input,
        })
        .to_string();
        let dispatched = orkia_knowledge_mcp::dispatch_request(&store, &frame);
        let accessed: Vec<String> = dispatched
            .accessed
            .iter()
            .map(|id| id.to_string())
            .collect();
        let value = match serde_json::to_value(&dispatched.response) {
            Ok(v) => v,
            Err(e) => return ToolOutcome::error(format!("recall: response encode: {e}")),
        };
        if let Some(err) = value.get("error") {
            return ToolOutcome::error(format!("recall: {err}"));
        }
        let content = value
            .get("result")
            .map(|r| r.to_string())
            .unwrap_or_else(|| "null".into());
        ToolOutcome {
            content: truncate(content),
            is_error: false,
            verdict: None,
            accessed_node_ids: accessed,
        }
    }
}

enum Gate {
    Allowed(VerdictNote),
    Refused(VerdictNote),
}

/// Run an allowed command under `/bin/sh -c` inside the session task.
/// Bounded by [`SHELL_TIMEOUT`] with kill-on-expiry; output capped at
/// [`OUTPUT_CAP_BYTES`].
async fn run_shell(command: &str, working_dir: Option<&std::path::Path>) -> ToolOutcome {
    let mut cmd = tokio::process::Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return ToolOutcome::error(format!("shell: spawn failed: {e}")),
    };
    let output = match tokio::time::timeout(SHELL_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return ToolOutcome::error(format!("shell: wait failed: {e}")),
        Err(_) => {
            // kill_on_drop reaps the child as the future is dropped.
            return ToolOutcome::error(format!(
                "shell: command timed out after {}s and was killed",
                SHELL_TIMEOUT.as_secs()
            ));
        }
    };
    let mut content = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str("[stderr]\n");
        content.push_str(stderr.trim_end());
    }
    let code = output.status.code().unwrap_or(-1);
    if !output.status.success() {
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str(&format!("(exit {code})"));
    }
    ToolOutcome {
        content: truncate(content),
        is_error: !output.status.success(),
        verdict: None,
        accessed_node_ids: Vec::new(),
    }
}

fn truncate(mut s: String) -> String {
    if s.len() > OUTPUT_CAP_BYTES {
        let mut cut = OUTPUT_CAP_BYTES;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push_str("\n… [output truncated]");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy_from(toml_text: &str) -> Policy {
        toml::from_str(toml_text).expect("test policy parses")
    }

    fn allowing_executor() -> ToolExecutor {
        let policy = policy_from(
            r#"
            default_verdict = "deny"

            [workspace]
            root = "."

            [[capabilities]]
            name = "read-only"
            matches = ["echo *", "true", "exit *"]
            verdict = "allow"

            [[capabilities]]
            name = "push"
            matches = ["git push*"]
            verdict = "ask"
            "#,
        );
        ToolExecutor::new(Some(policy), None, None)
    }

    #[tokio::test]
    async fn shell_allowed_command_runs() {
        let exec = allowing_executor();
        let out = exec
            .execute(SHELL_TOOL, &json!({"command": "echo hello"}))
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(out.content.trim(), "hello");
        assert_eq!(out.verdict.expect("verdict").decision, "allow");
    }

    #[tokio::test]
    async fn shell_denied_command_never_executes() {
        let exec = allowing_executor();
        let marker = "/tmp/orkia-native-denied-marker";
        let out = exec
            .execute(SHELL_TOOL, &json!({"command": format!("touch {marker}")}))
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("denied by policy"), "{}", out.content);
        assert_eq!(out.verdict.expect("verdict").decision, "deny");
        assert!(!std::path::Path::new(marker).exists());
    }

    #[tokio::test]
    async fn shell_ask_is_deny_in_v1() {
        let exec = allowing_executor();
        let out = exec
            .execute(SHELL_TOOL, &json!({"command": "git push origin main"}))
            .await;
        assert!(out.is_error);
        let v = out.verdict.expect("verdict");
        assert_eq!(v.decision, "ask_denied_v1");
        assert_eq!(v.rule.as_deref(), Some("git push*"));
    }

    #[tokio::test]
    async fn missing_policy_denies_shell() {
        let exec = ToolExecutor::new(None, None, None);
        let out = exec.execute(SHELL_TOOL, &json!({"command": "true"})).await;
        assert!(out.is_error);
        assert_eq!(out.verdict.expect("verdict").decision, "deny");
    }

    #[tokio::test]
    async fn malformed_input_is_a_tool_error_not_a_panic() {
        let exec = allowing_executor();
        // input is hostile model output.
        for bad in [json!({}), json!({"command": 42}), json!("ls")] {
            let out = exec.execute(SHELL_TOOL, &bad).await;
            assert!(out.is_error);
        }
        let out = exec.execute("rm_rf_everything", &json!({})).await;
        assert!(out.is_error);
        assert!(out.content.contains("unknown tool"));
    }

    #[tokio::test]
    async fn recall_absent_without_intelligence() {
        let exec = allowing_executor();
        assert_eq!(exec.defs().len(), 1);
        let out = exec.execute(RECALL_TOOL, &json!({"query": "x"})).await;
        assert!(out.is_error, "recall must not dispatch without a store");
    }

    #[tokio::test]
    async fn shell_failure_reports_exit_code() {
        let exec = allowing_executor();
        let out = exec
            .execute(SHELL_TOOL, &json!({"command": "exit 3"}))
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("(exit 3)"), "{}", out.content);
    }
}
