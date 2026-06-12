// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Runtime light/dark palette — single source of truth.
//!
//! ramp, 5 surface levels, 3 border levels, and unchanged accent /
//! semantic colours. Hierarchy is built from the *grey ramp*, never from
//! font weight (the shell uses only weight 400/500).
//!
//! Every token is a `fn() -> u32` resolving dark/light from one
//! lock-free `AtomicBool` (deliberately not the "global state" the style
//! guide warns about: a per-frame render-time presentation constant, not
//! shared mutable domain state; the UI is single threaded). The legacy
//! token names are kept as *role aliases* onto the new ramp so existing
//! call sites stay correct without a mass rename.

use std::sync::atomic::{AtomicBool, Ordering};

static LIGHT: AtomicBool = AtomicBool::new(false);

/// Select the active palette. Cheap; called once per frame by the shell.
#[inline]
pub fn set_light(light: bool) {
    LIGHT.store(light, Ordering::Relaxed);
}

/// Whether the light palette is active.
#[inline]
pub fn is_light() -> bool {
    LIGHT.load(Ordering::Relaxed)
}

/// Declares each token as `pub fn name() -> u32`, light value when
/// [`is_light`] else dark.
macro_rules! tokens {
    ($( $(#[$m:meta])* $name:ident = ($dark:literal, $light:literal); )*) => {
        $(
            $(#[$m])*
            #[inline]
            pub fn $name() -> u32 { if is_light() { $light } else { $dark } }
        )*
    };
}

tokens! {
    // ── Surfaces (bg.*) ───────────────────────────────────────────────
    /// bg.base — page background.
    app_bg = (0x0a0a0a, 0xfafafa);
    /// bg.panel — sidebar, status bar.
    sidebar_bg = (0x0c0c0d, 0xf4f4f5);
    /// bg.panel — panels / modals chrome.
    panel = (0x0c0c0d, 0xffffff);
    /// bg.raised — tabs bar, cards, raised chrome.
    card = (0x0f0f10, 0xffffff);
    bg_raised = (0x0f0f10, 0xffffff);
    /// bg.hover — row hover.
    card_hover = (0x18181b, 0xf4f4f5);
    bg_hover = (0x18181b, 0xf4f4f5);
    /// bg.active — active row / selected tab.
    bg_active = (0x1a1a1d, 0xe4e4e7);

    // ── Text ramp (text.* — 5 levels) ─────────────────────────────────
    /// text.primary — titles, selected items, salient values.
    t1 = (0xe4e4e7, 0x18181b);
    /// text.secondary — standard body / table cells.
    t2 = (0xa1a1aa, 0x3f3f46);
    /// text.tertiary — labels, inactive nav, helper text.
    text_dim = (0x71717a, 0x71717a);
    /// text.quaternary — column headers, counts, deemphasised meta.
    text_quaternary = (0x52525b, 0xa1a1aa);
    /// text.quinary — disabled, decorative.
    text_quinary = (0x27272a, 0xd4d4d8);

    // ── Borders (border.* — 3 levels) ─────────────────────────────────
    /// border.subtle — row separators.
    soft_border = (0x1a1a1d, 0xe4e4e7);
    /// border.default — section separators, sidebar divider.
    border_default = (0x1f1f23, 0xd4d4d8);
    /// border.strong — focused inputs, emphasised borders.
    border_strong = (0x27272a, 0xa1a1aa);

    // ── Accent (unchanged across modes) ───────────────────────────────
    /// accent.DEFAULT — Orkia violet.
    purple = (0x7f77dd, 0x7f77dd);
    /// accent (active nav text role).
    violet_400 = (0x7f77dd, 0x7f77dd);
    /// accent.muted.
    accent_muted = (0x534ab7, 0x534ab7);
    /// accent wash behind an active/selected element.
    violet_active_bg = (0x1a1a1d, 0xe9e7fa);

    // ── Semantic (unchanged; referenced, never hardcoded) ─────────────
    success = (0x5dcaa5, 0x2f9d7b);
    /// semantic warning (was `amber`).
    amber = (0xef9f27, 0xb9750f);
    /// semantic danger (was `red`).
    red = (0xe24b4a, 0xc23534);
    green = (0x5dcaa5, 0x2f9d7b);
    /// amber wash (compact pill bg — kept for non-dense surfaces).
    amber_dim = (0x3a2e12, 0xfdeccd);
    /// danger wash.
    red_dim = (0x3a1d1d, 0xf8d7d7);

    // ── Legacy POC aliases (mapped onto the ramp above) ───────────────
    bg = (0x0a0a0a, 0xfafafa);
    panel_bg = (0x0c0c0d, 0xffffff);
    border = (0x1f1f23, 0xd4d4d8);
    text_primary = (0xe4e4e7, 0x18181b);
    text_secondary = (0xa1a1aa, 0x3f3f46);
    accent = (0x7f77dd, 0x7f77dd);

    // ── React `Header` topbar palette (mapped onto the ramp) ──────────
    hdr_bg = (0x0c0c0d, 0xffffff); // bar background = bg.panel
    hdr_border = (0x1a1a1d, 0xe4e4e7); // border.subtle
    hdr_accent = (0x7f77dd, 0x7f77dd); // accent
    hdr_tx2 = (0xa1a1aa, 0x3f3f46); // text.secondary
    hdr_tx3 = (0x71717a, 0x71717a); // text.tertiary
    hdr_tx4 = (0x52525b, 0xa1a1aa); // text.quaternary
    hdr_tx5 = (0x27272a, 0xd4d4d8); // separators ≈ text.quinary
    hdr_bgh = (0x1a1a1d, 0xe4e4e7); // active toggle seg = bg.active
    hdr_bg3 = (0x0f0f10, 0xf4f4f5); // toggle track = bg.raised
    hdr_glyph = (0x0a0a0b, 0x0a0a0b); // dark glyph on coloured sprites
    hdr_bgb = (0x0a0a0a, 0xf4f4f5); // deepest surface = bg.base
    shell_cyan = (0x7dd3fc, 0x0284c7); // shell-mode prompt accent

    // ── Per-agent sprite colours (glyph is `hdr_glyph`) ───────────────
    spr_faye = (0xf472b6, 0xf472b6);
    spr_killua = (0xf97316, 0xf97316);
    spr_mina = (0x3b82f6, 0x3b82f6);
}
