// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::types::RfcFrontmatter;
use super::util::toml_escape;

// ─── Rfc frontmatter parsing ────────────────────────────────────────────────

pub fn parse_rfc_frontmatter(content: &str) -> (Option<RfcFrontmatter>, &str) {
    // Delegate delimiter handling to `split_frontmatter` so this matches the
    // canonical newline-anchored `\n+++` close. The old `rest.find("+++")`
    // closed at the first `+++` anywhere (e.g. in a pasted diff), truncating
    // the frontmatter and mis-delimiting the body (BUG-043).
    match split_frontmatter(content) {
        Some((toml_str, body)) => (toml::from_str(toml_str.trim()).ok(), body),
        None => (None, content),
    }
}

// ─── Frontmatter mutation helpers ───────────────────────────────────────────

/// Split a `+++…+++` RFC into `(frontmatter_body, after_close_including_newline)`.
pub(super) fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let rest = content.strip_prefix("+++")?;
    let rest = rest.trim_start_matches(['\n', '\r']);
    let end = rest.find("\n+++")?;
    let fm = &rest[..end];
    // After "\n+++" — skip the marker line and one trailing newline if present.
    let after = &rest[end + 4..];
    let after = after.strip_prefix('\n').unwrap_or(after);
    Some((fm, after))
}

pub(super) fn frontmatter_key_matches(line: &str, key: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with(key) && trimmed[key.len()..].trim_start().starts_with('=')
}

pub(super) fn read_frontmatter_field(fm: &str, key: &str) -> Option<String> {
    for line in fm.lines() {
        if !frontmatter_key_matches(line, key) {
            continue;
        }
        let after_eq = line.split_once('=').map(|(_, v)| v.trim())?;
        if let Some(stripped) = after_eq.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
            return Some(stripped.to_string());
        }
        if after_eq.starts_with('[') && after_eq.ends_with(']') {
            let inner = &after_eq[1..after_eq.len() - 1];
            let items: Vec<String> = inner
                .split(',')
                .map(|t| t.trim().trim_matches('"').to_string())
                .filter(|t| !t.is_empty())
                .collect();
            return Some(items.join(","));
        }
        return Some(after_eq.to_string());
    }
    None
}

pub(super) fn rewrite_frontmatter_scalar(fm: &str, key: &str, value: &str) -> String {
    let mut out = String::with_capacity(fm.len());
    let mut replaced = false;
    for line in fm.lines() {
        if !replaced && frontmatter_key_matches(line, key) {
            out.push_str(key);
            out.push_str(" = \"");
            out.push_str(&toml_escape(value));
            out.push('"');
            out.push('\n');
            replaced = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !replaced {
        out.push_str(key);
        out.push_str(" = \"");
        out.push_str(&toml_escape(value));
        out.push_str("\"\n");
    }
    out
}

pub(super) fn rewrite_frontmatter_array(fm: &str, key: &str, items: &[String]) -> String {
    let rendered_items = items
        .iter()
        .map(|i| format!("\"{}\"", toml_escape(i)))
        .collect::<Vec<_>>()
        .join(", ");
    let mut out = String::with_capacity(fm.len());
    let mut replaced = false;
    for line in fm.lines() {
        if !replaced && frontmatter_key_matches(line, key) {
            out.push_str(key);
            out.push_str(" = [");
            out.push_str(&rendered_items);
            out.push_str("]\n");
            replaced = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !replaced {
        out.push_str(key);
        out.push_str(" = [");
        out.push_str(&rendered_items);
        out.push_str("]\n");
    }
    out
}
