// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Engine startup configuration. Defaults are exactly the values the POC
//! validated (60fps p50, 7.5ms keystroke p95). Deserializable from a single
//! TOML file by the application; `Default` is the fallback.
//!
//! Note: the performance-critical extractor cadence and per-block grid
//! geometry are intentionally compile-time constants in `blocks.rs`, not
//! config fields. They are documented tripwires in `ARCHITECTURE-TERMINAL.md`
//! (changing them regresses the validated baseline); exposing them as runtime
//! knobs would invite exactly the regressions the baseline guards against.
//! Only safe-to-tune startup parameters live here.

use std::time::Duration;

use serde::Deserialize;

use crate::blocks::{ApcCallback, Osc133Callback};

/// Screen-mode snapshot extractor tuning. Lives on `EngineConfig` so
/// the publish cadence is a config-file knob, not a code change. The
/// chosen model is "Option C — coalesced publish on a per-thread time
/// budget", with a 16 ms / 60 Hz justification.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(default)]
pub struct ScreenExtractConfig {
    /// Minimum interval between `ScreenSnapshot` publishes from the
    /// engine reader thread. Default is 16 ms (matches the block-mode
    /// extractor's `EXTRACT_MIN` at `blocks.rs:40`, which targets
    /// ≤ 60 Hz). EOF publishes ignore this budget; the rendering
    /// cost is paid at most once per `min_publish` window.
    ///
    /// Serialised as milliseconds for human-readable TOML configs:
    /// `screen_extract = { min_publish_ms = 16 }`.
    #[serde(
        default = "default_min_publish_ms",
        rename = "min_publish_ms",
        deserialize_with = "deserialize_min_publish_ms"
    )]
    pub min_publish: Duration,
}

fn default_min_publish_ms() -> Duration {
    Duration::from_millis(16)
}

fn deserialize_min_publish_ms<'de, D>(de: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;
    let ms = u64::deserialize(de)?;
    Ok(Duration::from_millis(ms))
}

impl Default for ScreenExtractConfig {
    fn default() -> Self {
        Self {
            min_publish: default_min_publish_ms(),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    /// Initial PTY width in columns.
    pub init_cols: usize,
    /// Initial PTY height in rows.
    pub init_rows: usize,
    /// PTY reader buffer size in bytes (one allocation at thread start).
    pub read_buf_bytes: usize,
    /// Command to run. `None` = interactive `$SHELL` with OSC-133 hooks.
    #[serde(skip)]
    pub cmd: Option<String>,
    /// Arguments for `cmd`.
    #[serde(skip)]
    pub args: Vec<String>,
    /// Extra environment variables for the spawned process. Applied after
    /// parent env inheritance so they take precedence.
    #[serde(skip)]
    pub env: Vec<(String, String)>,
    /// Working directory for the spawned process. `None` inherits the
    /// orkia process's current_dir (`std::env::current_dir()`). Brush
    /// tracks its own cwd in-process and does NOT call
    /// `set_current_dir`, so background shell jobs must pass
    /// `Some(brush.cwd())` here to honour the user's expected cwd.
    #[serde(skip)]
    pub cwd: Option<std::path::PathBuf>,
    /// Optional OSC 133 marker listener wired into the `BlockParser`
    /// before the reader thread starts. Lets the protocol layer in
    /// `orkia-shell` surface A/B/C/D markers as unified events
    /// without re-parsing the byte stream. `None` keeps the
    /// BlockParser hot-path identical to V1.
    #[serde(skip)]
    pub on_osc133: Option<Osc133Callback>,
    /// Optional APC sequence listener (V2 Orkia protocol). Fires
    /// once per complete `ESC _ ... ESC \\` sequence with the
    /// payload bytes. `None` skips the APC state machine entirely.
    #[serde(skip)]
    pub on_apc: Option<ApcCallback>,
    /// Tuning for the screen-mode snapshot extractor. The reader
    /// thread publishes a fresh `ScreenSnapshot` at most every
    /// `min_publish` while in `InlineFull` / `AltScreenFull` mode.
    /// Default `min_publish = 16 ms` (≤ 60 Hz).
    #[serde(default)]
    pub screen_extract: ScreenExtractConfig,
    /// The engine hosts a single long-lived full-screen program (an
    /// agent TUI: claude / codex / gemini) rather than a shell. When
    /// `true` the display mode is never `BlockView`, so the reader
    /// always advances the alacritty grid and a (re-)attach can
    /// reconstruct the screen from `render_visible_snapshot`. Agents
    /// never emit the OSC-133 C/D command markers a shell uses, so
    /// without this the grid would never be advanced for them. Plain
    /// shell jobs and the brush session leave this `false`.
    #[serde(skip)]
    pub persistent_program: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            init_cols: 120,
            init_rows: 42,
            read_buf_bytes: 8192,
            cmd: None,
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
            on_osc133: None,
            on_apc: None,
            screen_extract: ScreenExtractConfig::default(),
            persistent_program: false,
        }
    }
}

impl EngineConfig {
    /// Load from a TOML file, falling back to `Default` if the path is absent
    /// or unreadable. Parse errors are returned to the caller.
    pub fn from_toml_path(path: &std::path::Path) -> Result<Self, toml_error::ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(s) => toml_error::parse(&s),
            Err(_) => Ok(Self::default()),
        }
    }
}

/// Thin error wrapper so config parsing has a typed boundary without leaking
/// the `toml` crate's error type across the public API.
pub mod toml_error {
    #[derive(thiserror::Error, Debug)]
    #[error("config parse error: {0}")]
    pub struct ConfigError(String);

    pub fn parse(s: &str) -> Result<super::EngineConfig, ConfigError> {
        toml::from_str(s).map_err(|e| ConfigError(e.to_string()))
    }
}
