use super::*;
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

impl State {
    pub(super) fn observe_hook(&mut self, env: &JournalEnvelope) {
        if env.event.as_deref() == Some("PermissionRequest") {
            if let Some(job_id) = env.job_id {
                self.apply(Command::BlockingApproval(BlockingApprovalInput {
                    job_id: JobId(job_id),
                    agent: env.agent.clone().unwrap_or_default(),
                    action: env
                        .action
                        .clone()
                        .or_else(|| env.tool.clone())
                        .unwrap_or_else(|| "request".into()),
                    risk: env.risk.clone().unwrap_or_else(|| "unknown".into()),
                }));
            }
            return;
        }
        if matches!(env.event.as_deref(), Some("PostToolUse" | "Stop")) {
            self.finish_resource_access(env);
            return;
        }
        if env.event.as_deref() != Some("PreToolUse") {
            return;
        }
        let Some(job) = env.job_id.map(JobId) else {
            return;
        };
        let paths = resource_paths(env);
        if paths.is_empty() {
            return;
        }
        let agent = env.agent.clone().unwrap_or_default();
        let intent = resource_intent(env);
        if intent == ResourceIntent::ReadLong {
            for path in paths {
                self.resources.reads.insert(
                    path,
                    ResourceAccess {
                        job_id: job,
                        agent: agent.clone(),
                    },
                );
            }
            return;
        }
        if intent != ResourceIntent::Write {
            return;
        }
        for path in paths {
            let conflicts: Vec<(PathBuf, ResourceAccess)> = self
                .resources
                .reads
                .iter()
                .filter(|(read_path, access)| {
                    access.job_id != job && paths_overlap(read_path, &path)
                })
                .map(|(p, a)| (p.clone(), a.clone()))
                .collect();
            for (read_path, access) in conflicts {
                self.upsert(EntrySpec {
                    key: EntryKey::Conflict(path.clone(), job),
                    job_id: Some(job),
                    agent: agent.clone(),
                    kind: AttentionKind::ResourceConflict,
                    summary: format!(
                        "conflict: requester={agent} resource={} owner={} active_read={}",
                        path.display(),
                        access.agent,
                        read_path.display()
                    ),
                    actions: vec![
                        AttentionAction::Hold,
                        AttentionAction::AbortAgent(agent.clone()),
                        AttentionAction::ProceedAnyway,
                    ],
                    resource: Some(path.clone()),
                });
            }
        }
    }

    fn finish_resource_access(&mut self, env: &JournalEnvelope) {
        let Some(job) = env.job_id.map(JobId) else {
            return;
        };
        let paths = resource_paths(env);
        if paths.is_empty() {
            self.resources
                .reads
                .retain(|_, access| access.job_id != job);
            return;
        }
        self.resources.reads.retain(|read_path, access| {
            access.job_id != job || !paths.iter().any(|path| paths_overlap(read_path, path))
        });
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResourceIntent {
    ReadLong,
    Write,
    Unknown,
}

fn resource_intent(env: &JournalEnvelope) -> ResourceIntent {
    let tool = env.tool.as_deref().unwrap_or_default();
    if matches!(tool, "Read" | "Grep" | "Glob" | "LS") || is_test_command(env) {
        return ResourceIntent::ReadLong;
    }
    if matches!(tool, "Edit" | "Write" | "MultiEdit") || is_write_command(env) {
        return ResourceIntent::Write;
    }
    ResourceIntent::Unknown
}

fn is_test_command(env: &JournalEnvelope) -> bool {
    command_text(env)
        .map(|s| {
            s.contains("cargo test") || s.contains("cargo bench") || s.contains("cargo nextest")
        })
        .unwrap_or(false)
}

fn is_write_command(env: &JournalEnvelope) -> bool {
    command_text(env)
        .map(|s| {
            s.contains("git commit")
                || s.contains("git add")
                || s.contains("git restore")
                || s.contains("git checkout --")
                || s.contains("rm ")
                || s.contains("mv ")
                || s.contains("cp ")
                || s.contains(" >")
                || s.contains(">>")
        })
        .unwrap_or(false)
}

fn command_text(env: &JournalEnvelope) -> Option<&str> {
    env.extra
        .get("command")
        .and_then(Value::as_str)
        .or(env.description.as_deref())
        .or(env.target.as_deref())
}

fn resource_paths(env: &JournalEnvelope) -> Vec<PathBuf> {
    let cwd = env.extra.get("cwd").and_then(Value::as_str).map(Path::new);
    let mut out = Vec::new();
    push_path(&mut out, cwd, env.target.as_deref());
    for key in ["file_path", "path", "target"] {
        push_value_paths(&mut out, cwd, env.extra.get(key));
    }
    push_value_paths(&mut out, cwd, env.extra.get("paths"));
    push_value_paths(&mut out, cwd, env.extra.get("files"));
    if let Some(tool_input) = env.extra.get("tool_input") {
        for key in ["file_path", "path", "target", "paths", "files"] {
            push_value_paths(&mut out, cwd, tool_input.get(key));
        }
    }
    if out.is_empty() {
        push_bash_paths(&mut out, cwd, command_text(env));
    }
    out.sort();
    out.dedup();
    out
}

fn push_value_paths(out: &mut Vec<PathBuf>, cwd: Option<&Path>, value: Option<&Value>) {
    match value {
        Some(Value::String(s)) => push_path(out, cwd, Some(s)),
        Some(Value::Array(items)) => {
            for item in items {
                if let Some(s) = item.as_str() {
                    push_path(out, cwd, Some(s));
                }
            }
        }
        _ => {}
    }
}

fn push_bash_paths(out: &mut Vec<PathBuf>, cwd: Option<&Path>, command: Option<&str>) {
    let Some(command) = command else {
        return;
    };
    let tokens = command.split_whitespace().collect::<Vec<_>>();
    for window in tokens.windows(2) {
        match window {
            ["git", "commit"] => push_path(out, cwd, Some(".")),
            ["rm" | "mv" | "cp", path] => push_path(out, cwd, Some(path)),
            [">" | ">>", path] => push_path(out, cwd, Some(path)),
            _ => {}
        }
    }
    for window in tokens.windows(3) {
        match window {
            ["cargo", "test" | "bench" | "nextest", path] => push_path(out, cwd, Some(path)),
            ["git", "add" | "restore", path] => push_path(out, cwd, Some(path)),
            ["git", "checkout", "--"] => push_path(out, cwd, Some(".")),
            _ => {}
        }
    }
}

fn push_path(out: &mut Vec<PathBuf>, cwd: Option<&Path>, raw: Option<&str>) {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return;
    };
    if raw.len() > 512 || raw.contains('\0') || raw.starts_with('-') {
        return;
    }
    let path = Path::new(raw);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else if let Some(cwd) = cwd {
        cwd.join(path)
    } else {
        path.to_path_buf()
    };
    if let Some(normalized) = normalize_path(&joined) {
        out.push(normalized);
    }
}

fn normalize_path(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    if out.as_os_str().is_empty() {
        Some(PathBuf::from("."))
    } else {
        Some(out)
    }
}

fn paths_overlap(a: &Path, b: &Path) -> bool {
    a == b || a.starts_with(b) || b.starts_with(a)
}
