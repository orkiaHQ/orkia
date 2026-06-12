// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `plan` — the plan tier and unlocked capabilities, migrated to a native
//! handle `AuthView`).
//!
//! `Nothing → List(String)`. Same `ctx.auth` handle as `whoami`; returns
//! `PipelineData` (C1). Absent handle → empty output.

use async_trait::async_trait;
use orkia_shell_types::exec::command::{Command, CommandCtx, EvaluatedCall};
use orkia_shell_types::exec::pipeline_data::PipelineData;
use orkia_shell_types::{ExecError, Signature, Type, Value};

pub struct Plan;

#[async_trait]
impl Command for Plan {
    fn signature(&self) -> Signature {
        Signature::builder("plan")
            .io(Type::Nothing, Type::List(Box::new(Type::String)))
            .build()
    }

    fn description(&self) -> &str {
        "show the current plan tier and unlocked capabilities"
    }

    fn is_streaming(&self) -> bool {
        false
    }

    async fn run(
        &self,
        ctx: &CommandCtx,
        _call: &EvaluatedCall,
        _input: PipelineData,
    ) -> Result<PipelineData, ExecError> {
        let lines = ctx
            .auth
            .as_ref()
            .map(|a| a.plan_lines())
            .unwrap_or_default();
        Ok(PipelineData::Value(Value::List(
            lines.into_iter().map(Value::String).collect(),
        )))
    }
}
