// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

use ratatui::style::Color;
use std::collections::HashMap;

pub struct Theme {
    pub fg: Color,
    pub dim: Color,
    pub accent: Color,
    pub green: Color,
    pub yellow: Color,
    pub red: Color,
    pub blue: Color,
    pub border: Color,
    /// Whole-frame background.
    pub bg: Color,
    /// Elevated surfaces (input card, modals) — one step lighter than `bg`.
    pub bg_elevated: Color,
    /// Sidebar background — sits between `bg` and `bg_elevated`.
    pub bg_sidebar: Color,
    /// Highlight behind a selected card's header.
    pub bg_selected: Color,
    pub agent_colors: HashMap<String, Color>,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            fg: Color::Rgb(200, 200, 200),
            dim: Color::Rgb(110, 110, 110),
            accent: Color::Rgb(139, 108, 239),
            green: Color::Rgb(74, 222, 128),
            yellow: Color::Rgb(250, 204, 21),
            red: Color::Rgb(248, 113, 113),
            blue: Color::Rgb(96, 165, 250),
            border: Color::Rgb(60, 60, 70),
            bg: Color::Rgb(22, 23, 29),
            bg_elevated: Color::Rgb(30, 32, 39),
            bg_sidebar: Color::Rgb(26, 27, 34),
            bg_selected: Color::Rgb(45, 42, 64),
            agent_colors: HashMap::new(),
        }
    }
}

impl Theme {
    pub fn agent_color(&self, name: &str) -> Color {
        self.agent_colors.get(name).copied().unwrap_or(self.accent)
    }

    pub fn tool_color(&self, tool: &str) -> Color {
        match tool {
            "Read" => self.blue,
            "Write" => self.green,
            "Bash" => self.yellow,
            _ => self.fg,
        }
    }

    pub fn status_color(&self, status: &str) -> Color {
        match status {
            "done" | "active" => self.green,
            "blocked" | "failed" | "error" => self.red,
            "in_progress" | "running" | "draft" => self.yellow,
            _ => self.dim,
        }
    }
}
