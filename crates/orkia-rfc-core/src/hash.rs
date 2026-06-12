// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;

/// `sha256:<hex>` over the body bytes of an RFC (post-frontmatter).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentHash(pub String);

impl ContentHash {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

pub fn content_hash_of(body: &str) -> ContentHash {
    let mut h = Sha256::new();
    h.update(body.as_bytes());
    let bytes = h.finalize();
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    ContentHash(format!("sha256:{hex}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable() {
        let a = content_hash_of("hello\nworld\n");
        let b = content_hash_of("hello\nworld\n");
        assert_eq!(a, b);
        assert!(a.as_str().starts_with("sha256:"));
    }

    #[test]
    fn hash_differs_on_change() {
        let a = content_hash_of("foo");
        let b = content_hash_of("foo ");
        assert_ne!(a, b);
    }
}
