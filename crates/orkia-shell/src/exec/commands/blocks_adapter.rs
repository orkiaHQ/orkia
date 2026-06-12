// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! A legacy display builtin returns `Vec<BlockContent>` — it *knows it renders*
//! (a C1 violation). A migrated `Command` must instead return `PipelineData`
//! (structured `Value`); the EXEC-SINK Display sink (`display::value_to_blocks`)
//! does the rendering downstream. For a pure-text builtin the honest structured
//! form is a list of lines: `Value::List(Vec<Value::String>)`, which the sink
//! emits as one `Text` block per line.
//!
//! This adapter flattens the existing pure content generators
//! (`orkia_builtin::{help,route,briefing}`) to that shape, keeping a single
//! source of truth for the text. It is deliberately transitional: when the
//! generators are rewritten `Value`-native and `BlockContent` is retired
//! (Vague 4/5), this module is deleted.

use orkia_shell_types::{BlockContent, Value};

/// Flatten a legacy `Vec<BlockContent>` into one string per line. The content
/// generators this bridges only ever emit `SystemInfo`/`Text`; the remaining
/// arm is a total, non-panicking fallback so a future block kind never crashes
/// the shell.
pub(crate) fn blocks_to_lines(blocks: Vec<BlockContent>) -> Vec<String> {
    blocks
        .into_iter()
        .map(|block| match block {
            BlockContent::Text(s) | BlockContent::SystemInfo(s) | BlockContent::Error(s) => s,
            // `seal` emits chain rows as `SealRecord`; flatten to the same text
            // the TUI renderer shows (sans ANSI) so the migrated report reads
            BlockContent::SealRecord {
                seq,
                agent,
                event,
                hash_short,
            } => format!("#{seq} {agent} {event} {hash_short}"),
            other => format!("{other:?}"),
        })
        .collect()
}

/// Flatten a legacy `Vec<BlockContent>` into a structured `Value::List` of
/// one string per line — the migrated-command output shape the Display sink
/// renders.
pub(crate) fn blocks_to_value(blocks: Vec<BlockContent>) -> Value {
    Value::List(
        blocks_to_lines(blocks)
            .into_iter()
            .map(Value::String)
            .collect(),
    )
}
