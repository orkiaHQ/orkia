// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Rendering a materialized `Value` into `BlockContent` for the REPL.
//!
//! through the *unchanged* `emit_block` path. A table of records becomes an
//! aligned text block; a record becomes key/value lines; a scalar a single
//! line.

use orkia_shell_types::{BlockContent, CellStyle, StyledCell, Value};

use crate::exec::convert::scalar_to_string;

/// Convert a materialized value into display blocks.
pub fn value_to_blocks(value: &Value) -> Vec<BlockContent> {
    match value {
        Value::Nothing => Vec::new(),
        Value::List(items) if all_records(items) => table_blocks(items),
        Value::List(items) => items
            .iter()
            .map(|v| BlockContent::Text(scalar_to_string(v)))
            .collect(),
        Value::Record(_) => record_blocks(value),
        scalar => vec![BlockContent::Text(scalar_to_string(scalar))],
    }
}

fn all_records(items: &[Value]) -> bool {
    !items.is_empty() && items.iter().all(|v| matches!(v, Value::Record(_)))
}

/// Render a list of records as an aligned table: a dim header block followed
/// by one styled `TableRow` per row. Per-cell colour is a presentation hint
/// (`CellStyle`), resolved by each renderer — the source `Value` is untouched.
fn table_blocks(rows: &[Value]) -> Vec<BlockContent> {
    let columns = collect_columns(rows);
    // (text, style) per cell, row-major. Style is decided from the *typed*
    // value, the text from its scalar form.
    let cells: Vec<Vec<(String, CellStyle)>> = rows
        .iter()
        .map(|row| columns.iter().map(|c| cell_text_style(row, c)).collect())
        .collect();

    let mut widths: Vec<usize> = columns.iter().map(|c| c.len()).collect();
    for row in &cells {
        for (i, (text, _)) in row.iter().enumerate() {
            widths[i] = widths[i].max(text.len());
        }
    }

    let mut blocks = Vec::with_capacity(rows.len() + 1);
    blocks.push(BlockContent::SystemInfo(join_padded(&columns, &widths)));
    for row in &cells {
        blocks.push(BlockContent::TableRow(styled_row(row, &widths)));
    }
    blocks
}

/// Pad each cell to its column width and attach its style hint.
fn styled_row(cells: &[(String, CellStyle)], widths: &[usize]) -> Vec<StyledCell> {
    cells
        .iter()
        .enumerate()
        .map(|(i, (text, style))| StyledCell {
            text: format!(
                "{text:<width$}",
                width = widths.get(i).copied().unwrap_or(0)
            ),
            style: *style,
        })
        .collect()
}

/// Render a single record as `key: value` lines.
fn record_blocks(value: &Value) -> Vec<BlockContent> {
    match value.as_record() {
        Some(map) => map
            .iter()
            .map(|(k, v)| BlockContent::Text(format!("{k}: {}", scalar_to_string(v))))
            .collect(),
        None => Vec::new(),
    }
}

/// Column names in first-seen order across all rows.
fn collect_columns(rows: &[Value]) -> Vec<String> {
    let mut columns: Vec<String> = Vec::new();
    for row in rows {
        if let Some(map) = row.as_record() {
            for key in map.keys() {
                if !columns.iter().any(|c| c == key) {
                    columns.push(key.clone());
                }
            }
        }
    }
    columns
}

/// The display text and presentation hint for one cell. The hint is derived
/// from the *typed* value (not the rendered string) so it stays robust to
/// formatting changes.
fn cell_text_style(row: &Value, column: &str) -> (String, CellStyle) {
    match row.as_record().and_then(|map| map.get(column)) {
        Some(value) => (scalar_to_string(value), cell_style(column, value)),
        None => (String::new(), CellStyle::Plain),
    }
}

/// Map a `(column, value)` to a colour hint. Known semantic columns
/// (`status`/`state`, `trust`) get a meaning-driven colour; everything else is
/// plain. Adding a column here is the single place new table colouring lands.
fn cell_style(column: &str, value: &Value) -> CellStyle {
    match column {
        "status" | "state" => match value {
            Value::String(s) => status_style(s),
            _ => CellStyle::Plain,
        },
        "trust" => match value {
            Value::Float(t) => trust_style(*t),
            Value::Int(t) => trust_style(*t as f64),
            _ => CellStyle::Plain,
        },
        "type" => match value {
            Value::String(s) => type_style(s),
            _ => CellStyle::Plain,
        },
        _ => CellStyle::Plain,
    }
}

/// `type` column → colour. Serves two disjoint value spaces: `ls` entry kinds
/// (dir/symlink/file) and `journal` event kinds (hook/approval/seal/…).
fn type_style(kind: &str) -> CellStyle {
    match kind {
        // ls entry kinds.
        "dir" | "symlink" => CellStyle::Accent,
        // journal event kinds.
        "seal" | "scope_change" => CellStyle::Accent,
        "approval" => CellStyle::Warn,
        "hook" | "lifecycle" | "shell" | "tell" => CellStyle::Dim,
        _ => CellStyle::Plain,
    }
}

/// Lifecycle status → colour. Mirrors the `JobUpdate` mapping (done/running →
/// good, stopped/waiting → warn, failed/error → bad, idle → dim).
fn status_style(status: &str) -> CellStyle {
    match status {
        "running" | "working" | "done" | "active" => CellStyle::Good,
        "waiting" | "stopped" | "paused" => CellStyle::Warn,
        "error" | "failed" => CellStyle::Bad,
        "idle" => CellStyle::Dim,
        _ => CellStyle::Plain,
    }
}

/// Trust score → colour, on ATLAS thresholds (≥0.7 supervised-or-better,
/// ≥0.5 restricted, below locked).
fn trust_style(score: f64) -> CellStyle {
    if score >= 0.7 {
        CellStyle::Good
    } else if score >= 0.5 {
        CellStyle::Warn
    } else {
        CellStyle::Bad
    }
}

fn join_padded(cells: &[String], widths: &[usize]) -> String {
    cells
        .iter()
        .enumerate()
        .map(|(i, cell)| {
            format!(
                "{cell:<width$}",
                width = widths.get(i).copied().unwrap_or(0)
            )
        })
        .collect::<Vec<_>>()
        .join("  ")
}
