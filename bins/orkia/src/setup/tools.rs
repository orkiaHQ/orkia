// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Detect agent-capable CLIs (`claude`, `codex`, `gemini`, `hermes`) on
//! PATH so the wizard can suggest a default for each new agent and
//! warn when none of the preferred tools is installed.

use std::path::PathBuf;

/// CLIs the wizard knows about. Order matters: it's used as the
/// tie-breaker preference when an archetype has no `preferred` list.
pub const KNOWN_TOOLS: &[&str] = &["claude", "codex", "gemini", "hermes"];

#[derive(Debug, Clone)]
pub struct CliTool {
    pub name: String,
    pub path: Option<PathBuf>,
}

impl CliTool {
    pub fn is_found(&self) -> bool {
        self.path.is_some()
    }
}

#[derive(Debug, Clone)]
pub struct DetectedTools {
    pub tools: Vec<CliTool>,
}

impl DetectedTools {
    /// Resolve every entry in `KNOWN_TOOLS` against PATH.
    pub fn scan() -> Self {
        let tools = KNOWN_TOOLS
            .iter()
            .map(|name| CliTool {
                name: (*name).to_string(),
                path: which::which(name).ok(),
            })
            .collect();
        Self { tools }
    }

    pub fn any_found(&self) -> bool {
        self.tools.iter().any(CliTool::is_found)
    }

    /// Best tool for an archetype: first item in `preferred` that is
    /// installed; otherwise the first installed tool by `KNOWN_TOOLS`
    /// order. `None` when nothing is installed.
    pub fn best_tool_for(&self, preferred: &[String]) -> Option<&str> {
        for pref in preferred {
            if let Some(t) = self.tools.iter().find(|t| t.is_found() && t.name == *pref) {
                return Some(&t.name);
            }
        }
        self.tools
            .iter()
            .find(|t| t.is_found())
            .map(|t| t.name.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(name: &str, found: bool) -> CliTool {
        CliTool {
            name: name.into(),
            path: found.then(|| PathBuf::from(format!("/usr/local/bin/{name}"))),
        }
    }

    #[test]
    fn best_tool_prefers_explicit_then_falls_back() {
        let d = DetectedTools {
            tools: vec![t("claude", false), t("codex", true), t("gemini", true)],
        };
        // Preferred list hits codex (claude not installed)
        assert_eq!(
            d.best_tool_for(&["claude".into(), "codex".into()]),
            Some("codex"),
        );
        // No preferences → first installed by KNOWN_TOOLS order
        assert_eq!(d.best_tool_for(&[]), Some("codex"));
    }

    #[test]
    fn best_tool_returns_none_when_nothing_found() {
        let d = DetectedTools {
            tools: KNOWN_TOOLS.iter().map(|n| t(n, false)).collect(),
        };
        assert_eq!(d.best_tool_for(&["claude".into()]), None);
        assert!(!d.any_found());
    }
}
