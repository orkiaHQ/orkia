// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

use thiserror::Error;

/// Human-readable description of the allowed name shape: kebab-case, must
pub const NAME_PATTERN: &str = "^[a-z][a-z0-9-]{1,49}$";

#[derive(Debug, Error)]
pub enum NameError {
    #[error("forge.name is empty")]
    Empty,
    #[error("forge.name must be 2-50 chars, got {0}")]
    BadLength(usize),
    #[error("forge.name must start with a lowercase letter")]
    BadStart,
    #[error("forge.name may only contain lowercase letters, digits, and '-'")]
    BadChar,
}

/// Validate a forge app name against `NAME_PATTERN` without pulling in regex.
pub fn validate_app_name(name: &str) -> Result<(), NameError> {
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    let len = name.chars().count();
    if !(2..=50).contains(&len) {
        return Err(NameError::BadLength(len));
    }
    let mut chars = name.chars();
    let first = chars.next().ok_or(NameError::Empty)?;
    if !first.is_ascii_lowercase() {
        return Err(NameError::BadStart);
    }
    for c in chars {
        let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
        if !ok {
            return Err(NameError::BadChar);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_canonical_names() {
        validate_app_name("hello-orkia").unwrap();
        validate_app_name("datadog-budget").unwrap();
        validate_app_name("ab").unwrap();
    }

    #[test]
    fn rejects_bad_start() {
        assert!(matches!(
            validate_app_name("1hello"),
            Err(NameError::BadStart)
        ));
        assert!(matches!(
            validate_app_name("-hello"),
            Err(NameError::BadStart)
        ));
        assert!(matches!(
            validate_app_name("Hello"),
            Err(NameError::BadStart)
        ));
    }

    #[test]
    fn rejects_bad_chars() {
        assert!(matches!(
            validate_app_name("hello_world"),
            Err(NameError::BadChar)
        ));
        assert!(matches!(
            validate_app_name("hello world"),
            Err(NameError::BadChar)
        ));
    }

    #[test]
    fn rejects_bad_length() {
        assert!(matches!(
            validate_app_name("a"),
            Err(NameError::BadLength(1))
        ));
        assert!(matches!(
            validate_app_name(&"a".repeat(51)),
            Err(NameError::BadLength(51))
        ));
    }
}
