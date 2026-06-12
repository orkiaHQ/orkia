// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Ephemeral journal-derived activity model for KG MCP enrichment.
//!
//! The REPL and MCP dispatcher do not share mutable state here. A background
//! actor owns the activity map; the journal subscriber sends observations and
//! the synchronous MCP dispatcher asks for a bounded snapshot over a channel.

use std::collections::{HashMap, VecDeque};
use std::path::{Component, Path};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use orkia_shell_types::JournalEnvelope;

use crate::JobId;

const MAX_DOMAINS: usize = 5;
const DEFAULT_TTL: Duration = Duration::from_secs(10 * 60);
const QUERY_TIMEOUT: Duration = Duration::from_millis(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveDomain {
    pub domain: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
struct JobActivity {
    domains: VecDeque<DomainHit>,
    last_target_path: Option<String>,
    last_tool: Option<String>,
    updated_at: Instant,
}

#[derive(Debug, Clone)]
struct DomainHit {
    domain: String,
    reason: String,
    seen_at: Instant,
}

impl JobActivity {
    fn new(now: Instant) -> Self {
        Self {
            domains: VecDeque::new(),
            last_target_path: None,
            last_tool: None,
            updated_at: now,
        }
    }

    fn observe_domain(&mut self, domain: String, reason: String, now: Instant) {
        self.domains.retain(|d| d.domain != domain);
        self.domains.push_front(DomainHit {
            domain,
            reason,
            seen_at: now,
        });
        self.domains.truncate(MAX_DOMAINS);
        self.updated_at = now;
    }

    fn active_domains(&self, now: Instant, ttl: Duration) -> Vec<ActiveDomain> {
        self.domains
            .iter()
            .filter(|d| now.duration_since(d.seen_at) <= ttl)
            .map(|d| ActiveDomain {
                domain: d.domain.clone(),
                reason: d.reason.clone(),
            })
            .collect()
    }
}

#[derive(Debug)]
pub struct ActivityModel {
    jobs: HashMap<u32, JobActivity>,
    ttl: Duration,
}

impl Default for ActivityModel {
    fn default() -> Self {
        Self {
            jobs: HashMap::new(),
            ttl: DEFAULT_TTL,
        }
    }
}

impl ActivityModel {
    #[cfg(test)]
    fn with_ttl(ttl: Duration) -> Self {
        Self {
            jobs: HashMap::new(),
            ttl,
        }
    }

    fn observe(&mut self, env: &JournalEnvelope, now: Instant) {
        let Some(job_id) = env.job_id else { return };
        let entry = self
            .jobs
            .entry(job_id)
            .or_insert_with(|| JobActivity::new(now));
        if let Some(tool) = env.tool.as_deref() {
            entry.last_tool = Some(tool.to_string());
        }
        for path in paths_from_env(env) {
            if let Some(domain) = infer_domain_from_path(&path) {
                entry.last_target_path = Some(path);
                entry.observe_domain(domain, "detected from recent file activity".into(), now);
            }
        }
    }

    fn active_domains(&mut self, job_id: JobId, now: Instant) -> Vec<ActiveDomain> {
        self.prune(now);
        self.jobs
            .get(&job_id.0)
            .map(|j| j.active_domains(now, self.ttl))
            .unwrap_or_default()
    }

    fn prune(&mut self, now: Instant) {
        let ttl = self.ttl;
        self.jobs.retain(|_, job| {
            job.domains.retain(|d| now.duration_since(d.seen_at) <= ttl);
            !job.domains.is_empty() || now.duration_since(job.updated_at) <= ttl
        });
    }
}

enum Command {
    Observe(Box<JournalEnvelope>),
    Domains {
        job_id: JobId,
        reply: std::sync::mpsc::Sender<Vec<ActiveDomain>>,
    },
}

#[derive(Clone)]
pub struct KnowledgeActivityHandle {
    tx: mpsc::Sender<Command>,
}

impl KnowledgeActivityHandle {
    pub fn active_domains(&self, job_id: JobId) -> Vec<ActiveDomain> {
        let (tx, rx) = mpsc::channel();
        if self
            .tx
            .send(Command::Domains { job_id, reply: tx })
            .is_err()
        {
            return Vec::new();
        }
        rx.recv_timeout(QUERY_TIMEOUT).unwrap_or_default()
    }

    pub fn observe(&self, env: JournalEnvelope) {
        let _ = self.tx.send(Command::Observe(Box::new(env)));
    }
}

pub fn spawn_activity_actor() -> KnowledgeActivityHandle {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut model = ActivityModel::default();
        while let Ok(cmd) = rx.recv() {
            match cmd {
                Command::Observe(env) => model.observe(&env, Instant::now()),
                Command::Domains { job_id, reply } => {
                    let _ = reply.send(model.active_domains(job_id, Instant::now()));
                }
            }
        }
    });
    KnowledgeActivityHandle { tx }
}

