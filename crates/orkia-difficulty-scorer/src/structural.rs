// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

use std::collections::HashSet;

#[derive(Debug, Clone)]
pub struct StructuralSignals {
    pub token_count: u32,
    pub char_count: u32,
    pub unique_token_ratio: f32,
    pub code_block_count: u32,
    pub code_line_count: u32,
    pub inline_code_count: u32,
    pub nesting_depth: u32,
    pub url_count: u32,
    pub newline_count: u32,
    pub list_marker_count: u32,
    pub punctuation_density: f32,
    pub indentation_levels: u32,
    pub has_table: bool,
    pub has_json: bool,
    pub has_xml: bool,
    pub math_symbol_count: u32,
    pub parenthetical_depth: u32,
    pub quoted_block_count: u32,
    pub section_count: u32,
}

pub fn extract_structural(prompt: &str) -> StructuralSignals {
    let lines: Vec<&str> = prompt.lines().collect();
    let words: Vec<&str> = prompt.split_whitespace().collect();

    let token_count = words.len() as u32;
    let char_count = prompt.len() as u32;

    let unique: HashSet<String> = words.iter().map(|w| w.to_lowercase()).collect();
    let unique_token_ratio = unique.len() as f32 / words.len().max(1) as f32;

    let code_block_count = (prompt.matches("```").count() / 2) as u32;

    let mut code_line_count = 0u32;
    let mut in_code = false;
    for line in &lines {
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            code_line_count += 1;
        }
    }

    let inline_code_count = count_inline_code(prompt);
    let nesting_depth = compute_nesting_depth(prompt);

    let url_count =
        prompt.matches("http://").count() as u32 + prompt.matches("https://").count() as u32;

    let newline_count = prompt.matches('\n').count() as u32;
    let list_marker_count = count_list_markers(&lines);

    let special = prompt
        .chars()
        .filter(|c| !c.is_alphanumeric() && !c.is_whitespace())
        .count();
    let punctuation_density = special as f32 / prompt.len().max(1) as f32;

    let indentation_levels = lines
        .iter()
        .map(|l| {
            let stripped = l.trim_start();
            let indent_chars = l.len() - stripped.len();
            indent_chars / 2
        })
        .collect::<HashSet<_>>()
        .len() as u32;

    let has_table = prompt.contains("|---|")
        || prompt.contains("| --- |")
        || lines
            .iter()
            .filter(|l| l.matches('\t').count() >= 2)
            .count()
            >= 2;

    let has_json = !in_code && prompt.contains('{') && prompt.contains(':') && prompt.contains('}');

    let has_xml = prompt.contains("</") || prompt.contains("/>");

    let math_symbol_count = prompt
        .chars()
        .filter(|c| "∑∫√π∞≠≤≥±×÷∈∉∀∃^".contains(*c))
        .count() as u32;

    let parenthetical_depth = compute_paren_depth(prompt);
    let quoted_block_count = count_long_quotes(prompt, 50);

    let section_count = count_sections(&lines);

    StructuralSignals {
        token_count,
        char_count,
        unique_token_ratio,
        code_block_count,
        code_line_count,
        inline_code_count,
        nesting_depth,
        url_count,
        newline_count,
        list_marker_count,
        punctuation_density,
        indentation_levels,
        has_table,
        has_json,
        has_xml,
        math_symbol_count,
        parenthetical_depth,
        quoted_block_count,
        section_count,
    }
}

fn compute_nesting_depth(text: &str) -> u32 {
    let mut max_depth = 0u32;
    let mut current = 0i32;
    for c in text.chars() {
        match c {
            '{' | '[' | '(' => {
                current += 1;
                max_depth = max_depth.max(current as u32);
            }
            '}' | ']' | ')' => {
                current = (current - 1).max(0);
            }
            _ => {}
        }
    }
    max_depth
}

fn compute_paren_depth(text: &str) -> u32 {
    let mut max_depth = 0u32;
    let mut current = 0i32;
    for c in text.chars() {
        match c {
            '(' => {
                current += 1;
                max_depth = max_depth.max(current as u32);
            }
            ')' => {
                current = (current - 1).max(0);
            }
            _ => {}
        }
    }
    max_depth
}

