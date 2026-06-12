// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `orkia reasoning backfill <file>` — stage a corpus of historical agent
//! sessions into the reasoning graph.
//!
//! The companion Python parser (`script/backfill_reasoning.py`) turns Claude
//! transcripts into a stream of [`JournalEnvelope`] JSON lines — the *exact*
//! wire type the live hot path consumes — with historical timestamps and a
//! synthetic per-session `job_id`. This command replays those envelopes through
//! the real [`ReasoningConsumer`] (same scrub, hash, enum encoding, session and
//! sequence bookkeeping as live capture — one owner of the encoding contract),
//! then flushes the staged dirty turns to the cloud with a one-shot
//! [`backfill_sync`].
//!
//! Synchronous staging (not the broadcast bus) is deliberate: a bulk replay
//! would overrun the bus capacity and the consumer would drop lagged frames.
//! `ingest()` processes every envelope, in order, with no drops.
//!
//! Fail-closed (#8): the gate must be open (premium login + parseable
//! workspace/account UUIDs) or nothing is written.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use orkia_auth::AuthProvider;
use orkia_kernel::{
    BackfillSyncConfig, CaptureScope, Intelligence, ReasoningConsumer, backfill_sync,
    new_job_scopes,
};
use orkia_magic_login::MagicLinkAuthProvider;
use orkia_reasoning_store::ReasoningStore;
use orkia_shell_types::journal::JournalEnvelope;
use sha2::{Digest, Sha256};

use crate::seal::{SealChain, SealManager};

/// Parsed flags for `reasoning backfill`.
struct BackfillArgs {
    file: PathBuf,
    push: bool,
    dry_run: bool,
}

/// Entry point routed from the binary (`orkia reasoning backfill ...`). Returns
/// the process exit code.
pub async fn run(args: &[String]) -> i32 {
    let parsed = match parse_args(args) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("orkia reasoning backfill: {msg}");
            eprintln!("usage: orkia reasoning backfill <envelopes.jsonl> [--no-push] [--dry-run]");
            return 2;
        }
    };
    // `--dry-run` writes nothing, so it does not need the premium gate — this
    // lets the Python parser's output be validated without premium creds.
    if parsed.dry_run {
        return dry_run(&parsed.file);
    }
    let scope = match resolve_scope() {
        Ok(s) => s,
        Err(code) => return code,
    };
    let data_dir = match data_dir() {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("orkia reasoning backfill: {msg}");
            return 1;
        }
    };
    stage_and_sync(parsed, scope, data_dir).await
}

/// Count parseable vs unparseable envelope lines without touching the store.
fn dry_run(file: &Path) -> i32 {
    let contents = match std::fs::read_to_string(file) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("orkia reasoning backfill: read {}: {e}", file.display());
            return 1;
        }
    };
    let (ok, bad) = count_envelopes(&contents);
    println!("dry-run: {ok} valid envelope(s), {bad} unparseable line(s) — nothing written");
    0
}

fn parse_args(args: &[String]) -> Result<BackfillArgs, String> {
    let mut file: Option<PathBuf> = None;
    let mut push = true;
    let mut dry_run = false;
    for a in args {
        match a.as_str() {
            "--no-push" => push = false,
            "--dry-run" => dry_run = true,
            other if other.starts_with('-') => {
                return Err(format!("unknown flag: {other}"));
            }
            path => {
                if file.is_some() {
                    return Err("more than one input file given".into());
                }
                file = Some(PathBuf::from(path));
            }
        }
    }
    let file = file.ok_or("missing input file")?;
    Ok(BackfillArgs {
        file,
        push,
        dry_run,
    })
}

/// The persisted-session auth provider. Identity comes from the session
/// saved by `orkia login` (keychain or `ORKIA_SESSION_FILE`), never from
/// env. The base URL is only used for network calls (the push path); for
/// identity resolution the store alone matters, so a missing URL is fine.
fn session_provider() -> Arc<dyn AuthProvider> {
    let base = orkia_shell_types::backend::resolve_backend_url(None).unwrap_or_default();
    Arc::new(MagicLinkAuthProvider::new(base))
}

/// Resolve the premium reasoning identity from the persisted login.
/// Fail-closed: no usable identity ⇒ a non-zero exit and a hint, never a write.
fn resolve_scope() -> Result<CaptureScope, i32> {
    let intel = Intelligence::new(session_provider(), None);
    match intel.identity() {
        Some(id) => Ok(CaptureScope {
            workspace_id: id.workspace_id,
            account_id: id.account_id,
            project_id: None,
            rfc_ref: None,
        }),
        None => {
            eprintln!(
                "orkia reasoning backfill: intelligence inactive — run `orkia login` on a \
                 premium plan first"
            );
            Err(1)
        }
    }
}

/// `$HOME/.orkia` — the data dir the REPL boots from. The reasoning store and
/// the SEAL workspace chain both live under it.
fn data_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(Path::new(&home).join(".orkia"))
}

/// `<data_dir>/reasoning/reasoning.db`, mirroring the REPL's boot path.
fn store_path(data_dir: &Path) -> PathBuf {
    data_dir.join("reasoning").join("reasoning.db")
}

