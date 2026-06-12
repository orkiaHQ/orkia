// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! SealChain watcher.
//!
//! Walks `<seal_root>/{workspace,projects/*,agents/*/jobs/*}/seal.jsonl`
//! and tails each file. New files appearing mid-session are discovered
//! via a recursive `notify::RecommendedWatcher` (inotify on Linux,
//! FSEvents on macOS, ReadDirectoryChangesW on Windows) — when a
//! Create/Modify event lands, the next [`SealSource::poll`] re-scans
//! the tree and starts tailing any new chain file from byte 0. A
//! one-second periodic rescan remains as a fallback for the rare
//! cases where the watcher drops or coalesces events.
//!
//! Per-chain tailing is still polled: each `seal.jsonl` is append-only
//! and the cost of `read_to_end` from the last cursor offset is
//! trivial. The watcher only influences *discovery* latency.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use orkia_shell_types::seal::SealRecord;

use crate::cursor::SealCursor;

#[derive(Debug, Clone)]
pub struct SealEvent {
    pub chain_id: String,
    pub record: SealRecord,
    /// Byte offset of the END of this record in the chain file.
    /// Saved into the cursor on successful publish.
    pub byte_end: u64,
}

/// Per-chain tail state.
struct ChainState {
    chain_id: String,
    path: PathBuf,
    offset: u64,
    last_hash: String,
}

pub struct SealSource {
    root: PathBuf,
    chains: HashMap<String, ChainState>,
    last_rescan: std::time::Instant,
    rescan_interval: std::time::Duration,
    // Filesystem watcher. Installed asynchronously on a background OS
    // thread — on macOS `FSEventStreamStart` against fresh paths under
    // `/var/folders` can block for tens of seconds, and the run loop
    // must not stall waiting for it. The `_watcher_holder` keeps the
    // notify handle alive (the install thread forwards events into the
    // local `events_rx`). Both are `Option` so the source still
    // functions if watcher installation fails — the run loop has a
    // periodic rescan fallback.
    _watcher_holder: Option<std::thread::JoinHandle<()>>,
    events_rx: Option<mpsc::Receiver<notify::Result<Event>>>,
    rescan_pending: bool,
    // Flipped true by the install thread once `watch()` succeeded.
    // Writes that land before this point produce no notify event (the
    // FSEvents stream only reports changes after it starts). Read only
    // by tests that disable the periodic rescan — the run loop doesn't
    // need it because the periodic rescan covers the install window.
    #[cfg_attr(not(test), allow(dead_code))]
    watcher_ready: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl SealSource {
    pub fn new(root: PathBuf, cursor: &SealCursor) -> Self {
        // The watcher needs the directory to exist before `watch()`
        // succeeds. Best-effort: if creation fails, the background
        // install thread will report and we fall back to polling.
        let _ = std::fs::create_dir_all(&root);
        let (forward_tx, events_rx) = mpsc::channel::<notify::Result<Event>>();
        let install_root = root.clone();
        let watcher_ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ready_flag = watcher_ready.clone();
        let watcher_holder = std::thread::Builder::new()
            .name("orkia-stream-watcher-install".into())
            .spawn(move || match install_watcher(&install_root) {
                Ok((watcher, rx)) => {
                    ready_flag.store(true, std::sync::atomic::Ordering::Release);
                    while let Ok(ev) = rx.recv() {
                        if forward_tx.send(ev).is_err() {
                            break;
                        }
                    }
                    drop(watcher);
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "seal source: notify watcher unavailable, falling back to periodic rescan",
                    );
                }
            })
            .ok();
        let mut s = Self {
            root,
            chains: HashMap::new(),
            last_rescan: std::time::Instant::now() - std::time::Duration::from_secs(3_600),
            rescan_interval: std::time::Duration::from_secs(1),
            _watcher_holder: watcher_holder,
            events_rx: Some(events_rx),
            rescan_pending: true,
            watcher_ready,
        };
        s.seed_from_cursor(cursor);
        s
    }

