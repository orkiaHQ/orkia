// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! YAML schema shared between the harness and the `orkia-fake-agent`
//! binary.
//!
//! A `AgentScript` is a deterministic, reproducible recipe of what a
//! fake TUI agent should do: print text, emit OSC-133 markers, call a
//! provider hook (which goes through the real `orkia bridge`), block
//! waiting for an input byte (simulating "waiting for user approval"),
//! sleep, exit with a status.
//!
//! The harness composes scripts in Rust (see `ScriptedAgentBuilder`)
//! and serialises them to YAML on disk; the agent binary reads the
//! YAML at startup. Keeping the schema in a shared crate guarantees
//! both sides agree byte-for-byte on the wire format.

use serde::{Deserialize, Serialize};

/// One agent scenario. Steps run sequentially.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AgentScript {
    /// Optional human-readable name for diagnostics.
    #[serde(default)]
    pub name: Option<String>,
    /// Whether to put the TTY into raw mode for the duration of the
    /// script. Real TUI agents (Claude, Codex) do this; tests that
    /// need to validate raw-mode forwarding should set true.
    #[serde(default)]
    pub raw_mode: bool,
    /// Steps in execution order.
    #[serde(default)]
    pub steps: Vec<ScriptStep>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScriptStep {
    /// Print arbitrary text to stdout. `\n` permitted.
    Print { text: String },
    /// Emit one of the OSC-133 prompt-marker sequences:
    ///   * `prompt_start` → `ESC ] 133 ; A BEL`
    ///   * `command_start` → `ESC ] 133 ; B BEL`
    ///   * `command_output` → `ESC ] 133 ; C BEL`
    ///   * `command_end` → `ESC ] 133 ; D BEL` (optional exit code)
    Osc133 {
        marker: Osc133Marker,
        #[serde(default)]
        exit_code: Option<i32>,
    },
    /// Invoke a provider hook by exec'ing `orkia bridge --source <source>`
    /// with the given JSON body on stdin. `orkia` env vars (HOME,
    /// ORKIA_JOB_ID, ORKIA_AGENT_NAME) are propagated automatically.
    Hook {
        source: String,
        /// Free-form JSON object — the bridge accepts any payload and
        /// forwards `event`, `tool_name`, etc. into the envelope.
        payload: serde_json::Value,
    },
    /// Read bytes from stdin until either `bytes` worth or `until` is
    /// seen. Used to simulate "agent waits for the user's approval
    /// keystroke" in tests. Times out after `timeout_ms` (default 5_000).
    ///
    /// The agent blocks in `poll(2)` while waiting — so the process is
    /// observably parked on input, exactly like a real TUI agent at its
    /// prompt. Orkia's prompt detector keys off that OS-level state, so
    /// a busy-poll would read as "running" and never trip detection.
    AwaitInput {
        #[serde(default)]
        bytes: Option<usize>,
        #[serde(default)]
        until: Option<String>,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
    /// Read and DISCARD anything on stdin for `ms` milliseconds, then
    /// continue. Mimics a real TUI agent (claude) consuming/ignoring
    /// stdin during its startup before its input box is ready: bytes a
    /// caller wrote to the PTY too early are swallowed, not buffered.
    /// Lets tests reproduce the "initial prompt written at spawn gets
    /// lost" race that the detector-gated injection path fixes.
    DrainInput { ms: u64 },
    /// Read stdin and ECHO every byte back to stdout (like a real input
    /// box rendering typed characters), returning only once a submit
    /// (CR or LF) is received. Models the contract the injection
    /// executor's grid-confirm relies on: the typed body must appear on
    /// screen, and the prompt is only committed by the trailing `\r`.
    EchoUntilSubmit {
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
    /// Sleep for `ms` milliseconds.
    Sleep { ms: u64 },
    /// Exit the agent process with the given status (default 0). If
    /// `Exit` is the last step it's implicit; specifying it lets you
    /// test non-zero exits and the journal `agent.exit` envelope.
    Exit {
        #[serde(default)]
        code: i32,
    },
    /// Crash the agent process by raising a fatal signal. Used by
    /// should turn the resulting WIFSIGNALED into a `Completed`
    /// lifecycle envelope with a non-zero `exit_code` (128 + signum).
    Crash {
        #[serde(default = "default_crash_mode")]
        mode: CrashMode,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrashMode {
    /// `std::process::abort()` → SIGABRT (signal 6) → exit code 134.
    /// Portable, no unsafe.
    Abort,
    /// `libc::raise(SIGSEGV)` (signal 11) → exit code 139. Unix only;
    /// unsafe block. Defaults to Abort if disabled by build target.
    Sigsegv,
}

fn default_crash_mode() -> CrashMode {
    CrashMode::Abort
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Osc133Marker {
    PromptStart,
    CommandStart,
    CommandOutput,
    CommandEnd,
}

fn default_timeout_ms() -> u64 {
    5_000
}

impl AgentScript {
    /// Render the script to YAML for on-disk storage.
    pub fn to_yaml(&self) -> Result<String, serde_yaml::Error> {
        serde_yaml::to_string(self)
    }

    /// Parse the script from YAML.
    pub fn from_yaml(s: &str) -> Result<Self, serde_yaml::Error> {
        serde_yaml::from_str(s)
    }
}
