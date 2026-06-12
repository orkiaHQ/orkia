// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Built-in `ShellRenderer` implementations.
//!
//! - [`shell_mode::ShellModeRenderer`] — interactive default. stdin/stdout
//!   like zsh/bash. Prompt + diagnostics on stderr, command output on
//!   stdout (pipe-friendly). The renderer for `chsh -s /usr/bin/orkia`.
//! - [`stdout::StdoutRenderer`] — non-interactive / piped fallback. Used
//!   when stdin/stdout aren't a TTY (`echo ls | orkia`) or with explicit
//!   `--no-tui`. No prompt; reads one line per command.

pub mod shell_mode;
pub mod stdout;

pub use shell_mode::ShellModeRenderer;
pub use stdout::StdoutRenderer;

use orkia_shell_types::decision::{BlockContent, CellStyle};
use std::io::Write;

/// Format a [`BlockContent`] as ANSI-coloured text. Shared between the
/// `ShellModeRenderer` and the `StdoutRenderer` since both want the same
/// visual conventions for blocks — only the prompt+input handling differs.
pub(crate) fn write_block_ansi<W: Write>(w: &mut W, block: &BlockContent) -> std::io::Result<()> {
    match block {
        BlockContent::Text(t) => writeln!(w, "{t}"),
        BlockContent::AgentMessage { agent, text } => {
            writeln!(w, "  \x1b[35m{agent}\x1b[0m: {text}")
        }
        BlockContent::ToolCall {
            agent: _,
            tool,
            target,
            duration_ms,
            status,
        } => {
            let status_color = if status == "done" { "32" } else { "31" };
            writeln!(
                w,
                "  \x1b[35m│\x1b[0m \x1b[34m{tool}\x1b[0m {target} \x1b[{status_color}m{status}\x1b[0m \x1b[90m{duration_ms}ms\x1b[0m"
            )
        }
        BlockContent::Approval {
            agent,
            action,
            risk,
        } => {
            let risk_painted = match cell_ansi(CellStyle::for_risk(risk)) {
                Some(code) => format!("\x1b[{code}m{risk}\x1b[0m"),
                None => risk.clone(),
            };
            writeln!(
                w,
                "  \x1b[33m⚠ APPROVAL\x1b[0m {agent}: {action} (risk: {risk_painted})\n  \x1b[32m[y]\x1b[0m approve  \x1b[31m[n]\x1b[0m deny"
            )
        }
        BlockContent::Attention { rows, message } => {
            if let Some(message) = message {
                writeln!(w, "  \x1b[35mattention\x1b[0m {message}")?;
            }
            for row in rows {
                let actions = row
                    .actions
                    .iter()
                    .map(orkia_shell_types::AttentionAction::as_str)
                    .collect::<Vec<_>>()
                    .join(", ");
                writeln!(
                    w,
                    "  \x1b[35m{}\x1b[0m agent={} kind={} age={} actions=[{}]\n    {}",
                    row.id,
                    row.agent,
                    row.kind.as_str(),
                    row.age,
                    actions,
                    row.summary
                )?;
            }
            Ok(())
        }
        BlockContent::SealRecord {
            seq,
            agent,
            event,
            hash_short,
        } => writeln!(
            w,
            "  \x1b[35m#{seq}\x1b[0m {agent} {event} \x1b[90m{hash_short}\x1b[0m"
        ),
        BlockContent::TableRow(cells) => {
            let mut line = String::new();
            for (i, cell) in cells.iter().enumerate() {
                if i > 0 {
                    line.push_str("  ");
                }
                match cell_ansi(cell.style) {
                    Some(code) => {
                        line.push_str("\x1b[");
                        line.push_str(code);
                        line.push('m');
                        line.push_str(&cell.text);
                        line.push_str("\x1b[0m");
                    }
                    None => line.push_str(&cell.text),
                }
            }
            writeln!(w, "{line}")
        }
        BlockContent::Notice { style, text } => match cell_ansi(*style) {
            Some(code) => writeln!(w, "  \x1b[{code}m{text}\x1b[0m"),
            None => writeln!(w, "  {text}"),
        },
        BlockContent::SystemInfo(t) => writeln!(w, "  \x1b[90m{t}\x1b[0m"),
        BlockContent::Error(t) => writeln!(w, "  \x1b[31merror:\x1b[0m {t}"),
    }
}