    fn seed_from_cursor(&mut self, cursor: &SealCursor) {
        // Walk + register everything we find. The cursor's offset for
        // a known chain is honoured; unknown chains start at byte 0.
        let mut found: Vec<(String, PathBuf)> = Vec::new();
        discover_chains(&self.root, &mut found);
        for (chain_id, path) in found {
            let (offset, last_hash) = match cursor.get(&chain_id) {
                Some(c) => (c.offset, c.last_hash.clone()),
                None => (0, String::new()),
            };
            self.chains.insert(
                chain_id.clone(),
                ChainState {
                    chain_id,
                    path,
                    offset,
                    last_hash,
                },
            );
        }
    }

    /// First-run scan; idempotent.
    pub fn initial_scan(&mut self) {
        if self.chains.is_empty() {
            let mut found = Vec::new();
            discover_chains(&self.root, &mut found);
            for (chain_id, path) in found {
                self.chains.entry(chain_id.clone()).or_insert(ChainState {
                    chain_id,
                    path,
                    offset: 0,
                    last_hash: String::new(),
                });
            }
        }
    }

    /// Drain any pending notify events; set `rescan_pending` if any
    /// Create/Modify event was observed under `root`.
    fn drain_watcher(&mut self) {
        let Some(rx) = self.events_rx.as_ref() else {
            return;
        };
        while let Ok(res) = rx.try_recv() {
            match res {
                Ok(ev)
                    if matches!(
                        ev.kind,
                        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Other
                    ) =>
                {
                    self.rescan_pending = true;
                }
                Ok(_) => { /* Access/Remove events are not actionable here */ }
                Err(e) => {
                    tracing::warn!(error = %e, "seal source: notify channel reported error");
                }
            }
        }
    }

    /// One iteration of the read loop. Re-scans either when the
    /// watcher signaled a filesystem change or, as a safety net, when
    /// the periodic timer fires. Reads any new bytes from each known
    /// chain. Each chain's records arrive in seq order; cross-chain
    /// order is not stabilised.
    pub fn poll(&mut self) -> Vec<SealEvent> {
        self.drain_watcher();
        let due_for_periodic = self.last_rescan.elapsed() >= self.rescan_interval;
        if self.rescan_pending || due_for_periodic {
            self.last_rescan = std::time::Instant::now();
            self.rescan_pending = false;
            let mut found = Vec::new();
            discover_chains(&self.root, &mut found);
            for (chain_id, path) in found {
                self.chains.entry(chain_id.clone()).or_insert(ChainState {
                    chain_id,
                    path,
                    offset: 0,
                    last_hash: String::new(),
                });
            }
        }
        let mut out = Vec::new();
        for state in self.chains.values_mut() {
            tail_chain(state, &mut out);
        }
        out
    }

    /// Test-only escape hatch: stretch the periodic rescan so a test
    /// can prove the watcher path (not the fallback timer) is what
    /// surfaced a newly-created chain.
    #[cfg(test)]
    pub fn set_rescan_interval_for_test(&mut self, d: std::time::Duration) {
        self.rescan_interval = d;
        self.last_rescan = std::time::Instant::now();
    }

    /// Test helper: list of currently-tracked chain ids.
    pub fn known_chains(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.chains.keys().cloned().collect();
        ids.sort();
        ids
    }
}

