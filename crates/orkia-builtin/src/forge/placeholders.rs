// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Hard-coded V0 placeholder app payload. Real generation lands in V1.
//!
//! The HTML is intentionally minimal — a single beige rectangle saying
//! "hello, forge" — so reviewers see a working window without us shipping
//! a half-finished design system.

use super::validate::ValidatedForge;

pub fn app_html(v: &ValidatedForge) -> String {
    let title = escape_html(&v.window.title);
    let name = escape_html(&v.name);
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>{title}</title>
  <link rel="stylesheet" href="app.css" />
</head>
<body>
  <main>
    <h1>hello, forge</h1>
    <p>this is the V0 placeholder for <code>{name}</code>.</p>
    <p>real generation lands in V1.</p>
  </main>
  <script src="app.js"></script>
</body>
</html>
"#
    )
}

pub fn app_css() -> &'static str {
    r#"html, body {
    margin: 0;
    padding: 0;
    height: 100%;
    background: #f5efe6;
    color: #2b2b2b;
    font-family: ui-sans-serif, system-ui, -apple-system, sans-serif;
}

main {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    height: 100%;
    text-align: center;
    padding: 1rem;
}

h1 {
    font-weight: 600;
    margin: 0 0 0.5rem 0;
}

code {
    background: rgba(0, 0, 0, 0.06);
    padding: 0.1rem 0.35rem;
    border-radius: 0.25rem;
}
"#
}

pub fn app_js() -> &'static str {
    r#"// V0 placeholder. Real logic lands when forge generates the app.
// `window.orkia.v1` is injected by the viewer at window setup.
(function () {
    if (typeof window === "undefined") return;
    document.addEventListener("DOMContentLoaded", function () {
        if (window.orkia && window.orkia.v1 && window.orkia.v1.app) {
            console.log("orkia forge app:", window.orkia.v1.app.name());
        }
    });
})();
"#
}

/// to match the placeholder HTML's body background. V3 ships the real
/// design-system icons; this just gives us a correctly-sized artifact
/// so per-app bundles (V3) don't have to backfill the size.
pub fn default_icon_png() -> &'static [u8] {
    include_bytes!("default-icon.png")
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_escapes_user_input() {
        let v = ValidatedForge {
            name: "x".into(),
            description: String::new(),
            icon: "default".into(),
            window: orkia_forge_types::WindowConfig {
                title: "<script>alert(1)</script>".into(),
                width: 480,
                height: 320,
                resizable: true,
            },
            permissions: orkia_forge_types::Permissions::default(),
        };
        let html = app_html(&v);
        assert!(html.contains("&lt;script&gt;"));
        assert!(!html.contains("<script>alert"));
    }
}