async fn stage_and_sync(args: BackfillArgs, scope: CaptureScope, data_dir: PathBuf) -> i32 {
    let store_path = store_path(&data_dir);
    let contents = match std::fs::read_to_string(&args.file) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "orkia reasoning backfill: read {}: {e}",
                args.file.display()
            );
            return 1;
        }
    };
    if let Some(parent) = store_path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("orkia reasoning backfill: create store dir: {e}");
        return 1;
    }
    let staged = match stage(&store_path, &scope, &contents) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("orkia reasoning backfill: staging failed: {e}");
            return 1;
        }
    };
    report_staged(&store_path, staged);
    // Provenance: record the cold-start batch on the workspace SEAL chain
    // before any cloud push. Fail-closed (#8): an audit-write failure aborts —
    // the turns stay staged locally (dirty) but are not pushed un-attested.
    if let Err(e) = seal_backfill(&data_dir, &store_path, &contents, staged) {
        eprintln!(
            "orkia reasoning backfill: SEAL provenance write failed ({e}); \
             {staged} turn(s) staged but not sealed or pushed — aborting"
        );
        return 1;
    }
    if args.push {
        return push(store_path, scope).await;
    }
    println!(
        "--no-push: {} turn(s) staged locally (dirty), not synced",
        staged
    );
    0
}

/// Seal one `reasoning.backfilled` record onto the workspace SEAL chain — the
/// tamper-evident provenance attestation for this cold-start batch. Mirrors the
/// consumer's `reasoning.nodes_consolidated` append (same chain, same
/// `reasoning.` routing); done directly because the standalone CLI runs no live
/// SEAL consumer.
///
/// Fail-closed (#8): `seal_workspace` swallows a *quarantined* chain into `Ok`
/// (the live REPL must not crash on a corrupt chain) — but for backfill that
/// silent no-op means provenance was NOT recorded. We re-check `is_closed()`
/// after the append and surface it as an error so the caller aborts the push
/// rather than shipping un-attested turns.
fn seal_backfill(
    data_dir: &Path,
    store_path: &Path,
    contents: &str,
    staged: usize,
) -> Result<(), String> {
    let corpus_sha256 = hex(Sha256::digest(contents.as_bytes()).as_slice());
    let sessions = ReasoningStore::open(store_path)
        .and_then(|s| s.stats())
        .map(|s| s.sessions)
        .unwrap_or(0);
    let detail = serde_json::json!({
        "turns_staged": staged,
        "sessions": sessions,
        "corpus_sha256": corpus_sha256,
        "origin": "backfill",
    });
    let mut manager = SealManager::new(data_dir.to_path_buf());
    manager
        .seal_workspace("reasoning.backfilled", detail)
        .map_err(|e| format!("append error: {e}"))?;
    if manager.workspace_chain().is_none_or(SealChain::is_closed) {
        return Err(
            "workspace SEAL chain is quarantined (corrupt or unreadable) — record not appended"
                .into(),
        );
    }
    println!(
        "sealed: reasoning.backfilled on workspace chain (corpus sha256 {}…)",
        &corpus_sha256[..corpus_sha256.len().min(12)]
    );
    Ok(())
}

/// Lowercase hex of a byte slice (SHA-256 digest → 64-char string).
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Replay every envelope through the real consumer. Returns the number of turns
/// written (envelopes that mapped to a graph turn).
fn stage(
    store_path: &Path,
    scope: &CaptureScope,
    contents: &str,
) -> Result<usize, orkia_reasoning_store::StoreError> {
    let store = ReasoningStore::open(store_path)?;
    let mut consumer = ReasoningConsumer::with_job_scopes(store, scope.clone(), new_job_scopes());
    let mut turns = 0usize;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Untrusted input (#7): a malformed line is skipped, never fatal.
        let Ok(env) = serde_json::from_str::<JournalEnvelope>(line) else {
            continue;
        };
        if let Ok(Some(_)) = consumer.ingest(&env) {
            turns += 1;
        }
    }
    Ok(turns)
}

/// Drain the staged dirty rows to the cloud once.
async fn push(store_path: PathBuf, scope: CaptureScope) -> i32 {
    let backend_url = match orkia_shell_types::backend::resolve_backend_url(None) {
        Ok(u) => u,
        Err(e) => {
            eprintln!(
                "orkia reasoning backfill: backend URL unusable ({e}); staged locally but not \
                 synced — set ORKIA_BACKEND_URL to an https endpoint and run `reasoning sync`"
            );
            return 0;
        }
    };
    let auth = session_provider();
    let dirty_before = dirty_turns(&store_path);
    let cfg = BackfillSyncConfig {
        store_path: store_path.clone(),
        scope,
        backend_url,
        auth,
    };
    if let Err(e) = backfill_sync(cfg).await {
        eprintln!("orkia reasoning backfill: sync failed: {e}");
        return 1;
    }
    let dirty_after = dirty_turns(&store_path);
    let pushed = dirty_before.saturating_sub(dirty_after);
    println!(
        "synced: {pushed} turn(s) pushed to cloud; {dirty_after} still pending (re-run \
         `reasoning sync` to retry)"
    );
    0
}

fn report_staged(store_path: &Path, turns: usize) {
    match ReasoningStore::open(store_path).and_then(|s| s.stats()) {
        Ok(s) => println!(
            "staged: {turns} turn(s) this run; store now holds {} session(s), {} turn(s), \
             {} pending sync",
            s.sessions, s.turns, s.dirty_turns
        ),
        Err(_) => println!("staged: {turns} turn(s) this run"),
    }
}

/// Count of dirty turns, or 0 when the store is unreadable (best-effort report).
fn dirty_turns(store_path: &Path) -> usize {
    ReasoningStore::open(store_path)
        .and_then(|s| s.stats())
        .map(|s| s.dirty_turns as usize)
        .unwrap_or(0)
}

/// Classify lines for `--dry-run`: how many parse as envelopes vs not.
fn count_envelopes(contents: &str) -> (usize, usize) {
    let mut ok = 0;
    let mut bad = 0;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if serde_json::from_str::<JournalEnvelope>(line).is_ok() {
            ok += 1;
        } else {
            bad += 1;
        }
    }
    (ok, bad)
}
