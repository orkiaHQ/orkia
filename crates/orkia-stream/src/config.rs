// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! `StreamConfig` — defaults, env vars, optional `~/.orkia/config.toml [stream]`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::errors::StreamError;

pub const DEFAULT_BATCH_MAX_EVENTS: usize = 50;
pub const DEFAULT_BATCH_MAX_BYTES: usize = 262_144;
pub const DEFAULT_BATCH_FLUSH_MS: u64 = 5_000;

pub const ENV_DISABLED: &str = "ORKIA_STREAM_DISABLED";
pub const ENV_BATCH_MAX_EVENTS: &str = "ORKIA_STREAM_BATCH_MAX_EVENTS";
pub const ENV_FLUSH_MS: &str = "ORKIA_STREAM_FLUSH_MS";

#[derive(Debug, Clone)]
pub struct StreamConfig {
    pub backend_url: String,
    /// Root of all SealChain `.jsonl` files (usually `~/.orkia`).
    pub seal_root: PathBuf,
    /// Where cursor + paused.flag live (usually `~/.orkia/state/stream/`).
    pub state_dir: PathBuf,
    pub batch_max_events: usize,
    pub batch_max_bytes: usize,
    pub batch_flush_interval: Duration,
    pub disabled: bool,
}

impl StreamConfig {
    /// Build a config from defaults + optional config file + env vars.
    ///
    /// `orkia_home` is the base of `~/.orkia` (passed in so tests can
    /// redirect away from the real home dir).
    pub fn from_env(orkia_home: &Path) -> Result<Self, StreamError> {
        let backend_url = orkia_shell_types::backend::resolve_backend_url(None)?;
        let seal_root = orkia_home.to_path_buf();
        let state_dir = orkia_home.join("state").join("stream");

        let mut cfg = StreamConfig {
            backend_url,
            seal_root,
            state_dir,
            batch_max_events: DEFAULT_BATCH_MAX_EVENTS,
            batch_max_bytes: DEFAULT_BATCH_MAX_BYTES,
            batch_flush_interval: Duration::from_millis(DEFAULT_BATCH_FLUSH_MS),
            disabled: false,
        };

        // Optional config file under ~/.orkia/config.toml — [stream] section.
        let cfg_path = orkia_home.join("config.toml");
        if cfg_path.exists() {
            apply_config_file(&mut cfg, &cfg_path)?;
        }

        // Env overrides.
        if std::env::var(ENV_DISABLED).is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true")) {
            cfg.disabled = true;
        }
        if let Ok(s) = std::env::var(ENV_BATCH_MAX_EVENTS)
            && let Ok(n) = s.parse::<usize>()
            && n > 0
        {
            cfg.batch_max_events = n;
        }
        if let Ok(s) = std::env::var(ENV_FLUSH_MS)
            && let Ok(n) = s.parse::<u64>()
            && n > 0
        {
            cfg.batch_flush_interval = Duration::from_millis(n);
        }

        Ok(cfg)
    }

    pub fn paused_flag_path(&self) -> PathBuf {
        self.state_dir.join("paused.flag")
    }

    pub fn paused_flag_present(&self) -> bool {
        self.paused_flag_path().exists()
    }
}

fn apply_config_file(cfg: &mut StreamConfig, path: &Path) -> Result<(), StreamError> {
    let text = std::fs::read_to_string(path)?;
    // `toml::Table` is the DOCUMENT parser; `toml::Value::from_str`
    // (toml v1) parses a single inline value and rejects any real
    // config file ("expected nothing" at the first comment).
    let parsed: toml::Table = match text.parse() {
        Ok(v) => v,
        Err(e) => return Err(StreamError::Config(format!("config.toml parse: {e}"))),
    };
    let stream = match parsed.get("stream") {
        Some(s) => s,
        None => return Ok(()),
    };
    if let Some(n) = stream.get("batch_max_events").and_then(|v| v.as_integer())
        && n > 0
    {
        cfg.batch_max_events = n as usize;
    }
    if let Some(n) = stream
        .get("batch_flush_interval_ms")
        .and_then(|v| v.as_integer())
        && n > 0
    {
        cfg.batch_flush_interval = Duration::from_millis(n as u64);
    }
    if let Some(b) = stream.get("disabled").and_then(|v| v.as_bool()) {
        cfg.disabled = b;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_cfg() -> StreamConfig {
        StreamConfig {
            backend_url: "https://example.com".into(),
            seal_root: PathBuf::from("/tmp"),
            state_dir: PathBuf::from("/tmp"),
            batch_max_events: DEFAULT_BATCH_MAX_EVENTS,
            batch_max_bytes: DEFAULT_BATCH_MAX_BYTES,
            batch_flush_interval: Duration::from_millis(DEFAULT_BATCH_FLUSH_MS),
            disabled: false,
        }
    }

    /// Regression: a real `config.toml` (comments + unrelated keys, the
    /// shape `orkia setup` scaffolds) must parse — `toml::Value::from_str`
    /// used to reject the whole document at the first comment.
    #[test]
    fn scaffolded_config_with_comments_parses() {
        let dir = std::env::temp_dir().join("orkia-stream-cfg-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "# Orkia Shell Configuration\nload_bashrc = true\n\n[stream]\nbatch_max_events = 7\n",
        )
        .unwrap();
        let mut cfg = base_cfg();
        apply_config_file(&mut cfg, &path).unwrap();
        assert_eq!(cfg.batch_max_events, 7);
    }

    #[test]
    fn config_without_stream_section_is_noop() {
        let dir = std::env::temp_dir().join("orkia-stream-cfg-test2");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "# comment only\nload_bashrc = true\n").unwrap();
        let mut cfg = base_cfg();
        apply_config_file(&mut cfg, &path).unwrap();
        assert_eq!(cfg.batch_max_events, DEFAULT_BATCH_MAX_EVENTS);
    }
}
