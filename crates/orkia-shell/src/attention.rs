// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Global attention queue actor.

#[path = "attention/audit.rs"]
mod audit;
#[path = "attention/resource.rs"]
mod resource;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use arc_swap::ArcSwap;
use orkia_shell_types::attention::{
    AttentionAction, AttentionCommandResult, AttentionControl, AttentionHint, AttentionId,
    AttentionKind, AttentionResolveEffect, AttentionRow, AttentionSeverity,
};
use orkia_shell_types::{JobId, JournalEnvelope};
use tokio::sync::mpsc::UnboundedSender as JournalSender;

const ATTENTION_TICK: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct AttentionCoordinator {
    tx: Sender<Command>,
    snapshot: Arc<ArcSwap<Snapshot>>,
}

#[derive(Clone, Default)]
struct Snapshot {
    rows: Vec<AttentionRow>,
    hint: Option<AttentionHint>,
}

#[derive(Debug)]
enum Command {
    AgentPrompt(AgentPromptInput),
    QueuedInput(QueuedInputInput),
    BlockingApproval(BlockingApprovalInput),
    ResolveByJob(JobId),
    JobEnded(JobId),
    SetJournalSender(Option<JournalSender<JournalEnvelope>>),
    ObserveHook(Box<JournalEnvelope>),
    Pull(Sender<AttentionCommandResult>),
    Resolve {
        id: AttentionId,
        action: String,
        reply: Sender<AttentionCommandResult>,
    },
}

#[derive(Debug)]
pub struct AgentPromptInput {
    pub job_id: JobId,
    pub agent: String,
    pub summary: String,
    pub pending_body: Option<String>,
}

#[derive(Debug)]
pub struct QueuedInputInput {
    pub job_id: JobId,
    pub agent: String,
    pub depth: usize,
    pub body: String,
}

#[derive(Debug)]
pub struct BlockingApprovalInput {
    pub job_id: JobId,
    pub agent: String,
    pub action: String,
    pub risk: String,
}

#[derive(Clone)]
struct Entry {
    id: AttentionId,
    job_id: Option<JobId>,
    agent: String,
    kind: AttentionKind,
    created_at: chrono::DateTime<chrono::Utc>,
    summary: String,
    actions: Vec<AttentionAction>,
    resource: Option<PathBuf>,
    last_journaled_severity: AttentionSeverity,
}

#[derive(Default)]
struct ResourceTracker {
    reads: HashMap<PathBuf, ResourceAccess>,
    held_jobs: std::collections::HashSet<JobId>,
}

#[derive(Clone)]
struct ResourceAccess {
    job_id: JobId,
    agent: String,
}

impl AttentionCoordinator {
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel();
        let snapshot = Arc::new(ArcSwap::from_pointee(Snapshot::default()));
        let thread_snapshot = Arc::clone(&snapshot);
        let spawn_result = thread::Builder::new()
            .name("orkia-attention".into())
            .spawn(move || run_actor(rx, thread_snapshot));
        if let Err(e) = spawn_result {
            tracing::error!(error = %e, "attention coordinator failed to spawn");
        }
        Self { tx, snapshot }
    }

    pub fn rows(&self) -> Vec<AttentionRow> {
        self.snapshot.load_full().rows.clone()
    }

    pub fn hint(&self) -> Option<AttentionHint> {
        self.snapshot.load_full().hint.clone()
    }

    pub fn agent_prompt(&self, input: AgentPromptInput) {
        let _ = self.tx.send(Command::AgentPrompt(input));
    }

    pub fn queued_input(&self, input: QueuedInputInput) {
        let _ = self.tx.send(Command::QueuedInput(input));
    }

    pub fn blocking_approval(&self, input: BlockingApprovalInput) {
        let _ = self.tx.send(Command::BlockingApproval(input));
    }

    pub fn resolve_by_job(&self, job_id: JobId) {
        let _ = self.tx.send(Command::ResolveByJob(job_id));
    }

    pub fn job_ended(&self, job_id: JobId) {
        let _ = self.tx.send(Command::JobEnded(job_id));
    }

    pub fn observe_hook(&self, env: &JournalEnvelope) {
        let _ = self.tx.send(Command::ObserveHook(Box::new(env.clone())));
    }

    pub fn set_journal_sender(&self, tx: Option<JournalSender<JournalEnvelope>>) {
        let _ = self.tx.send(Command::SetJournalSender(tx));
    }
}

