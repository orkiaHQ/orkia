// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `$reasoning` shell builtin — the user-facing surface of Orkia Intelligence.
//!
//! Subcommands:
//!
//! - `$reasoning` / `$reasoning status` — gate + worker state and local counts
//! - `$reasoning sync`                  — wake the sync worker now
//! - `$reasoning graph [--rfc <id>|--project <uuid>|--domain <name>|--status <state>]` — list nodes
//! - `$reasoning recall <query>`        — human mirror of the agent's MCP `recall`
//! - `$reasoning purge`                 — handled by the REPL (drops local data)
//!
//! Reads open a fresh read-only connection to the local store file (WAL lets a
//! reader run alongside the consumer's writer — one owner per connection,
//! CLAUDE.md #2). Nothing here touches the network directly; `sync` only sends
//! the running worker a wake message.

use std::path::{Path, PathBuf};

use orkia_kernel::{GateState, Intelligence};
use orkia_reasoning_core::compile_context_block;
use orkia_reasoning_core::dto::KnowledgeNode;
use orkia_reasoning_store::ReasoningStore;
use orkia_shell_types::BlockContent;

/// The on-disk location of the local reasoning store, mirroring the path the
/// REPL boots the consumer against (`<data_dir>/reasoning/reasoning.db`).
pub fn store_path(data_dir: &Path) -> PathBuf {
    data_dir.join("reasoning").join("reasoning.db")
}

fn header(label: impl Into<String>) -> BlockContent {
    BlockContent::SystemInfo(format!(" {}", label.into()))
}

fn line(text: impl Into<String>) -> BlockContent {
    BlockContent::Text(text.into())
}

/// `$reasoning status` — gate state, worker liveness, and local row counts.
pub fn status(intel: Option<&Intelligence>, data_dir: &Path) -> Vec<BlockContent> {
    let mut out = vec![header("orkia intelligence")];
    match intel {
        None => {
            out.push(line("  state:     inactive (login + premium required)"));
            out.push(line(
                "  reasoning capture is a premium feature — run `login`",
            ));
            return out;
        }
        Some(i) => {
            out.push(line(format!("  gate:      {}", gate_label(i.gate_state()))));
            out.push(line(format!(
                "  capture:   {}",
                if i.is_active() { "on" } else { "off" }
            )));
            out.push(line(format!(
                "  sync:      {}",
                if i.premium_denied() {
                    "parked (server denied — re-`login`)"
                } else if i.sync_active() {
                    "on"
                } else {
                    "off (offline)"
                }
            )));
        }
    }
    append_stats(&mut out, data_dir);
    out
}

fn gate_label(state: GateState) -> &'static str {
    match state {
        GateState::Enabled => "enabled",
        GateState::Anonymous => "anonymous (not logged in)",
        GateState::FreePlan => "free plan (premium required)",
    }
}

fn append_stats(out: &mut Vec<BlockContent>, data_dir: &Path) {
    let path = store_path(data_dir);
    if !path.exists() {
        out.push(line("  store:     (empty)"));
        return;
    }
    match ReasoningStore::open(&path).and_then(|s| s.stats()) {
        Ok(s) => {
            out.push(line(format!(
                "  sessions:  {}   turns: {}   nodes: {}",
                s.sessions, s.turns, s.nodes
            )));
            out.push(line(format!(
                "  pending:   {} turns, {} signals awaiting sync",
                s.dirty_turns, s.dirty_signals
            )));
        }
        Err(e) => out.push(line(format!("  store:     ✗ unreadable ({e})"))),
    }
}

/// `$reasoning sync` — ask the running worker to push/pull now.
pub fn sync(intel: Option<&Intelligence>) -> Vec<BlockContent> {
    match intel {
        None => vec![header(
            "✗ intelligence inactive — `login` on a premium plan first",
        )],
        Some(i) if i.premium_denied() => vec![header(
            "✗ sync parked — the server denied premium; re-`login` to retry",
        )],
        Some(i) if i.request_sync() => vec![header("✓ sync requested")],
        Some(_) => vec![header("✗ sync worker offline (no backend configured)")],
    }
}

/// `$reasoning graph [--rfc <id>|--project <uuid>]` — list knowledge nodes.
pub fn graph(data_dir: &Path, args: &[String]) -> Vec<BlockContent> {
    let path = store_path(data_dir);
    if !path.exists() {
        return vec![header("no reasoning data yet")];
    }
    let store = match ReasoningStore::open(&path) {
        Ok(s) => s,
        Err(e) => return vec![header(format!("✗ store unreadable ({e})"))],
    };
    let nodes = match select_nodes(&store, args) {
        Ok(n) => n,
        Err(msg) => return vec![header(msg)],
    };
    render_nodes(nodes)
}

fn select_nodes(store: &ReasoningStore, args: &[String]) -> Result<Vec<KnowledgeNode>, String> {
    match args.first().map(String::as_str) {
        Some("--rfc") => {
            let id = args.get(1).ok_or("✗ usage: reasoning graph --rfc <id>")?;
            store.nodes_for_rfc(id).map_err(|e| format!("✗ {e}"))
        }
        Some("--project") => {
            let raw = args
                .get(1)
                .ok_or("✗ usage: reasoning graph --project <uuid>")?;
            let pid =
                uuid::Uuid::parse_str(raw).map_err(|_| "✗ invalid project uuid".to_string())?;
            store.nodes_for_project(pid).map_err(|e| format!("✗ {e}"))
        }
        Some("--domain") => {
            let d = args
                .get(1)
                .ok_or("✗ usage: reasoning graph --domain <name>")?;
            store.nodes_for_domain(d, 50).map_err(|e| format!("✗ {e}"))
        }
        Some("--status") => {
            let s = args
                .get(1)
                .ok_or("✗ usage: reasoning graph --status <state>")?;
            store.nodes_by_status(s, 50).map_err(|e| format!("✗ {e}"))
        }
        Some(other) => Err(format!("✗ unknown flag: {other}")),
        None => store.recent_nodes(50).map_err(|e| format!("✗ {e}")),
    }
}

