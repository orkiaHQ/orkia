// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `version` — the build version, migrated to a native `Command`.
//!
//! `Nothing → List(String)`. Structured `Value` line; the Display sink renders.

use async_trait::async_trait;
use orkia_shell_types::exec::command::{Command, CommandCtx, EvaluatedCall};
use orkia_shell_types::exec::pipeline_data::PipelineData;
use orkia_shell_types::{ExecError, Signature, Type};

use crate::exec::commands::blocks_adapter::blocks_to_value;

pub struct Version;

#[async_trait]
impl Command for Version {
    fn signature(&self) -> Signature {
        Signature::builder("version")
            .io(Type::Nothing, Type::List(Box::new(Type::String)))
            .build()
    }

    fn description(&self) -> &str {
        "show the Orkia version"
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
            orkia_builtin::help::version(),
        )))
    }
}
