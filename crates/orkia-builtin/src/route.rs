// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use orkia_shell_types::BlockContent;

pub fn route(_args: &[String]) -> Vec<BlockContent> {
    vec![
        BlockContent::SystemInfo(" ROUTING TABLE — no learned rules yet".into()),
        BlockContent::Text(" route add     add a manual rule".into()),
        BlockContent::Text(" route suspend suspend a rule".into()),
        BlockContent::Text(" route reset   clear learned rules".into()),
    ]
}