fn tail_chain(state: &mut ChainState, out: &mut Vec<SealEvent>) {
    let mut file = match std::fs::File::open(&state.path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let len = match file.metadata().map(|m| m.len()) {
        Ok(n) => n,
        Err(_) => return,
    };
    if len <= state.offset {
        // No new bytes.
        return;
    }
    if file.seek(SeekFrom::Start(state.offset)).is_err() {
        return;
    }
    let mut reader = BufReader::new(file);
    let mut bytes_consumed = state.offset;
    loop {
        let mut line: Vec<u8> = Vec::new();
        let n = match reader.read_until(b'\n', &mut line) {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break; // EOF
        }
        // A line without a trailing newline is a record still being appended.
        // Stop here WITHOUT advancing past it so the next poll re-reads the
        // complete record — the old code consumed the partial line, failed to
        // parse it, and advanced the cursor mid-record, desyncing the chain
        // forever (BUG-040).
        if !line.ends_with(b"\n") {
            break;
        }
        // Exact byte accounting (includes the newline) — no `+1` guess.
        bytes_consumed += n as u64;
        let text = match std::str::from_utf8(&line) {
            Ok(t) => t.trim(),
            Err(_) => continue,
        };
        if text.is_empty() {
            continue;
        }
        match serde_json::from_str::<SealRecord>(text) {
            Ok(record) => {
                state.last_hash = record.hash.clone();
                out.push(SealEvent {
                    chain_id: state.chain_id.clone(),
                    record,
                    byte_end: bytes_consumed,
                });
            }
            Err(e) => {
                tracing::warn!(
                    chain = %state.chain_id,
                    error = %e,
                    "orkia-stream: skipping malformed seal record",
                );
            }
        }
    }
    state.offset = bytes_consumed;
}

/// Install a recursive notify watcher on `root` and return both the
/// watcher (kept alive by the caller) and the receiver that surfaces
/// every event.
fn install_watcher(
    root: &Path,
) -> notify::Result<(RecommendedWatcher, mpsc::Receiver<notify::Result<Event>>)> {
    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        // If the receiver is dropped (source torn down) we silently
        // discard. The watcher itself is dropped right after, so this
        // window is short.
        let _ = tx.send(res);
    })?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    Ok((watcher, rx))
}

