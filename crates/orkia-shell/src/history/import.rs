// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use std::path::{Path, PathBuf};

pub(super) struct Imported {
    pub line: String,
    pub source: &'static str,
}

pub(super) fn collect_system_history(limit: usize) -> Vec<Imported> {
    let mut out: Vec<Imported> = Vec::new();

    if let Some(path) = zsh_history_path() {
        for line in read_zsh_history(&path) {
            out.push(Imported {
                line,
                source: "zsh",
            });
        }
    }
    if let Some(path) = bash_history_path() {
        for line in read_bash_history(&path) {
            out.push(Imported {
                line,
                source: "bash",
            });
        }
    }

    dedup_consecutive(&mut out);

    if out.len() > limit {
        let drop = out.len() - limit;
        out.drain(0..drop);
    }
    out
}

fn dedup_consecutive(entries: &mut Vec<Imported>) {
    entries.dedup_by(|a, b| a.line == b.line);
}

fn zsh_history_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("HISTFILE") {
        let candidate = PathBuf::from(p);
        if candidate
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n.contains("zsh"))
        {
            return Some(candidate);
        }
    }
    home_dir().map(|h| h.join(".zsh_history"))
}

fn bash_history_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("HISTFILE") {
        let candidate = PathBuf::from(p);
        if candidate
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n.contains("bash"))
        {
            return Some(candidate);
        }
    }
    home_dir().map(|h| h.join(".bash_history"))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn read_zsh_history(path: &Path) -> Vec<String> {
    let Ok(raw) = read_lossy(path) else {
        return Vec::new();
    };

    let mut out: Vec<String> = Vec::new();
    let mut pending: Option<String> = None;

    for raw_line in raw.lines() {
        let line = if let Some(prev) = pending.take() {
            // Continuation of a multi-line command.
            format!("{prev}\n{raw_line}")
        } else {
            raw_line.to_string()
        };

        // Detect continuation: zsh escapes newlines with a trailing backslash.
        if line.ends_with('\\') {
            let mut without_backslash = line;
            without_backslash.pop();
            pending = Some(without_backslash);
            continue;
        }

        let cmd = strip_zsh_extended(&line);
        let trimmed = cmd.trim();
        if !trimmed.is_empty() {
            out.push(trimmed.to_string());
        }
    }

    if let Some(remainder) = pending {
        let cmd = strip_zsh_extended(&remainder);
        let trimmed = cmd.trim();
        if !trimmed.is_empty() {
            out.push(trimmed.to_string());
        }
    }
    out
}

/// Strip the zsh extended-history prefix `: <timestamp>:<duration>;` if present.
fn strip_zsh_extended(line: &str) -> &str {
    let Some(rest) = line.strip_prefix(": ") else {
        return line;
    };
    match rest.find(';') {
        Some(idx) => &rest[idx + 1..],
        None => line,
    }
}

fn read_bash_history(path: &Path) -> Vec<String> {
    let Ok(raw) = read_lossy(path) else {
        return Vec::new();
    };
    raw.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.to_string())
        .collect()
}

fn read_lossy(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
pub(super) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn parses_zsh_extended_format() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(".zsh_history");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, ": 1700000000:0;ls -la").unwrap();
        writeln!(f, ": 1700000100:5;cargo build").unwrap();

        let lines = read_zsh_history(&path);
        assert_eq!(lines, vec!["ls -la", "cargo build"]);
    }

    #[test]
    fn parses_zsh_simple_lines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(".zsh_history");
        std::fs::write(&path, "ls\npwd\n").unwrap();

        assert_eq!(read_zsh_history(&path), vec!["ls", "pwd"]);
    }

    #[test]
    fn handles_zsh_multiline_continuation() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(".zsh_history");
        std::fs::write(&path, ": 1700000000:0;echo foo \\\nbar\n").unwrap();

        let lines = read_zsh_history(&path);
        assert_eq!(lines, vec!["echo foo \nbar"]);
    }

    #[test]
    fn bash_skips_timestamp_comments() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(".bash_history");
        std::fs::write(&path, "#1700000000\nls\n#1700000100\ncd /tmp\n").unwrap();

        assert_eq!(read_bash_history(&path), vec!["ls", "cd /tmp"]);
    }

    #[test]
    fn missing_file_returns_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does-not-exist");
        assert!(read_zsh_history(&path).is_empty());
        assert!(read_bash_history(&path).is_empty());
    }

    #[test]
    fn collect_respects_limit_keeping_latest() {
        let _guard = super::ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // Use a fake HOME with a zsh history of 10 entries, limit=3 -> last 3.
        let dir = tempdir().unwrap();
        let history = (0..10)
            .map(|i| format!("cmd{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.path().join(".zsh_history"), history).unwrap();

        let prev_home = std::env::var_os("HOME");
        let prev_histfile = std::env::var_os("HISTFILE");
        // SAFETY: Process-wide env mutation; serialized on `ENV_LOCK`
        // (acquired above) so no other thread reads/writes env
        // concurrently within this test.
        unsafe {
            std::env::set_var("HOME", dir.path());
            std::env::remove_var("HISTFILE");
        }

        let collected = collect_system_history(3);
        let lines: Vec<&str> = collected.iter().map(|i| i.line.as_str()).collect();
        assert_eq!(lines, vec!["cmd7", "cmd8", "cmd9"]);

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
    fn dedup_consecutive_duplicates() {
        let mut v = vec![
            Imported {
                line: "ls".into(),
                source: "zsh",
            },
            Imported {
                line: "ls".into(),
                source: "zsh",
            },
            Imported {
                line: "pwd".into(),
                source: "zsh",
            },
            Imported {
                line: "pwd".into(),
                source: "bash",
            },
        ];
        dedup_consecutive(&mut v);
        let lines: Vec<&str> = v.iter().map(|i| i.line.as_str()).collect();
        assert_eq!(lines, vec!["ls", "pwd"]);
    }
}
