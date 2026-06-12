// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

use crate::card::CommandCard;
use crate::theme::Theme;
use orkia_shell_types::{BlockContent, CellStyle};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use std::time::{Duration, Instant};

/// Render the scrollable card stream. Each [`CommandCard`] is a header line
/// (command · duration · status) followed by its blocks, each block carrying
/// a colored left gutter. `scroll_offset` is in *visual lines* from the
/// bottom (0 = pinned to bottom).
pub fn render_main_pane(
    f: &mut Frame<'_>,
    area: Rect,
    cards: &[CommandCard],
    scroll_offset: usize,
    selected: Option<usize>,
    theme: &Theme,
) {
    let width = area.width as usize;
    let mut all_lines: Vec<Line<'static>> = Vec::new();
    // Line index where each card's header begins — used to anchor the view
    // on the selected card.
    let mut header_at: Vec<usize> = vec![0; cards.len()];
    for (ci, card) in cards.iter().enumerate() {
        // Blank spacer between cards (not before the first, and not before
        // an empty preamble that would render nothing).
        let renders_anything = card.command.is_some() || !card.blocks.is_empty();
        if ci > 0 && renders_anything {
            all_lines.push(Line::raw(""));
        }
        header_at[ci] = all_lines.len();
        if card.command.is_some() {
            all_lines.push(render_card_header(card, width, selected == Some(ci), theme));
        }
        if card.collapsed {
            all_lines.push(fold_summary(card, theme));
        } else {
            push_card_body(&mut all_lines, &card.blocks, theme);
        }
    }

    let height = area.height as usize;
    let total = all_lines.len();

    // With a selection, anchor the view so the selected card's header is
    // visible (one line of breathing room above). Otherwise keep the
    // bottom-pinned behaviour driven by `scroll_offset`.
    let start = match selected {
        Some(sel) => header_at
            .get(sel)
            .copied()
            .unwrap_or(0)
            .saturating_sub(1)
            .min(total.saturating_sub(height)),
        None => {
            let end = total.saturating_sub(scroll_offset);
            end.saturating_sub(height)
        }
    };
    let visible: Vec<Line<'static>> = all_lines.into_iter().skip(start).take(height).collect();

    let p = Paragraph::new(visible).wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

/// Push a card's blocks into `out`, each line carrying a colored left
/// gutter. Same-kind blocks stay tight; a blank spacer separates a change
/// of kind (text → agent → approval).
fn push_card_body(out: &mut Vec<Line<'static>>, blocks: &[BlockContent], theme: &Theme) {
    for (i, b) in blocks.iter().enumerate() {
        if i > 0 && !same_group(&blocks[i - 1], b) {
            out.push(Line::raw(""));
        }
        let accent = block_accent(b, theme);
        for line in render_block_lines(b, theme) {
            let mut spans = vec![Span::styled("┃ ", Style::default().fg(accent))];
            spans.extend(line.spans);
            out.push(Line::from(spans));
        }
    }
}

/// The card header bar: `❯ <command>` on the left, `<duration> <dot>` on the
/// right, padded to the pane width and laid on the elevated surface so it
/// reads as a Warp-style block header.
fn render_card_header(
    card: &CommandCard,
    width: usize,
    selected: bool,
    theme: &Theme,
) -> Line<'static> {
    let cmd = card.command.clone().unwrap_or_default();
    let (status, status_color) = status_label(card, theme);
    let dur = duration_label(card);
    // A fold marker on the left edge signals collapse state at a glance.
    let marker = if card.collapsed { "▸ " } else { "❯ " };

    let left_len = 2 + cmd.chars().count();
    let right_len = dur.chars().count() + 2 + status.chars().count(); // dur + "  " + status
    let pad = width.saturating_sub(left_len + right_len).max(1);

    let line = Line::from(vec![
        Span::styled(marker, Style::default().fg(theme.accent)),
        Span::styled(
            cmd,
            Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" ".repeat(pad)),
        Span::styled(dur, Style::default().fg(theme.dim)),
        Span::raw("  "),
        Span::styled(status, Style::default().fg(status_color)),
    ]);
    let bg = if selected {
        theme.bg_selected
    } else {
        theme.bg_elevated
    };
    line.style(Style::default().bg(bg))
}

