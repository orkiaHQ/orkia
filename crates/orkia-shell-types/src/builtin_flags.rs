// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PsFlags {
    pub show_agents: bool,
    pub show_system: bool,
    pub full: bool,
    pub json: bool,
}

impl Default for PsFlags {
    fn default() -> Self {
        Self {
            show_agents: true,
            show_system: true,
            full: false,
            json: false,
        }
    }
}

impl PsFlags {
    /// flags (`-a`, `-ef`, …) belong to the system `ps` and route the
    /// bare line to brush before this parser is ever reached.
    /// `--agents` agents-only, `--system` system-only, `--full`, `--json`.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut flags = Self::default();
        for arg in args {
            match arg.as_str() {
                "--agents" => {
                    flags.show_agents = true;
                    flags.show_system = false;
                }
                "--system" => {
                    flags.show_agents = false;
                    flags.show_system = true;
                }
                "--full" => flags.full = true,
                "--json" => flags.json = true,
                other => return Err(format!("ps: unknown flag '{other}'")),
            }
        }
        Ok(flags)
    }
}
