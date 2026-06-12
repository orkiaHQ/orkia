// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `briefing` — the session briefing, migrated to a native `Command`.
//!
//! `Nothing → List(String)`. A placeholder generator today ("no sessions
//! recorded yet"); it stays a real Command so the future implementation slots
//! in without re-plumbing.

use async_trait::async_trait;
use orkia_shell_types::exec::command::{Command, CommandCtx, EvaluatedCall};
use orkia_shell_types::exec::pipeline_data::PipelineData;
use orkia_shell_types::{ExecError, Signature, Type};

use crate::exec::commands::blocks_adapter::blocks_to_value;

pub struct Briefing;

#[async_trait]
impl Command for Briefing {
    fn signature(&self) -> Signature {
        Signature::builder("briefing")
            .io(Type::Nothing, Type::List(Box::new(Type::String)))
            .build()
    }

    fn description(&self) -> &str {
        "show the session briefing"
    }

    fn is_streaming(&self) -> bool {
        false
    }

    async fn run(
        &self,
        _ctx: &CommandCtx,
        _call: &EvaluatedCall,
        _input: PipelineData,
    ) -> Result<PipelineData, ExecError> {
        Ok(PipelineData::Value(blocks_to_value(
            orkia_builtin::briefing::briefing(),
        )))
    }
}