/// `reasoning recall <query>` — the human mirror of the agent's MCP `recall`
/// whose summary contains the query (case-insensitive), rendering each node's
/// deterministic `context_block` so the human sees exactly what an agent would.
pub fn recall(data_dir: &Path, args: &[String]) -> Vec<BlockContent> {
    let query = args.join(" ");
    if query.trim().is_empty() {
        return vec![header("usage: reasoning recall <query>")];
    }
    let path = store_path(data_dir);
    if !path.exists() {
        return vec![header("no reasoning data yet")];
    }
    let store = match ReasoningStore::open(&path) {
        Ok(s) => s,
        Err(e) => return vec![header(format!("✗ store unreadable ({e})"))],
    };
    let nodes = match store.recent_nodes(200) {
        Ok(n) => n,
        Err(e) => return vec![header(format!("✗ {e}"))],
    };
    let q = query.trim().to_lowercase();
    let hits: Vec<KnowledgeNode> = nodes
        .into_iter()
        .filter(|n| n.summary.to_lowercase().contains(&q))
        .take(8)
        .collect();
    render_recall(&query, hits)
}

fn render_recall(query: &str, hits: Vec<KnowledgeNode>) -> Vec<BlockContent> {
    if hits.is_empty() {
        return vec![header(format!("no cached knowledge for “{query}”"))];
    }
    let mut out = vec![header(format!("recall “{query}” ({} nodes)", hits.len()))];
    for n in &hits {
        out.push(line(format!("  {}", compile_context_block(n))));
    }
    out
}

fn render_nodes(nodes: Vec<KnowledgeNode>) -> Vec<BlockContent> {
    if nodes.is_empty() {
        return vec![header("no knowledge nodes for that scope")];
    }
    let mut out = vec![header(format!("knowledge graph ({} nodes)", nodes.len()))];
    for n in nodes {
        let rfc = n
            .rfc_ref
            .as_ref()
            .map(|r| format!(" [{}]", r.rfc_id.as_str()))
            .unwrap_or_default();
        out.push(line(format!(
            "  {:<10} {:.2}  {}{}",
            format!("{:?}", n.kind),
            n.confidence,
            n.summary,
            rfc
        )));
    }
    out
}

/// `$reasoning` with no/unknown subcommand — usage.
pub fn usage(sub: &str) -> Vec<BlockContent> {
    let msg = if sub.is_empty() {
        "usage: reasoning {status|sync|graph|recall|purge}".to_string()
    } else {
        format!("✗ unknown subcommand: reasoning {sub}")
    };
    vec![header(msg)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use orkia_reasoning_core::enums::{KnowledgeNodeKind, NodeOrigin};
    use orkia_reasoning_store::NodeInsert;
    use uuid::Uuid;

    fn text_of(blocks: &[BlockContent]) -> String {
        blocks
            .iter()
            .map(|b| match b {
                BlockContent::SystemInfo(s) | BlockContent::Text(s) => s.clone(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn seeded_dir(summary: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let store = ReasoningStore::open(&path).unwrap();
        let node = KnowledgeNode {
            id: Uuid::new_v4(),
            workspace_id: Uuid::from_u128(1),
            project_id: None,
            rfc_ref: None,
            kind: KnowledgeNodeKind::Decision,
            summary: summary.into(),
            confidence: 0.9,
            origin: NodeOrigin::Cloud,
            created_at: Utc::now(),
        };
        store
            .upsert_node(&NodeInsert {
                node: &node,
                details: None,
                domain: None,
                context_block: None,
                source_turn_id: None,
                source_session_id: None,
                seal_id: None,
            })
            .unwrap();
        dir
    }

    #[test]
    fn recall_renders_context_block_for_match_and_usage_when_empty() {
        let dir = seeded_dir("Auth uses PKCE for the device flow");
        let hit = recall(dir.path(), &["pkce".into()]);
        let s = text_of(&hit);
        assert!(s.contains("recall"));
        assert!(s.contains("[DECISION:0.90]"));
        assert!(s.contains("PKCE"));

        // No query → usage, never a panic.
        assert!(text_of(&recall(dir.path(), &[])).contains("usage: reasoning recall"));

        // Query with no match → explicit miss, exit-clean.
        assert!(text_of(&recall(dir.path(), &["nonexistent".into()])).contains("no cached"));
    }

    #[test]
    fn graph_status_filter_separates_lifecycle() {
        let dir = seeded_dir("A decision");
        // Active node shows under --status active and not under superseded.
        assert!(
            text_of(&graph(dir.path(), &["--status".into(), "active".into()])).contains("1 nodes")
        );
        assert!(
            text_of(&graph(
                dir.path(),
                &["--status".into(), "superseded".into()]
            ))
            .contains("no knowledge nodes")
        );
    }
}