/// [`write_block_ansi`] for non-TTY stdout. Same match arms, same line
/// structure and column joins, zero escape sequences. Table cells arrive
/// pre-padded (see `exec/display.rs::styled_row`), so the two-space joins
/// here keep plain tables aligned and machine-greppable.
pub(crate) fn write_block_plain<W: Write>(w: &mut W, block: &BlockContent) -> std::io::Result<()> {
    match block {
        BlockContent::Text(t) => writeln!(w, "{t}"),
        BlockContent::AgentMessage { agent, text } => writeln!(w, "  {agent}: {text}"),
        BlockContent::ToolCall {
            agent: _,
            tool,
            target,
            duration_ms,
            status,
        } => writeln!(w, "  │ {tool} {target} {status} {duration_ms}ms"),
        BlockContent::Approval {
            agent,
            action,
            risk,
        } => writeln!(
            w,
            "  ⚠ APPROVAL {agent}: {action} (risk: {risk})\n  [y] approve  [n] deny"
        ),
        BlockContent::Attention { rows, message } => {
            if let Some(message) = message {
                writeln!(w, "  attention {message}")?;
            }
            for row in rows {
                let actions = row
                    .actions
                    .iter()
                    .map(orkia_shell_types::AttentionAction::as_str)
                    .collect::<Vec<_>>()
                    .join(", ");
                writeln!(
                    w,
                    "  {} agent={} kind={} age={} actions=[{}]\n    {}",
                    row.id,
                    row.agent,
                    row.kind.as_str(),
                    row.age,
                    actions,
                    row.summary
                )?;
            }
            Ok(())
        }
        BlockContent::SealRecord {
            seq,
            agent,
            event,
            hash_short,
        } => writeln!(w, "  #{seq} {agent} {event} {hash_short}"),
        BlockContent::TableRow(cells) => {
            let mut line = String::new();
            for (i, cell) in cells.iter().enumerate() {
                if i > 0 {
                    line.push_str("  ");
                }
                line.push_str(&cell.text);
            }
            writeln!(w, "{line}")
        }
        BlockContent::Notice { style: _, text } => writeln!(w, "  {text}"),
        BlockContent::SystemInfo(t) => writeln!(w, "  {t}"),
        BlockContent::Error(t) => writeln!(w, "  error: {t}"),
    }
}

/// Route a block through the ANSI or plain writer. `plain` is decided once
pub(crate) fn write_block<W: Write>(
    w: &mut W,
    block: &BlockContent,
    plain: bool,
) -> std::io::Result<()> {
    if plain {
        write_block_plain(w, block)
    } else {
        write_block_ansi(w, block)
    }
}

/// `NO_COLOR` per the de facto standard (<https://no-color.org>): the
/// variable being **set at all** — any value, including the empty string —
/// disables colour. Chosen because `var_os(..).is_some()` is the simplest
/// faithful reading of the standard.
pub(crate) fn no_color_env() -> bool {
    std::env::var_os("NO_COLOR").is_some()
}

/// plain when stdout is not a terminal, or when `NO_COLOR` is set. Pure so
/// the seam is unit-testable without faking a TTY.
pub(crate) fn plain_output(stdout_is_tty: bool, no_color: bool) -> bool {
    !stdout_is_tty || no_color
}