/// The single dim line shown in place of a folded card's body.
fn fold_summary(card: &CommandCard, theme: &Theme) -> Line<'static> {
    let mut body: Vec<Line<'static>> = Vec::new();
    push_card_body(&mut body, &card.blocks, theme);
    let n = body.len();
    let unit = if n == 1 { "line" } else { "lines" };
    Line::from(vec![
        Span::styled("┃ ", Style::default().fg(theme.border)),
        Span::styled(
            format!("… {n} {unit} folded (Ctrl-O)"),
            Style::default().fg(theme.dim),
        ),
    ])
}

/// Status indicator for a card: green ✓ when done, red ✗ (with the exit
/// code when known) on failure, yellow ◐ while still running.
fn status_label(card: &CommandCard, theme: &Theme) -> (String, Color) {
    match (card.finished, card.failed) {
        (Some(_), true) => {
            let label = match card.exit_code {
                Some(code) => format!("✗ {code}"),
                None => "✗".to_string(),
            };
            (label, theme.red)
        }
        (Some(_), false) => ("✓".to_string(), theme.green),
        (None, _) => ("◐".to_string(), theme.yellow),
    }
}

/// Elapsed-time label: the final duration once finished, otherwise the live
/// elapsed for a running command.
fn duration_label(card: &CommandCard) -> String {
    match (card.started, card.finished) {
        (Some(start), Some(end)) => fmt_duration(end.saturating_duration_since(start)),
        (Some(start), None) => fmt_duration(Instant::now().saturating_duration_since(start)),
        _ => String::new(),
    }
}

fn fmt_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

fn render_block_lines(block: &BlockContent, theme: &Theme) -> Vec<Line<'static>> {
    match block {
        BlockContent::Text(t) => t.lines().map(|l| Line::raw(l.to_string())).collect(),
        BlockContent::AgentMessage { agent, text } => render_agent_message(agent, text, theme),
        BlockContent::ToolCall {
            agent,
            tool,
            target,
            duration_ms,
            status,
        } => render_tool_call(agent, tool, target, *duration_ms, status, theme),
        BlockContent::Approval {
            agent,
            action,
            risk,
        } => render_approval(agent, action, risk, theme),
        BlockContent::Attention { rows, message } => render_attention(rows, message, theme),
        BlockContent::SealRecord {
            seq,
            agent,
            event,
            hash_short,
        } => render_seal_record(*seq, agent, event, hash_short, theme),
        BlockContent::TableRow(cells) => render_table_row(cells, theme),
        BlockContent::Notice { style, text } => {
            vec![Line::styled(text.clone(), cell_tui_style(*style, theme))]
        }
        BlockContent::SystemInfo(t) => {
            vec![Line::styled(t.clone(), Style::default().fg(theme.dim))]
        }
        BlockContent::Error(t) => vec![Line::from(vec![
            Span::styled(
                "error: ",
                Style::default().fg(theme.red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(t.clone(), Style::default().fg(theme.red)),
        ])],
    }
}

fn render_agent_message(agent: &str, text: &str, theme: &Theme) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        Span::styled(
            agent.to_string(),
            Style::default()
                .fg(theme.agent_color(agent))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(": "),
        Span::raw(text.to_string()),
    ])]
}

fn render_tool_call(
    agent: &str,
    tool: &str,
    target: &str,
    duration_ms: u64,
    status: &str,
    theme: &Theme,
) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        Span::styled("│ ", Style::default().fg(theme.agent_color(agent))),
        Span::styled(
            tool.to_string(),
            Style::default().fg(theme.tool_color(tool)),
        ),
        Span::raw(format!(" {target} ")),
        Span::styled(
            status.to_string(),
            Style::default().fg(if status == "done" {
                theme.green
            } else {
                theme.red
            }),
        ),
        Span::styled(format!(" {duration_ms}ms"), Style::default().fg(theme.dim)),
    ])]
}

