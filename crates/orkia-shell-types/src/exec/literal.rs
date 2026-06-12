// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Typed-literal parsing for `Filesize` and `Duration`.
//!
//! pins `1mb` == 1_048_576 (binary units), so all size suffixes are 1024-based.

/// Split a literal into its numeric prefix and (lowercased) unit suffix.
/// Returns `None` if there is no numeric prefix or no unit suffix.
fn split_number_unit(input: &str) -> Option<(f64, String)> {
    let split_at =
        input.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+'))?;
    if split_at == 0 {
        return None;
    }
    let (num, unit) = input.split_at(split_at);
    let value: f64 = num.parse().ok()?;
    Some((value, unit.to_ascii_lowercase()))
}

/// Parse a filesize literal (`1mb`, `1kib`, `2gb`, `512b`) into bytes.
/// Size suffixes are binary (1024-based) so `1mb` == 1_048_576.
pub fn parse_filesize(input: &str) -> Option<i64> {
    let (value, unit) = split_number_unit(input)?;
    let multiplier: i64 = match unit.as_str() {
        "b" => 1,
        "kb" | "kib" => 1 << 10,
        "mb" | "mib" => 1 << 20,
        "gb" | "gib" => 1 << 30,
        "tb" | "tib" => 1 << 40,
        _ => return None,
    };
    Some((value * multiplier as f64).round() as i64)
}

/// Parse a duration literal (`5sec`, `2min`, `1hr`, `500ms`) into nanoseconds.
pub fn parse_duration(input: &str) -> Option<i64> {
    let (value, unit) = split_number_unit(input)?;
    const NS: i64 = 1;
    const US: i64 = 1_000;
    const MS: i64 = 1_000_000;
    const SEC: i64 = 1_000_000_000;
    let multiplier: i64 = match unit.as_str() {
        "ns" => NS,
        "us" => US,
        "ms" => MS,
        "s" | "sec" => SEC,
        "min" => 60 * SEC,
        "hr" => 3_600 * SEC,
        "day" => 86_400 * SEC,
        _ => return None,
    };
    Some((value * multiplier as f64).round() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filesize_units() {
        assert_eq!(parse_filesize("1mb"), Some(1_048_576));
        assert_eq!(parse_filesize("1kib"), Some(1024));
        assert_eq!(parse_filesize("2gb"), Some(2 * (1 << 30)));
        assert_eq!(parse_filesize("512b"), Some(512));
    }

    #[test]
    fn duration_units() {
        assert_eq!(parse_duration("5sec"), Some(5_000_000_000));
        assert_eq!(parse_duration("2min"), Some(120_000_000_000));
        assert_eq!(parse_duration("1hr"), Some(3_600_000_000_000));
        assert_eq!(parse_duration("500ms"), Some(500_000_000));
    }

    #[test]
    fn non_literals_reject() {
        assert_eq!(parse_filesize("10"), None);
        assert_eq!(parse_filesize("foo"), None);
        assert_eq!(parse_duration("abc"), None);
        assert_eq!(parse_duration(""), None);
    }
}
