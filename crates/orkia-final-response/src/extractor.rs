// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Per-provider transcript extractor trait.
//!
//! Each provider (Claude / Codex / Gemini) writes its conversation to a
//! provider-specific log on disk. On every `Stop` envelope the
//! `FinalResponseService` looks up the matching extractor and asks it
//! to return the assistant's final text for the just-completed turn.
//!

use std::fmt;
use std::io;
use std::path::PathBuf;

/// Context passed to every extractor invocation. Built from the `Stop`
/// envelope (job_id, agent, session_id) plus an optional
/// `transcript_path` hint (Claude sets this in its Stop payload; other
/// providers do not).
#[derive(Debug, Clone)]
pub struct ExtractionContext {
    pub job_id: u32,
    pub agent: String,
    pub session_id: Option<String>,
    pub transcript_path_hint: Option<PathBuf>,
    /// Working directory the agent was spawned in. Used by the Gemini
    /// extractor to derive its per-project hash directory.
    pub spawn_cwd: Option<PathBuf>,
    /// Override for the `transcript_path_hint` confinement root (SEC-029).
    /// `None` in production → each extractor confines the hint to its
    /// provider's real transcripts dir (`~/.claude/projects`,
    /// `$CODEX_HOME`, `~/.gemini/tmp`). Set only in tests to point
    /// confinement at a tempdir.
    pub confine_root: Option<PathBuf>,
    /// Final assistant text the provider delivered *in the Stop hook
    /// payload itself* (Claude's `last_assistant_message`). When present
    /// this is authoritative and race-free — it arrives synchronously with
    /// the Stop event, so the extractor returns it directly instead of
    /// re-reading a transcript file that the provider may not have flushed
    /// yet (or, for an orkia-spawned Claude TUI, never writes to disk at
    /// all). `None` → fall back to the on-disk transcript. Oversize
    /// messages are not carried here (see the normalize cap), so this
    /// never forces a giant Stop envelope.
    pub final_message_hint: Option<String>,
}

/// Failure modes for transcript extraction. `TranscriptNotFound` and
/// `NoAssistantMessage` are not bugs — they are legitimate "we have
/// nothing to report" outcomes that the service surfaces in the failure
#[derive(Debug)]
pub enum ExtractionError {
    TranscriptNotFound,
    TranscriptUnreadable(io::Error),
    NoAssistantMessage,
    MalformedTranscript(String),
}

impl fmt::Display for ExtractionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TranscriptNotFound => f.write_str("transcript file not found"),
            Self::TranscriptUnreadable(e) => write!(f, "transcript unreadable: {e}"),
            Self::NoAssistantMessage => f.write_str("no assistant message in final turn"),
            Self::MalformedTranscript(msg) => write!(f, "malformed transcript: {msg}"),
        }
    }
}

impl std::error::Error for ExtractionError {}

/// Read the final assistant text from a provider's transcript file.
/// Implementations may locate the file via the hook payload
/// (`transcript_path_hint`), the session UUID, or well-known paths
/// under `$HOME`. Empty assistant turns return `Ok(String::new())`
pub trait TranscriptExtractor: Send + Sync {
    fn extract_final_assistant_text(
        &self,
        ctx: &ExtractionContext,
    ) -> Result<String, ExtractionError>;
}