impl AttentionControl for AttentionCoordinator {
    fn pull(&self) -> AttentionCommandResult {
        let (tx, rx) = mpsc::channel();
        if self.tx.send(Command::Pull(tx)).is_err() {
            return unavailable();
        }
        rx.recv().unwrap_or_else(|_| unavailable())
    }

    fn resolve(&self, id: AttentionId, action: &str) -> AttentionCommandResult {
        let (tx, rx) = mpsc::channel();
        if self
            .tx
            .send(Command::Resolve {
                id,
                action: action.to_string(),
                reply: tx,
            })
            .is_err()
        {
            return unavailable();
        }
        rx.recv().unwrap_or_else(|_| unavailable())
    }
}

fn unavailable() -> AttentionCommandResult {
    AttentionCommandResult {
        rows: Vec::new(),
        message: Some("attention coordinator unavailable".into()),
        effect: AttentionResolveEffect::None,
    }
}

fn run_actor(rx: Receiver<Command>, snapshot: Arc<ArcSwap<Snapshot>>) {
    let mut state = State::default();
    loop {
        match rx.recv_timeout(ATTENTION_TICK) {
            Ok(cmd) => {
                let reply = state.apply(cmd);
                state.publish_snapshot(&snapshot);
                if let Some((tx, result)) = reply {
                    let _ = tx.send(result);
                }
            }
            Err(RecvTimeoutError::Timeout) => state.publish_snapshot(&snapshot),
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

#[derive(Default)]
struct State {
    next_id: u64,
    entries: Vec<Entry>,
    resources: ResourceTracker,
    journal_tx: Option<JournalSender<JournalEnvelope>>,
}

type Reply = Option<(Sender<AttentionCommandResult>, AttentionCommandResult)>;

impl State {
    fn apply(&mut self, cmd: Command) -> Reply {
        match cmd {
            Command::AgentPrompt(input) => {
                self.upsert(EntrySpec {
                    key: EntryKey::KindJob(AttentionKind::AgentPrompt, input.job_id),
                    job_id: Some(input.job_id),
                    agent: input.agent,
                    kind: AttentionKind::AgentPrompt,
                    summary: input
                        .pending_body
                        .map(|b| format!("waiting for input — pending: {}", truncate(&b, 80)))
                        .unwrap_or(input.summary),
                    actions: vec![AttentionAction::Pull, AttentionAction::Inspect],
                    resource: None,
                });
                None
            }
            Command::QueuedInput(input) => {
                self.upsert(EntrySpec {
                    key: EntryKey::KindJob(AttentionKind::QueuedInput, input.job_id),
                    job_id: Some(input.job_id),
                    agent: input.agent,
                    kind: AttentionKind::QueuedInput,
                    summary: format!(
                        "{} queued input(s): {}",
                        input.depth,
                        truncate(&input.body, 80)
                    ),
                    actions: vec![AttentionAction::Pull],
                    resource: None,
                });
                None
            }
            Command::BlockingApproval(input) => {
                self.upsert(EntrySpec {
                    key: EntryKey::KindJob(AttentionKind::BlockingApproval, input.job_id),
                    job_id: Some(input.job_id),
                    agent: input.agent,
                    kind: AttentionKind::BlockingApproval,
                    summary: format!("approval needed: {} (risk: {})", input.action, input.risk),
                    actions: vec![
                        AttentionAction::Allow,
                        AttentionAction::Deny,
                        AttentionAction::Inspect,
                    ],
                    resource: None,
                });
                None
            }
            Command::ResolveByJob(job_id) => {
                self.remove_job_entries(job_id, "attention.resolved");
                self.resources
                    .reads
                    .retain(|_, access| access.job_id != job_id);
                self.resources.held_jobs.remove(&job_id);
                None
            }
            Command::JobEnded(job_id) => {
                self.remove_job_entries(job_id, "attention.expired");
                self.resources
                    .reads
                    .retain(|_, access| access.job_id != job_id);
                self.resources.held_jobs.remove(&job_id);
                None
            }
            Command::SetJournalSender(tx) => {
                self.journal_tx = tx;
                None
            }
            Command::ObserveHook(env) => {
                self.observe_hook(&env);
                None
            }
            Command::Pull(reply) => {
                let rows = self.rows();
                if let Some(row) = rows.first() {
                    self.emit_row_event("attention.pulled", row);
                }
                let msg = rows
                    .first()
                    .map(|r| format!("{} {} · {}", r.id, r.kind.as_str(), r.summary))
                    .or_else(|| Some("attention queue is empty".into()));
                Some((
                    reply,
                    AttentionCommandResult {
                        rows: rows.into_iter().take(1).collect(),
                        message: msg,
                        effect: AttentionResolveEffect::None,
                    },
                ))
            }
            Command::Resolve { id, action, reply } => {
                let result = self.resolve(id, &action);
                Some((reply, result))
            }
        }
    }

    fn upsert(&mut self, spec: EntrySpec) {
        if let Some(entry) = self.entries.iter_mut().find(|e| spec.matches(e)) {
            entry.summary = spec.summary;
            entry.actions = spec.actions;
            return;
        }
        self.next_id = self.next_id.saturating_add(1);
        let mut entry = Entry {
            id: AttentionId(self.next_id),
            job_id: spec.job_id,
            agent: spec.agent,
            kind: spec.kind,
            created_at: chrono::Utc::now(),
            summary: spec.summary,
            actions: spec.actions,
            resource: spec.resource,
            last_journaled_severity: AttentionSeverity::Fresh,
        };
        entry.last_journaled_severity = severity_for(&entry);
        self.emit_entry_event("attention.queued", &entry);
        if entry.kind == AttentionKind::ResourceConflict {
            self.emit_entry_event("resource.conflict", &entry);
        }
        self.entries.push(entry);
    }

    fn rows(&self) -> Vec<AttentionRow> {
        let mut entries: Vec<_> = self.entries.iter().collect();
        entries.sort_by(|a, b| {
            a.created_at.cmp(&b.created_at).then_with(|| {
                severity_rank(severity_for(b))
                    .cmp(&severity_rank(severity_for(a)))
                    .then_with(|| a.id.cmp(&b.id))
            })
        });
        entries.into_iter().map(row_for).collect()
    }

    fn resolve(&mut self, id: AttentionId, action: &str) -> AttentionCommandResult {
        let Some(pos) = self.entries.iter().position(|e| e.id == id) else {
            return AttentionCommandResult {
                rows: Vec::new(),
                message: Some(format!("attention entry {id} not found")),
                effect: AttentionResolveEffect::None,
            };
        };
        let entry = self.entries[pos].clone();
        if entry.kind == AttentionKind::ResourceConflict
            && action == "hold"
            && let Some(job_id) = entry.job_id
        {
            self.resources.held_jobs.insert(job_id);
            self.emit_entry_event("attention.resolved", &entry);
            return AttentionCommandResult {
                rows: vec![row_for(&entry)],
                message: Some(format!("{id} held")),
                effect: AttentionResolveEffect::HoldJob(job_id.0),
            };
        }
        let effect = resolve_effect(&entry, action);
        self.entries.remove(pos);
        if let Some(job_id) = entry.job_id
            && matches!(effect, AttentionResolveEffect::ReleaseJob(_))
        {
            self.resources.held_jobs.remove(&job_id);
        }
        self.emit_entry_event("attention.resolved", &entry);
        AttentionCommandResult {
            rows: vec![row_for(&entry)],
            message: Some(format!("{id} resolved with {action}")),
            effect,
        }
    }

    fn remove_job_entries(&mut self, job_id: JobId, event: &str) {
        let mut kept = Vec::with_capacity(self.entries.len());
        let entries = std::mem::take(&mut self.entries);
        for entry in entries {
            if entry.job_id == Some(job_id) {
                self.emit_entry_event(event, &entry);
            } else {
                kept.push(entry);
            }
        }
        self.entries = kept;
    }

    fn publish_snapshot(&mut self, snapshot: &ArcSwap<Snapshot>) {
        self.emit_aged_events();
        let rows = self.rows();
        let hint = hint_for(&rows);
        snapshot.store(Arc::new(Snapshot { rows, hint }));
    }

    fn emit_aged_events(&mut self) {
        let mut changed = Vec::new();
        for entry in &mut self.entries {
            let severity = severity_for(entry);
            if severity != entry.last_journaled_severity {
                entry.last_journaled_severity = severity;
                changed.push(entry.clone());
            }
        }
        for entry in changed {
            self.emit_entry_event("attention.aged", &entry);
        }
    }
}

struct EntrySpec {
    key: EntryKey,
    job_id: Option<JobId>,
    agent: String,
    kind: AttentionKind,
    summary: String,
    actions: Vec<AttentionAction>,
    resource: Option<PathBuf>,
}

enum EntryKey {
    KindJob(AttentionKind, JobId),
    Conflict(PathBuf, JobId),
}

impl EntrySpec {
    fn matches(&self, e: &Entry) -> bool {
        match &self.key {
            EntryKey::KindJob(kind, job) => e.kind == *kind && e.job_id == Some(*job),
            EntryKey::Conflict(path, job) => {
                e.kind == AttentionKind::ResourceConflict
                    && e.job_id == Some(*job)
                    && e.resource.as_ref() == Some(path)
            }
        }
    }
}

fn hint_for(rows: &[AttentionRow]) -> Option<AttentionHint> {
    let blocking = rows
        .iter()
        .filter(|r| r.kind == AttentionKind::BlockingApproval)
        .count();
    if blocking > 0 {
        return Some(AttentionHint::Blocking { count: blocking });
    }
    if rows.is_empty() {
        return None;
    }
    if rows
        .iter()
        .any(|r| r.kind == AttentionKind::ResourceConflict)
    {
        return Some(AttentionHint::Passive(format!(
            "({} queued · conflict · ^G)",
            rows.len()
        )));
    }
    if rows.len() == 1 {
        return Some(AttentionHint::Passive(format!(
            "(@{} queued · ^G)",
            rows[0].agent
        )));
    }
    Some(AttentionHint::Passive(format!(
        "({} queued · oldest {} · ^G)",
        rows.len(),
        rows[0].age
    )))
}

fn row_for(entry: &Entry) -> AttentionRow {
    AttentionRow {
        id: entry.id,
        job_id: entry.job_id.map(|id| id.0),
        agent: entry.agent.clone(),
        kind: entry.kind,
        severity: severity_for(entry),
        age: age_label(entry.created_at),
        summary: entry.summary.clone(),
        actions: entry.actions.clone(),
    }
}

fn severity_for(entry: &Entry) -> AttentionSeverity {
    match entry.kind {
        AttentionKind::BlockingApproval => AttentionSeverity::Blocking,
        AttentionKind::ResourceConflict => AttentionSeverity::Conflict,
        _ => age_severity(entry.created_at),
    }
}

fn severity_rank(severity: AttentionSeverity) -> u8 {
    match severity {
        AttentionSeverity::Blocking => 5,
        AttentionSeverity::Conflict => 4,
        AttentionSeverity::Overdue => 3,
        AttentionSeverity::Warning => 2,
        AttentionSeverity::Muted => 1,
        AttentionSeverity::Fresh => 0,
    }
}

fn age_severity(created_at: chrono::DateTime<chrono::Utc>) -> AttentionSeverity {
    let age = chrono::Utc::now().signed_duration_since(created_at);
    severity_for_age(age)
}

fn severity_for_age(age: chrono::Duration) -> AttentionSeverity {
    if age >= chrono::Duration::hours(1) {
        AttentionSeverity::Overdue
    } else if age >= chrono::Duration::minutes(30) {
        AttentionSeverity::Warning
    } else if age >= chrono::Duration::minutes(15) {
        AttentionSeverity::Muted
    } else {
        AttentionSeverity::Fresh
    }
}

fn age_label(created_at: chrono::DateTime<chrono::Utc>) -> String {
    let age = chrono::Utc::now().signed_duration_since(created_at);
    if age.num_hours() > 0 {
        format!("{}h {}m", age.num_hours(), age.num_minutes() % 60)
    } else if age.num_minutes() > 0 {
        format!("{}m", age.num_minutes())
    } else {
        "now".into()
    }
}

fn resolve_effect(entry: &Entry, action: &str) -> AttentionResolveEffect {
    match (entry.kind, action) {
        (AttentionKind::BlockingApproval, "allow") => entry
            .job_id
            .map(|id| AttentionResolveEffect::Approval {
                job_id: id.0,
                approved: true,
            })
            .unwrap_or(AttentionResolveEffect::None),
        (AttentionKind::BlockingApproval, "deny") => entry
            .job_id
            .map(|id| AttentionResolveEffect::Approval {
                job_id: id.0,
                approved: false,
            })
            .unwrap_or(AttentionResolveEffect::None),
        (AttentionKind::ResourceConflict, action) if action.starts_with("abort-") => entry
            .job_id
            .map(|id| AttentionResolveEffect::StopJob(id.0))
            .unwrap_or(AttentionResolveEffect::None),
        (AttentionKind::ResourceConflict, "proceed-anyway") => entry
            .job_id
            .map(|id| AttentionResolveEffect::ReleaseJob(id.0))
            .unwrap_or(AttentionResolveEffect::None),
        _ => AttentionResolveEffect::None,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push_str("...");
        out
    }
}