/// SGR colour code for a cell hint, or `None` for no styling. Mirrors
/// `cell_tui_style` in the TUI renderer so the two stay visually consistent.
fn cell_ansi(style: CellStyle) -> Option<&'static str> {
    match style {
        CellStyle::Plain => None,
        CellStyle::Dim => Some("90"),
        CellStyle::Good => Some("32"),
        CellStyle::Warn => Some("33"),
        CellStyle::Bad => Some("31"),
        CellStyle::Accent => Some("35"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::display::value_to_blocks;
    use indexmap::IndexMap;
    use orkia_shell_types::{
        AttentionAction, AttentionId, AttentionKind, AttentionRow, AttentionSeverity, StyledCell,
        Value,
    };

    fn render_plain(blocks: &[BlockContent]) -> String {
        let mut buf = Vec::new();
        for block in blocks {
            write_block_plain(&mut buf, block).unwrap();
        }
        String::from_utf8(buf).unwrap()
    }

    fn representative_blocks() -> Vec<BlockContent> {
        vec![
            BlockContent::Text("plain text".into()),
            BlockContent::AgentMessage {
                agent: "faye".into(),
                text: "hello".into(),
            },
            BlockContent::ToolCall {
                agent: "faye".into(),
                tool: "Read".into(),
                target: "src/main.rs".into(),
                duration_ms: 12,
                status: "done".into(),
            },
            BlockContent::Approval {
                agent: "faye".into(),
                action: "rm -rf build".into(),
                risk: "high".into(),
            },
            BlockContent::Attention {
                rows: vec![AttentionRow {
                    id: AttentionId(7),
                    job_id: Some(3),
                    agent: "faye".into(),
                    kind: AttentionKind::AgentPrompt,
                    severity: AttentionSeverity::Fresh,
                    age: "2m".into(),
                    summary: "needs input".into(),
                    actions: vec![AttentionAction::Pull, AttentionAction::Resolve],
                }],
                message: Some("1 item".into()),
            },
            BlockContent::SealRecord {
                seq: 42,
                agent: "faye".into(),
                event: "tool.call".into(),
                hash_short: "ab12cd".into(),
            },
            BlockContent::TableRow(vec![
                StyledCell {
                    text: "alpha    ".into(),
                    style: CellStyle::Good,
                },
                StyledCell {
                    text: "done".into(),
                    style: CellStyle::Bad,
                },
            ]),
            BlockContent::Notice {
                style: CellStyle::Warn,
                text: "soft refusal".into(),
            },
            BlockContent::SystemInfo("name  status".into()),
            BlockContent::Error("boom".into()),
        ]
    }

    #[test]
    fn plain_writer_emits_no_escape_bytes() {
        let rendered = render_plain(&representative_blocks());
        assert!(
            !rendered.as_bytes().contains(&0x1b),
            "plain output must contain no ESC byte: {rendered:?}"
        );
        // The ANSI writer over the same blocks *does* escape — guards the
        // test against a representative set that exercises nothing.
        let mut ansi = Vec::new();
        for block in &representative_blocks() {
            write_block_ansi(&mut ansi, block).unwrap();
        }
        assert!(ansi.contains(&0x1b));
    }

    #[test]
    fn plain_error_is_greppable_error_line() {
        let rendered = render_plain(&[BlockContent::Error("boom".into())]);
        assert_eq!(rendered, "  error: boom\n");
    }

    /// Snapshot of the plain table format — a scripting contract
    /// identical to the ANSI version.
    #[test]
    fn plain_table_snapshot() {
        let row = |name: &str, status: &str, ms: i64| {
            let mut r = IndexMap::new();
            r.insert("name".to_string(), Value::String(name.into()));
            r.insert("status".to_string(), Value::String(status.into()));
            r.insert("ms".to_string(), Value::Int(ms));
            Value::Record(r)
        };
        let blocks = value_to_blocks(&Value::List(vec![
            row("alpha", "done", 12),
            row("beta-long", "failed", 3),
        ]));
        let rendered = render_plain(&blocks);
        assert_eq!(
            rendered,
            "  name       status  ms\n\
             alpha      done    12\n\
             beta-long  failed  3 \n"
        );
        // Column starts are stable across rows — machine-greppable.
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines[1].find("done"), Some(11));
        assert_eq!(lines[2].find("failed"), Some(11));
        assert_eq!(lines[1].find("12"), Some(19));
        assert_eq!(lines[2].find('3'), Some(19));
    }

    /// forces plain even when stdout would be a TTY (i.e. when the
    /// renderer would otherwise be ANSI); a non-TTY stdout is plain
    /// regardless. Tested as the pure function the constructors call —
    /// not by faking a TTY.
    #[test]
    fn no_color_forces_plain_at_constructor_seam() {
        assert!(!plain_output(true, false), "TTY, no NO_COLOR → ANSI");
        assert!(plain_output(true, true), "NO_COLOR wins on a TTY");
        assert!(plain_output(false, false), "non-TTY stdout → plain");
        assert!(plain_output(false, true));
    }
}
