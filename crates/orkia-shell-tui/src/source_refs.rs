// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use orkia_shell_types::BlockContent;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceItem {
    pub raw: String,
    pub kind: SourceKind,
    pub preview: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Knowledge,
    Journal,
    Seal,
}

impl SourceKind {
    pub fn label(self) -> &'static str {
        match self {
            SourceKind::Knowledge => "KG",
            SourceKind::Journal => "JOURNAL",
            SourceKind::Seal => "SEAL",
        }
    }
}

pub fn refs_from_blocks(blocks: &[BlockContent]) -> Vec<String> {
    items_from_blocks(blocks)
        .into_iter()
        .map(|item| item.raw)
        .collect()
}

pub fn items_from_blocks(blocks: &[BlockContent]) -> Vec<SourceItem> {
    let mut out = Vec::new();
    for block in blocks {
        for text in block_texts(block) {
            collect_refs(&text, &mut out);
        }
    }
    dedupe(out)
}

fn block_texts(block: &BlockContent) -> Vec<String> {
    match block {
        BlockContent::Text(text) | BlockContent::SystemInfo(text) | BlockContent::Error(text) => {
            vec![text.clone()]
        }
        BlockContent::Notice { text, .. } => vec![text.clone()],
        BlockContent::AgentMessage { text, .. } => vec![text.clone()],
        BlockContent::ToolCall { target, .. } => vec![target.clone()],
        BlockContent::TableRow(cells) => cells.iter().map(|cell| cell.text.clone()).collect(),
        BlockContent::Approval { action, .. } => vec![action.clone()],
        BlockContent::SealRecord { hash_short, .. } => vec![hash_short.clone()],
        BlockContent::Attention { rows, message } => {
            let mut texts = message.clone().into_iter().collect::<Vec<_>>();
            texts.extend(rows.iter().map(|row| row.summary.clone()));
            texts
        }
    }
}

fn collect_refs(text: &str, out: &mut Vec<SourceItem>) {
    for raw in text.split_whitespace() {
        let token = raw
            .trim_start_matches("ref=")
            .trim_matches(|ch: char| matches!(ch, '[' | ']' | ',' | ';' | '"' | '\''));
        if let Some(item) = source_item(token) {
            out.push(item);
        }
    }
}

pub fn source_item(token: &str) -> Option<SourceItem> {
    let kind = if token.starts_with("kg://") || token.starts_with("kg:") {
        SourceKind::Knowledge
    } else if token.starts_with("journal://") || token.starts_with("journal:") {
        SourceKind::Journal
    } else if token.starts_with("seal:") {
        SourceKind::Seal
    } else {
        return None;
    };
    Some(SourceItem {
        raw: token.to_string(),
        kind,
        preview: preview(token),
    })
}

fn preview(token: &str) -> String {
    if let Some(id) = token.strip_prefix("kg://node/") {
        return short(id, 12);
    }
    if let Some(id) = token.split_once("/node/").map(|(_, id)| id) {
        return short(id, 12);
    }
    if let Some(id) = token.strip_prefix("kg:") {
        return short(id, 12);
    }
    if let Some(id) = token
        .strip_prefix("journal://event/")
        .or_else(|| token.strip_prefix("journal:"))
        .or_else(|| token.strip_prefix("seal:"))
    {
        return format!("#{id}");
    }
    short(token, 24)
}

fn short(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        value.to_string()
    } else {
        format!("{}...", &value[..limit])
    }
}

fn dedupe(items: Vec<SourceItem>) -> Vec<SourceItem> {
    let mut out = Vec::new();
    for item in items {
        if !out.iter().any(|seen: &SourceItem| seen.raw == item.raw) {
            out.push(item);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_source_refs_from_projection_text() {
        let blocks = vec![BlockContent::Text(
            "    [kg:abcdef12] score=44 source=knowledge_node ref=kg://node/abc Auth".into(),
        )];
        assert_eq!(
            refs_from_blocks(&blocks),
            vec!["kg:abcdef12".to_string(), "kg://node/abc".to_string()]
        );
    }

    #[test]
    fn groups_source_items_with_previews() {
        let blocks = vec![BlockContent::Text(
            "[kg:abcdef123456] ref=journal://event/7 [seal:9]".into(),
        )];
        let items = items_from_blocks(&blocks);
        assert_eq!(items[0].kind, SourceKind::Knowledge);
        assert_eq!(items[0].preview, "abcdef123456");
        assert_eq!(items[1].kind, SourceKind::Journal);
        assert_eq!(items[1].preview, "#7");
        assert_eq!(items[2].kind, SourceKind::Seal);
        assert_eq!(items[2].preview, "#9");
    }

    #[test]
    fn extracts_journal_and_seal_refs() {
        let blocks = vec![BlockContent::Text(
            "[journal:7] ref=journal://event/7 [seal:9]".into(),
        )];
        assert_eq!(
            refs_from_blocks(&blocks),
            vec![
                "journal:7".to_string(),
                "journal://event/7".to_string(),
                "seal:9".to_string()
            ]
        );
    }
}
