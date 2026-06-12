// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Assertions over files in the session data directory.

use std::path::{Path, PathBuf};

use crate::error::{AssertKind, HarnessError};

pub struct FileAssert<'a> {
    data_dir: Option<&'a Path>,
}

impl<'a> FileAssert<'a> {
    pub fn with_data_dir(data_dir: &'a Path) -> Self {
        Self {
            data_dir: Some(data_dir),
        }
    }

    /// No shell was booted — assertions return a clear error.
    pub fn detached() -> Self {
        Self { data_dir: None }
    }

    pub fn exists(self, relative_path: &str) -> crate::Result<()> {
        let path = self.resolve(relative_path)?;
        if !path.exists() {
            let state = self.tree_dump();
            return Err(HarnessError::assertion(
                format!("file_exists({}): not found", path.display()),
                AssertKind::File,
                state,
            ));
        }
        Ok(())
    }

    pub fn contains(self, relative_path: &str, needle: &str) -> crate::Result<()> {
        let path = self.resolve(relative_path)?;
        let content = std::fs::read_to_string(&path)?;
        if !content.contains(needle) {
            let state = format!(
                "--- {} (first 1000 chars) ---\n{}",
                path.display(),
                content.chars().take(1000).collect::<String>()
            );
            return Err(HarnessError::assertion(
                format!("file_contains({}, {needle:?}): not found", path.display()),
                AssertKind::File,
                state,
            ));
        }
        Ok(())
    }

    /// Assert a file does NOT contain `needle`. Unlike output-based
    /// `not_contains`, this reads the file fresh — no terminal-scrollback
    /// false positives (cf. the F101/F402 command-echo trap).
    pub fn not_contains(self, relative_path: &str, needle: &str) -> crate::Result<()> {
        let path = self.resolve(relative_path)?;
        let content = std::fs::read_to_string(&path)?;
        if content.contains(needle) {
            let state = format!(
                "--- {} (first 1000 chars) ---\n{}",
                path.display(),
                content.chars().take(1000).collect::<String>()
            );
            return Err(HarnessError::assertion(
                format!(
                    "file_not_contains({}, {needle:?}): unexpectedly present",
                    path.display()
                ),
                AssertKind::File,
                state,
            ));
        }
        Ok(())
    }

    pub fn empty(self, relative_path: &str) -> crate::Result<()> {
        let path = self.resolve(relative_path)?;
        let meta = std::fs::metadata(&path)?;
        if meta.len() != 0 {
            return Err(HarnessError::assertion(
                format!("file_empty({}): {} bytes", path.display(), meta.len()),
                AssertKind::File,
                String::new(),
            ));
        }
        Ok(())
    }

    pub fn jsonl_count(self, relative_path: &str, expected: usize) -> crate::Result<()> {
        let path = self.resolve(relative_path)?;
        let content = std::fs::read_to_string(&path)?;
        let got = content.lines().filter(|l| !l.trim().is_empty()).count();
        if got != expected {
            let state = format!(
                "--- {} (first 800 chars) ---\n{}",
                path.display(),
                content.chars().take(800).collect::<String>()
            );
            return Err(HarnessError::assertion(
                format!(
                    "jsonl_count({}): expected {expected}, got {got}",
                    path.display()
                ),
                AssertKind::File,
                state,
            ));
        }
        Ok(())
    }

    /// Count files under `data_dir` matching a `<subdir>/<prefix>*<suffix>` shape.
    /// Pattern grammar is intentionally narrow — one `*` wildcard supported.
    pub fn matches_glob(self, pattern: &str, expected_count: usize) -> crate::Result<()> {
        let matches = self.find_glob(pattern)?;
        if matches.len() != expected_count {
            let names: Vec<String> = matches.iter().map(|p| p.display().to_string()).collect();
            let state = format!(
                "--- pattern {pattern} ---\nmatched files:\n  {}\n--- data_dir tree (4 levels) ---\n{}",
                if names.is_empty() {
                    "(none)".to_string()
                } else {
                    names.join("\n  ")
                },
                self.tree_dump()
            );
            return Err(HarnessError::assertion(
                format!(
                    "matches_glob({pattern}): expected {expected_count}, got {}",
                    matches.len()
                ),
                AssertKind::File,
                state,
            ));
        }
        Ok(())
    }

    fn resolve(&self, relative_path: &str) -> crate::Result<PathBuf> {
        let root = self.data_dir.ok_or(HarnessError::NotImplemented {
            what: "FileAssert: shell not booted",
        })?;
        Ok(root.join(relative_path))
    }

    fn find_glob(&self, pattern: &str) -> crate::Result<Vec<PathBuf>> {
        let root = self.data_dir.ok_or(HarnessError::NotImplemented {
            what: "FileAssert: shell not booted",
        })?;
        let (dir_part, file_pattern) = match pattern.rsplit_once('/') {
            Some((d, f)) => (root.join(d), f.to_string()),
            None => (root.to_path_buf(), pattern.to_string()),
        };
        let (prefix, suffix) = match file_pattern.split_once('*') {
            Some((p, s)) => (p.to_string(), s.to_string()),
            None => (file_pattern.clone(), String::new()),
        };
        let entries = std::fs::read_dir(&dir_part);
        let entries = match entries {
            Ok(e) => e,
            // Missing directory = zero matches, not an error.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with(&prefix) && name.ends_with(&suffix) {
                out.push(entry.path());
            }
        }
        out.sort();
        Ok(out)
    }

    /// Best-effort recursive listing of `data_dir`, 4 levels deep,
    /// for failure diagnostics. No-op if no data_dir.
    fn tree_dump(&self) -> String {
        let Some(root) = self.data_dir else {
            return String::new();
        };
        let mut out = String::new();
        walk(root, 0, 4, &mut out);
        out
    }
}

fn walk(p: &Path, depth: usize, max: usize, out: &mut String) {
    if depth > max {
        return;
    }
    let Ok(entries) = std::fs::read_dir(p) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        for _ in 0..depth {
            out.push_str("  ");
        }
        out.push_str(path.file_name().and_then(|n| n.to_str()).unwrap_or("?"));
        if path.is_dir() {
            out.push('/');
        }
        out.push('\n');
        if path.is_dir() {
            walk(&path, depth + 1, max, out);
        }
    }
}
