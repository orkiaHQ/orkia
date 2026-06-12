// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Hex → `ratatui::Color` conversion shared by every team widget.
//!
//! Accepts `#RRGGBB` or `RRGGBB`. Anything else returns `None` and the

use ratatui::style::Color;

pub fn hex_to_color(hex: &str) -> Option<Color> {
    let trimmed = hex.trim().trim_start_matches('#');
    // `len()` counts bytes; require ASCII so the six bytes are six chars and
    // the `[0..2]`/`[2..4]`/`[4..6]` slices can never split a multibyte char.
    if trimmed.len() != 6 || !trimmed.is_ascii() {
        return None;
    }
    let r = u8::from_str_radix(&trimmed[0..2], 16).ok()?;
    let g = u8::from_str_radix(&trimmed[2..4], 16).ok()?;
    let b = u8::from_str_radix(&trimmed[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_with_or_without_hash() {
        assert_eq!(hex_to_color("#ff5733"), Some(Color::Rgb(0xff, 0x57, 0x33)));
        assert_eq!(hex_to_color("FF5733"), Some(Color::Rgb(0xff, 0x57, 0x33)));
    }

    #[test]
    fn rejects_malformed() {
        assert!(hex_to_color("").is_none());
        assert!(hex_to_color("#fff").is_none());
        assert!(hex_to_color("not-hex").is_none());
        assert!(hex_to_color("#ggggGG").is_none());
        // 6-byte multibyte string: must not panic on a non-char-boundary slice.
        assert!(hex_to_color("€€").is_none());
    }

    #[test]
    fn trims_whitespace() {
        assert_eq!(
            hex_to_color("  #112233  "),
            Some(Color::Rgb(0x11, 0x22, 0x33))
        );
    }
}
