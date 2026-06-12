// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

mod import;

use orkia_shell_types::{HistoryEntry, HistoryType};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

const MAX_ENTRIES: usize = 10_000;
const IMPORT_LIMIT: usize = 500;

pub struct History {
    entries: Vec<HistoryEntry>,
    file_path: PathBuf,
    next_seq: u64,
}

impl History {
    pub fn new(data_dir: &Path) -> Self {
        let _ = std::fs::create_dir_all(data_dir);
        let file_path = data_dir.join("history.jsonl");
        let is_first_launch = !file_path.exists();
        let entries = Self::load(&file_path);
        let next_seq = entries.last().map(|e| e.seq + 1).unwrap_or(1);
        let mut history = Self {
            entries,
            file_path,
            next_seq,
        };
        if is_first_launch {
            history.seed_from_system_shells();
        }
        history
    }

    fn seed_from_system_shells(&mut self) {
        for imported in import::collect_system_history(IMPORT_LIMIT) {
            self.push_typed(
                HistoryType::Shell,
                &imported.line,
                Some(format!("imported:{}", imported.source)),
                None,
            );
        }
    }

    /// Append a typed entry. The caller fills in line / agent / job_id; this
    /// assigns the sequence number and persists to JSONL.
    pub fn push_typed(
        &mut self,
        entry_type: HistoryType,
        line: &str,
        agent: Option<String>,
        job_id: Option<u32>,
    ) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        if self
            .entries
            .last()
            .is_some_and(|e| e.entry_type == entry_type && e.line == trimmed)
        {
            return;
        }
        let mut entry = HistoryEntry::new(self.next_seq, entry_type, trimmed);
        entry.agent = agent;
        entry.job_id = job_id;
        self.next_seq += 1;

        self.append_line(&entry);
        self.entries.push(entry);

        if self.entries.len() > MAX_ENTRIES {
            let trim = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(0..trim);
            self.rewrite_all();
        }
    }

    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    /// Load persisted entries from `<data_dir>/history.jsonl` — the on-disk
    /// mirror that `push_typed` keeps in sync. Shared by the in-memory store
    /// and the migrated `history` Command, which reads this mirror via
    /// `CommandCtx.data_dir` rather than snapshotting the (up-to-10k) in-memory
    pub fn load_entries(data_dir: &Path) -> Vec<HistoryEntry> {
        Self::load(&data_dir.join("history.jsonl"))
    }

    pub fn search(&self, query: &str) -> Vec<&HistoryEntry> {
        self.entries
            .iter()
            .rev()
            .filter(|e| e.line.contains(query))
            .take(20)
            .collect()
    }

    /// Return the last `limit` entries that satisfy `pred`, in chronological order.
    pub fn filter<F>(&self, limit: usize, pred: F) -> Vec<&HistoryEntry>
    where
        F: Fn(&HistoryEntry) -> bool,
    {
        let mut buf: Vec<&HistoryEntry> = self
            .entries
            .iter()
            .rev()
            .filter(|e| pred(e))
            .take(limit)
            .collect();
        buf.reverse();
        buf
    }

    fn load(path: &Path) -> Vec<HistoryEntry> {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        raw.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<HistoryEntry>(l).ok())
            .collect()
    }

    fn append_line(&self, entry: &HistoryEntry) {
        let Ok(line) = serde_json::to_string(entry) else {
            return;
        };
        let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file_path)
        else {
            return;
        };
        let _ = writeln!(file, "{line}");
    }

    fn rewrite_all(&self) {
        let lines: Vec<String> = self
            .entries
            .iter()
            .filter_map(|e| serde_json::to_string(e).ok())
            .collect();
        let body = lines.join("\n") + "\n";
        let _ = std::fs::write(&self.file_path, body);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn with_fake_home<F: FnOnce()>(home: &Path, f: F) {
        let _guard = super::import::ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev_home = std::env::var_os("HOME");
        let prev_histfile = std::env::var_os("HISTFILE");
        // SAFETY: Mutating process-wide env is unsafe under the
        // Rust 2024 edition. We serialize all such mutations on
        // `ENV_LOCK` (acquired above) so no other thread reads/writes
        // env concurrently for the duration of this test.
        unsafe {
            std::env::set_var("HOME", home);
            std::env::remove_var("HISTFILE");
        }
        f();
        // SAFETY: Same `ENV_LOCK` guard remains held while we restore
        // the previous env state.
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            if let Some(v) = prev_histfile {
                std::env::set_var("HISTFILE", v);
            }
        }
    }

    #[test]
    fn seeds_on_first_launch_only() {
        let home = tempdir().unwrap();
        let data = tempdir().unwrap();
        std::fs::write(home.path().join(".zsh_history"), "alpha\nbeta\n").unwrap();

        with_fake_home(home.path(), || {
            let h1 = History::new(data.path());
            let lines: Vec<&str> = h1.entries().iter().map(|e| e.line.as_str()).collect();
            assert_eq!(lines, vec!["alpha", "beta"]);
            assert!(
                h1.entries()
                    .iter()
                    .all(|e| e.agent.as_deref() == Some("imported:zsh"))
            );

            // Extend the source file, then reopen — no re-import should happen.
            std::fs::write(home.path().join(".zsh_history"), "alpha\nbeta\ngamma\n").unwrap();
            let h2 = History::new(data.path());
            let lines2: Vec<&str> = h2.entries().iter().map(|e| e.line.as_str()).collect();
            assert_eq!(lines2, vec!["alpha", "beta"]);
        });
    }
}
