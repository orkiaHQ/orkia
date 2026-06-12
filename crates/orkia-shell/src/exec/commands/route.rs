// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `route` — the agent routing table, migrated to a native `Command`.
//!
//! `Nothing → List(String)`. The legacy generator ignores its args (the
//! `add`/`suspend`/`reset` subcommands are unimplemented in the legacy too),
//! so no behaviour is lost; subcommand flags are a later wave.

use async_trait::async_trait;
use orkia_shell_types::exec::command::{Command, CommandCtx, EvaluatedCall};
use orkia_shell_types::exec::pipeline_data::PipelineData;
use orkia_shell_types::{ExecError, Signature, Type};

use crate::exec::commands::blocks_adapter::blocks_to_value;

pub struct Route;

#[async_trait]
impl Command for Route {
    fn signature(&self) -> Signature {
        Signature::builder("route")
            .io(Type::Nothing, Type::List(Box::new(Type::String)))
            .build()
    }

    fn description(&self) -> &str {
        "show the agent routing table"
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
            orkia_builtin::route::route(&[]),
        )))
    }
}