fn count_inline_code(text: &str) -> u32 {
    let mut count = 0u32;
    let mut in_inline = false;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // `i + 3 <= len` (not `i + 2 < len`) so a fence flush at end-of-string
        // with no trailing newline is recognised, not mis-counted (BUG-N04).
        if i + 3 <= bytes.len() && &bytes[i..i + 3] == b"```" {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'`' {
            if in_inline {
                count += 1;
                in_inline = false;
            } else {
                in_inline = true;
            }
        }
        i += 1;
    }
    count
}

fn count_list_markers(lines: &[&str]) -> u32 {
    let mut count = 0u32;
    for line in lines {
        let trimmed = line.trim_start();
        if trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || trimmed.starts_with("\u{2022} ")
            || trimmed.starts_with("\u{2460} ")
        {
            count += 1;
        } else if trimmed.len() >= 3 {
            let bytes = trimmed.as_bytes();
            if bytes[0].is_ascii_digit() && (bytes[1] == b'.' || bytes[1] == b')') {
                count += 1;
            }
        }
    }
    count
}

fn count_long_quotes(text: &str, min_len: usize) -> u32 {
    let mut count = 0u32;
    let mut in_quote = false;
    let mut quote_start = 0;
    let mut quote_char = '"';

    for (i, c) in text.char_indices() {
        if !in_quote && (c == '"' || c == '\'') {
            in_quote = true;
            quote_start = i + c.len_utf8();
            quote_char = c;
        } else if in_quote && c == quote_char {
            if i - quote_start >= min_len {
                count += 1;
            }
            in_quote = false;
        }
    }
    count
}

fn count_sections(lines: &[&str]) -> u32 {
    lines
        .iter()
        .filter(|l| {
            let t = l.trim();
            t.starts_with("---") || t.starts_with("===") || t.starts_with("##")
        })
        .count() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_prompt() {
        let s = extract_structural("");
        assert_eq!(s.token_count, 0);
        assert_eq!(s.code_block_count, 0);
        assert_eq!(s.nesting_depth, 0);
    }

    #[test]
    fn code_blocks_counted() {
        let prompt = "Here:\n```python\ndef foo():\n    pass\n```\nAnd:\n```rust\nfn bar() {}\n```";
        let s = extract_structural(prompt);
        assert_eq!(s.code_block_count, 2);
        assert_eq!(s.code_line_count, 3);
    }

    #[test]
    fn nesting_depth_computed() {
        let s = extract_structural("f(g(h(x)))");
        assert_eq!(s.nesting_depth, 3);
        assert_eq!(s.parenthetical_depth, 3);
    }

    #[test]
    fn list_markers_counted() {
        let prompt = "Tasks:\n- item one\n- item two\n1. numbered\n2. also numbered";
        let s = extract_structural(prompt);
        assert_eq!(s.list_marker_count, 4);
    }

    #[test]
    fn urls_counted() {
        let prompt = "See https://example.com and http://other.com for details";
        let s = extract_structural(prompt);
        assert_eq!(s.url_count, 2);
    }

    #[test]
    fn math_symbols_counted() {
        let prompt = "Prove that ∑(1/n²) = π²/6";
        let s = extract_structural(prompt);
        assert!(s.math_symbol_count >= 2);
    }

    #[test]
    fn table_detected() {
        let prompt = "| Col A | Col B |\n|---|---|\n| 1 | 2 |";
        let s = extract_structural(prompt);
        assert!(s.has_table);
    }

    #[test]
    fn json_detected() {
        let prompt = "Parse this: { \"key\": \"value\" }";
        let s = extract_structural(prompt);
        assert!(s.has_json);
    }

    #[test]
    fn xml_detected() {
        let prompt = "Transform this <root><item/></root>";
        let s = extract_structural(prompt);
        assert!(s.has_xml);
    }

    #[test]
    fn inline_code_counted() {
        let prompt = "Use `foo()` and `bar()` and `baz()`";
        let s = extract_structural(prompt);
        assert_eq!(s.inline_code_count, 3);
    }
}