pub fn spawn_journal_subscriber(
    handle: KnowledgeActivityHandle,
    mut bus_rx: tokio::sync::broadcast::Receiver<JournalEnvelope>,
) {
    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            match bus_rx.recv().await {
                Ok(env) => handle.observe(env),
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    });
}

fn paths_from_env(env: &JournalEnvelope) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(target) = env.target.as_deref() {
        out.push(target.to_string());
    }
    collect_path_values(&serde_json::Value::Object(env.extra.clone()), &mut out);
    out
}

fn collect_path_values(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) if looks_like_path(s) => out.push(s.clone()),
        serde_json::Value::Array(items) => {
            for item in items {
                collect_path_values(item, out);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                if key.to_ascii_lowercase().contains("path")
                    && let Some(s) = value.as_str()
                    && looks_like_path(s)
                {
                    out.push(s.to_string());
                    continue;
                }
                collect_path_values(value, out);
            }
        }
        _ => {}
    }
}

fn looks_like_path(s: &str) -> bool {
    s.contains('/') || s.ends_with(".rs") || s.ends_with(".toml") || s.ends_with(".md")
}

pub fn infer_domain_from_path(raw: &str) -> Option<String> {
    let parts: Vec<String> = Path::new(raw)
        .components()
        .filter_map(component_text)
        .filter(|p| !p.is_empty())
        .collect();
    if parts.is_empty() {
        return None;
    }
    for window in parts.windows(2) {
        if window[0] == "src" {
            return sanitize_domain(&window[1]);
        }
    }
    for window in parts.windows(3) {
        if window[0] == "crates" && window[2] == "src" {
            return sanitize_domain(&window[1]);
        }
    }
    parts
        .iter()
        .rev()
        .find(|p| meaningful_parent(p))
        .and_then(|p| sanitize_domain(p))
}

fn component_text(c: Component<'_>) -> Option<String> {
    match c {
        Component::Normal(s) => Some(s.to_string_lossy().to_string()),
        _ => None,
    }
}

fn meaningful_parent(part: &str) -> bool {
    !matches!(
        part,
        "src" | "crates" | "tests" | "test" | "target" | "debug" | "release"
    ) && !part.contains('.')
}

fn sanitize_domain(raw: &str) -> Option<String> {
    let out: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    (!out.is_empty()).then(|| out.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_shell_types::EventType;

    #[test]
    fn knowledge_activity_domain_inference_maps_common_paths() {
        assert_eq!(
            infer_domain_from_path("src/auth/pkce.rs").as_deref(),
            Some("auth")
        );
        assert_eq!(
            infer_domain_from_path("crates/x/src/sync/engine.rs").as_deref(),
            Some("sync")
        );
        assert_eq!(
            infer_domain_from_path("docs/architecture/kg.md").as_deref(),
            Some("architecture")
        );
    }

    #[test]
    fn knowledge_activity_ttl_and_domain_cap_are_deterministic() {
        let mut model = ActivityModel::with_ttl(Duration::from_secs(10));
        let now = Instant::now();
        for idx in 0..6 {
            let mut env = JournalEnvelope::now(EventType::Hook);
            env.job_id = Some(7);
            env.target = Some(format!("src/domain{idx}/file.rs"));
            model.observe(&env, now + Duration::from_secs(idx));
        }
        let hits = model.active_domains(JobId(7), now + Duration::from_secs(6));
        assert_eq!(hits.len(), 5);
        assert_eq!(hits[0].domain, "domain5");
        assert_eq!(hits[4].domain, "domain1");

        let expired = model.active_domains(JobId(7), now + Duration::from_secs(30));
        assert!(expired.is_empty());
    }
}
