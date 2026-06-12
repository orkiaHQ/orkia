// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! The parsed shape of a typed pipeline, carried by `Decision::Exec`.
//!
//! Plain data (no engine, no registry) so it can live in the types crate and
//! be referenced from `Decision`. The parser that produces it lives in
//! `orkia-shell` (it needs the `CommandRegistry`).

/// One typed stage: a registry command name plus its raw, un-evaluated args.
/// Arguments are bound to the command's `Signature` later, by the engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedStage {
    pub name: String,
    pub raw_args: Vec<String>,
}

/// A fully parsed typed pipeline.
///
/// `shell_prefix` is the optional external/POSIX left part (everything left of
/// the first registry command), run via the shell engine and captured as a
/// `ByteStream` that feeds the typed stages — this is the `Bytes → Value`
/// boundary. When absent, the pipeline starts from `Empty`/`Nothing`.
///
/// `external_suffix` is the optional external command on the *right* of the
/// typed segment (`ork ls | where | grep`). When present, the typed output is
/// serialized line-by-line and streamed into that command's stdin — the
/// `Value → Bytes` boundary, driven by `PipelineSink::External`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecPlan {
    pub shell_prefix: Option<String>,
    pub stages: Vec<ParsedStage>,
    pub external_suffix: Option<String>,
}
