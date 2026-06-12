// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

pub mod attention_modal;
pub mod briefing;
pub mod cockpit;
pub mod cockpit_daemon;
pub mod input_bar;
pub mod invite_modal;
pub mod main_pane;
pub mod share_dialog;
pub mod sidebar;
pub mod source_detail;
pub mod status_bar;
pub mod team_color;
pub mod team_detail;
pub mod team_pane;

pub use attention_modal::{AttentionModalAction, AttentionModalState, render_attention_modal};
pub use briefing::render_briefing;
pub use cockpit::{CockpitModel, render_cockpit};
pub use input_bar::{InputBarView, render_input_bar};
pub use invite_modal::{InviteModalAction, InviteModalState, render_invite_modal};
pub use main_pane::render_main_pane;
pub use share_dialog::{ShareDialogAction, ShareDialogState, ShareSubject, render_share_dialog};
pub use sidebar::render_sidebar;
pub use source_detail::selected_output_detail;
pub use status_bar::render_status_bar;
pub use team_color::hex_to_color;
pub use team_detail::{TeamDetailAction, TeamDetailState, render_team_detail};
pub use team_pane::{TeamPaneAction, TeamPaneState, render_team_pane};
