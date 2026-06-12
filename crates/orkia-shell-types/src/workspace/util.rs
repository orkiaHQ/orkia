// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

/// Escape a string for safe embedding inside a TOML basic-string (`"..."`).
/// Handles `\` and `"`; control chars are replaced with a space.
pub(super) fn toml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

pub fn slug(title: &str) -> String {
    let s: String = title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let trimmed = s.trim_matches('-').to_string();
    // Collapse runs of '-'
    let mut out = String::with_capacity(trimmed.len());
    let mut prev_dash = false;
    for c in trimmed.chars() {
        if c == '-' {
            if !prev_dash {
                out.push(c);
            }
            prev_dash = true;
        } else {
            out.push(c);
            prev_dash = false;
        }
    }
    if out.is_empty() {
        "untitled".into()
    } else {
        out
    }
}

/// Replace `<field> = "..."` under `<section_header>` (e.g. `[project]`
/// or `[issue]`) if the line exists, otherwise insert it immediately
/// after the header line. Other lines are preserved verbatim. Section
/// boundaries are detected by looking for the next line that starts with
/// `[`. Values are wrapped in double quotes and escaped per TOML rules.
pub(super) fn upsert_toml_field(
    content: &str,
    section_header: &str,
    field: &str,
    value: &str,
) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let mut out = String::with_capacity(content.len() + field.len() + value.len() + 8);
    let mut in_section = false;
    let mut section_seen = false;
    let mut wrote = false;
    let mut header_just_written = false;

    for line in &lines {
        let trimmed = line.trim_start();

        // Exiting the target section without finding the field: insert
        // it just before the next header.
        if in_section && !wrote && trimmed.starts_with('[') && trimmed != section_header {
            out.push_str(field);
            out.push_str(" = \"");
            out.push_str(&toml_escape(value));
            out.push_str("\"\n");
            wrote = true;
            in_section = false;
        }

        if trimmed == section_header {
            in_section = true;
            section_seen = true;
            header_just_written = true;
            out.push_str(line);
            out.push('\n');
            continue;
        }

        if in_section
            && !wrote
            && trimmed.starts_with(field)
            && trimmed[field.len()..].trim_start().starts_with('=')
        {
            out.push_str(field);
            out.push_str(" = \"");
            out.push_str(&toml_escape(value));
            out.push_str("\"\n");
            wrote = true;
            header_just_written = false;
            continue;
        }

        // First non-header line of the target section: if we haven't
        // written yet, insert at the top of the section for stable
        // ordering. We still pass the original line through afterwards.
        if in_section && header_just_written && !wrote {
            // Only insert immediately under the header when the section
            // is otherwise empty (the next line either starts a new
            // section or is blank). Otherwise let the existing
            // ordering carry — we'll insert at section end if no match.
            // Falling through preserves the original line.
        }
        header_just_written = false;

        out.push_str(line);
        out.push('\n');
    }

    if in_section && !wrote {
        out.push_str(field);
        out.push_str(" = \"");
        out.push_str(&toml_escape(value));
        out.push_str("\"\n");
        wrote = true;
    }
    if !section_seen {
        // Caller's content doesn't have the section at all — append.
        out.push('\n');
        out.push_str(section_header);
        out.push('\n');
        out.push_str(field);
        out.push_str(" = \"");
        out.push_str(&toml_escape(value));
        out.push_str("\"\n");
    } else if !wrote {
        // Section seen but never matched, somehow — append at end.
        out.push_str(field);
        out.push_str(" = \"");
        out.push_str(&toml_escape(value));
        out.push_str("\"\n");
    }
    out
}