fn render_approval(agent: &str, action: &str, risk: &str, theme: &Theme) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::styled(
                "approval · ",
                Style::default()
                    .fg(theme.yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                agent.to_string(),
                Style::default().fg(theme.agent_color(agent)),
            ),
            Span::raw(format!(" — {action}")),
        ]),
        Line::from(vec![
            Span::styled("risk ", Style::default().fg(theme.dim)),
            Span::styled(
                risk.to_string(),
                cell_tui_style(CellStyle::for_risk(risk), theme),
            ),
            Span::styled(
                "    approve <id> | deny <id>",
                Style::default().fg(theme.dim),
            ),
        ]),
    ]
}

fn render_seal_record(
    seq: u64,
    agent: &str,
    event: &str,
    hash_short: &str,
    theme: &Theme,
) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        Span::styled(format!("#{seq}"), Style::default().fg(theme.accent)),
        Span::raw(format!(" {agent} {event} ")),
        Span::styled(hash_short.to_string(), Style::default().fg(theme.dim)),
    ])]
}

fn render_table_row(cells: &[orkia_shell_types::StyledCell], theme: &Theme) -> Vec<Line<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(cells.len() * 2);
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            cell.text.clone(),
            cell_tui_style(cell.style, theme),
        ));
    }
    vec![Line::from(spans)]
}

/// Whether two adjacent blocks belong to the same visual group and so
/// should not be separated by a blank spacer. Blocks of the same kind
/// stream as one unit (a command's text output, a multi-row table); the
/// spacer only appears when the kind changes (text → agent → approval).
fn same_group(prev: &BlockContent, cur: &BlockContent) -> bool {
    std::mem::discriminant(prev) == std::mem::discriminant(cur)
}

/// Colour of a block's left gutter, signalling its kind at a glance:
/// agent output tints to the agent's colour, approvals to yellow, errors
/// to red, audit/notice to accent, and plain output to a muted border.
fn block_accent(block: &BlockContent, theme: &Theme) -> ratatui::style::Color {
    match block {
        BlockContent::AgentMessage { agent, .. } | BlockContent::ToolCall { agent, .. } => {
            theme.agent_color(agent)
        }
        BlockContent::Approval { .. } | BlockContent::Attention { .. } => theme.yellow,
        BlockContent::Error(_) => theme.red,
        BlockContent::SealRecord { .. } | BlockContent::Notice { .. } => theme.accent,
        BlockContent::Text(_) | BlockContent::TableRow(_) | BlockContent::SystemInfo(_) => {
            theme.border
        }
    }
}

fn render_attention(
    rows: &[orkia_shell_types::AttentionRow],
    message: &Option<String>,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(message) = message {
        lines.push(Line::from(vec![
            Span::styled(
                "attention ",
                Style::default()
                    .fg(theme.yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(message.clone(), Style::default().fg(theme.fg)),
        ]));
    }
    for row in rows {
        lines.push(Line::from(vec![
            Span::styled(
                row.id.to_string(),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                row.agent.clone(),
                Style::default().fg(theme.agent_color(&row.agent)),
            ),
            Span::styled(
                format!(" {} {}", row.kind.as_str(), row.age),
                Style::default().fg(theme.dim),
            ),
        ]));
        lines.push(Line::raw(format!("  {}", row.summary)));
    }
    lines
}

/// Map a cell hint to a ratatui style via the theme. Mirrors `cell_ansi` in the
/// shell-mode renderer so both surfaces colour tables identically.
fn cell_tui_style(style: CellStyle, theme: &Theme) -> Style {
    match style {
        CellStyle::Plain => Style::default(),
        CellStyle::Dim => Style::default().fg(theme.dim),
        CellStyle::Good => Style::default().fg(theme.green),
        CellStyle::Warn => Style::default().fg(theme.yellow),
        CellStyle::Bad => Style::default().fg(theme.red),
        CellStyle::Accent => Style::default().fg(theme.accent),
    }
}