/// Walk `root` and collect every `seal.jsonl` file along with its
/// derived `chain_id`.
fn discover_chains(root: &Path, out: &mut Vec<(String, PathBuf)>) {
    // workspace chain
    let workspace = root.join("workspace").join("seal.jsonl");
    if workspace.is_file() {
        out.push(("workspace".to_string(), workspace));
    }

    // project chains: <root>/projects/<name>/seal.jsonl
    if let Ok(entries) = std::fs::read_dir(root.join("projects")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let chain_file = path.join("seal.jsonl");
            if chain_file.is_file() {
                let chain_id = entry.file_name().to_string_lossy().to_string();
                out.push((chain_id, chain_file));
            }
        }
    }

    // job chains: <root>/agents/<agent>/jobs/<id>/seal.jsonl
    if let Ok(agents) = std::fs::read_dir(root.join("agents")) {
        for agent_entry in agents.flatten() {
            let agent_path = agent_entry.path();
            if !agent_path.is_dir() {
                continue;
            }
            let agent_name = agent_entry.file_name().to_string_lossy().to_string();
            let jobs_dir = agent_path.join("jobs");
            if let Ok(jobs) = std::fs::read_dir(&jobs_dir) {
                for job_entry in jobs.flatten() {
                    let job_path = job_entry.path();
                    if !job_path.is_dir() {
                        continue;
                    }
                    let chain_file = job_path.join("seal.jsonl");
                    if chain_file.is_file() {
                        let job_id = job_entry.file_name().to_string_lossy().to_string();
                        let chain_id = format!("job:{agent_name}/{job_id}");
                        out.push((chain_id, chain_file));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_record(path: &Path, record: &SealRecord) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        let line = serde_json::to_string(record).unwrap();
        writeln!(f, "{line}").unwrap();
    }

    fn rec(seq: u64, prev: &str, hash: &str) -> SealRecord {
        SealRecord {
            seq,
            timestamp: "2026-05-26T00:00:00+00:00".into(),
            event_type: "rfc.create".into(),
            detail: serde_json::json!({"scope": "public"}),
            hash: hash.into(),
            prev_hash: prev.into(),
            rfc_id: None,
        }
    }

    #[test]
    fn discovers_workspace_project_and_job_chains() {
        let dir = tempdir().unwrap();
        write_record(&dir.path().join("workspace/seal.jsonl"), &rec(0, "0", "a"));
        write_record(
            &dir.path().join("projects/orkia-shell/seal.jsonl"),
            &rec(0, "0", "b"),
        );
        write_record(
            &dir.path().join("agents/faye/jobs/1/seal.jsonl"),
            &rec(0, "0", "c"),
        );

        let cursor = SealCursor::default();
        let mut src = SealSource::new(dir.path().to_path_buf(), &cursor);
        src.initial_scan();
        let chains = src.known_chains();
        assert!(chains.contains(&"workspace".to_string()));
        assert!(chains.contains(&"orkia-shell".to_string()));
        assert!(chains.contains(&"job:faye/1".to_string()));
    }

    #[test]
    fn tails_appended_records() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("projects/p/seal.jsonl");
        write_record(&path, &rec(0, "0", "a"));
        let cursor = SealCursor::default();
        let mut src = SealSource::new(dir.path().to_path_buf(), &cursor);
        src.initial_scan();

        let first = src.poll();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].record.seq, 0);

        write_record(&path, &rec(1, "a", "b"));
        write_record(&path, &rec(2, "b", "c"));
        let next = src.poll();
        assert_eq!(next.len(), 2);
        assert_eq!(next[0].record.seq, 1);
        assert_eq!(next[1].record.seq, 2);
    }

    #[test]
    fn watcher_picks_up_new_chain_without_periodic_rescan() {
        // Stretch the periodic rescan to effectively-never so this
        // test can only pass via the notify-driven discovery path.
        let dir = tempdir().unwrap();
        let cursor = SealCursor::default();
        let mut src = SealSource::new(dir.path().to_path_buf(), &cursor);
        src.initial_scan();
        assert!(src.known_chains().is_empty());

        // Drain the initial `rescan_pending=true` flag set by `new()`
        // before stretching the rescan interval; otherwise the first
        // poll would discover via that flag, not via the watcher.
        let _ = src.poll();
        src.set_rescan_interval_for_test(std::time::Duration::from_secs(3_600));

        // The watcher installs asynchronously; a write that lands
        // before the FSEvents stream starts produces no event, ever.
        // With the periodic net stretched above, that's a guaranteed
        // miss — so wait for the install thread to flip the flag.
        // Generous deadline: FSEventStreamStart against fresh paths
        // under /var/folders can block for tens of seconds (see the
        // struct comment), which is exactly why install is async.
        let install_deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        while !src.watcher_ready.load(std::sync::atomic::Ordering::Acquire) {
            assert!(
                std::time::Instant::now() < install_deadline,
                "notify watcher never installed"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        // Write a new chain only now that the watcher is live.
        write_record(
            &dir.path().join("projects/late/seal.jsonl"),
            &rec(0, "0", "z"),
        );

        // Poll on a tight loop until the watcher fires or we time out.
        // FSEvents on macOS can take ~250ms–2s to deliver; inotify on
        // Linux is sub-100ms. 5s is generous enough for either.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut events = Vec::new();
        while std::time::Instant::now() < deadline {
            events = src.poll();
            if !events.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(events.len(), 1, "expected watcher to surface the new chain");
        assert_eq!(events[0].chain_id, "late");
    }

    #[test]
    fn cursor_resume_skips_already_seen_records() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("projects/p/seal.jsonl");
        write_record(&path, &rec(0, "0", "a"));
        write_record(&path, &rec(1, "a", "b"));
        let offset = std::fs::metadata(&path).unwrap().len();

        let mut cursor = SealCursor::default();
        cursor.set("p", offset, "b".into());

        let mut src = SealSource::new(dir.path().to_path_buf(), &cursor);
        src.initial_scan();
        let polled = src.poll();
        assert!(
            polled.is_empty(),
            "cursor at EOF must skip existing records"
        );

        write_record(&path, &rec(2, "b", "c"));
        let after = src.poll();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].record.seq, 2);
    }
}
