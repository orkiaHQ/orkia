// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `whoami` — account, plan, kernel, and capabilities, migrated to a native
//! handle de service `AuthView`).
//!
//! `Nothing → List(String)`. The first line is the system username
//! superset rule: the builtin must include the system answer. Identity
//! state then follows through `ctx.auth` (the lightweight `AuthView`
//! handle) — the auth/capability impl stays in `orkia-shell`, the command
//! only sees rendered lines, and returns `PipelineData` (C1). Absent
//! handle → username only.

use async_trait::async_trait;
use orkia_shell_types::exec::command::{Command, CommandCtx, EvaluatedCall};
use orkia_shell_types::exec::pipeline_data::PipelineData;
use orkia_shell_types::{ExecError, Signature, Type, Value};

pub struct Whoami;

#[async_trait]
impl Command for Whoami {
    fn signature(&self) -> Signature {
        Signature::builder("whoami")
            .io(Type::Nothing, Type::List(Box::new(Type::String)))
            .build()
    }

    fn description(&self) -> &str {
        "show the signed-in account, plan, and capabilities"
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
        let username = std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .unwrap_or_else(|_| "unknown".into());
        let mut lines = vec![username];
        lines.extend(
            ctx.auth
                .as_ref()
                .map(|a| a.whoami_lines())
                .unwrap_or_default(),
        );
        Ok(PipelineData::Value(Value::List(
            lines.into_iter().map(Value::String).collect(),
        )))
    }
}
