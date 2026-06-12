// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! `run_plan` resolves and binds every stage, then type-checks the whole
//! chain, **before** any command's `run` is called (the kernel's "type before
//! run" guarantee). Only then does it execute, threading `PipelineData` by
//! value so each stream has exactly one owner. Streaming commands wrap the
//! upstream lazily; collecting commands drain it inside their own `run`.

use orkia_shell_types::exec::command::CommandCtx;
use orkia_shell_types::exec::pipeline_data::PipelineData;
use orkia_shell_types::{ExecError, ParsedStage, Type};

use crate::exec::eval;
use crate::exec::registry::CommandRegistry;

/// The initial data fed into a plan, plus a label for stage-0 type errors
/// (the producer name, an external command, or an agent).
pub struct PipelineInput {
    pub data: PipelineData,
    pub label: String,
}

/// Run a typed pipeline to completion (or early termination).
pub async fn run_plan(
    stages: &[ParsedStage],
    input: PipelineInput,
    ctx: &CommandCtx,
    registry: &CommandRegistry,
) -> Result<PipelineData, ExecError> {
    // Phases 1+2: resolve, bind args, and type-check the entire chain. No
    // command's `run` is invoked until every stage has passed.
    let mut bound = Vec::with_capacity(stages.len());
    let mut current_type = input.data.type_of();
    let mut upstream = input.label.clone();

    for stage in stages {
        let command = registry
            .get(&stage.name)
            .ok_or_else(|| ExecError::Runtime {
                command: stage.name.clone(),
                message: "command not found in registry".to_string(),
            })?;
        let signature = command.signature();

        let output =
            signature
                .output_for(&current_type)
                .ok_or_else(|| ExecError::TypeMismatch {
                    command: stage.name.clone(),
                    expected: signature
                        .io_types
                        .first()
                        .map(|(input_ty, _)| input_ty.clone())
                        .unwrap_or(Type::Any),
                    got: current_type.clone(),
                    upstream: upstream.clone(),
                })?;
        let next_type = output.clone();

        let call = eval::evaluate(&signature, &stage.name, &stage.raw_args)?;

        bound.push((command, call));
        current_type = next_type;
        upstream = stage.name.clone();
    }

    let mut data = input.data;
    for (command, call) in bound {
        data = command.run(ctx, &call, data).await?;
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::probe::{CountingProducer, ProbeFirst, RequiresArg, TypedConsumer, empty_ctx};
    use futures::stream::StreamExt;
    use orkia_shell_types::ParsedStage;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    fn stage(name: &str, args: &[&str]) -> ParsedStage {
        ParsedStage {
            name: name.to_string(),
            raw_args: args.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn empty_input() -> PipelineInput {
        PipelineInput {
            data: PipelineData::Empty,
            label: "input".to_string(),
        }
    }

    /// A type mismatch must be detected before any `run` is called.
    #[tokio::test]
    async fn type_mismatch_never_runs() {
        let producer = Arc::new(CountingProducer::new(10));
        let consumer = Arc::new(TypedConsumer::new()); // wants String, producer emits Table
        let mut registry = CommandRegistry::new();
        registry.register(producer.clone());
        registry.register(consumer.clone());

        let plan = vec![stage("count", &[]), stage("typed", &[])];
        let result = run_plan(&plan, empty_input(), &empty_ctx(), &registry).await;

        assert!(matches!(result, Err(ExecError::TypeMismatch { .. })));
        assert_eq!(
            producer.produced.load(Ordering::SeqCst),
            0,
            "producer must not run"
        );
        assert_eq!(
            consumer.ran.load(Ordering::SeqCst),
            0,
            "consumer must not run"
        );
    }

    /// A missing required argument is caught during binding — before any run.
    #[tokio::test]
    async fn missing_arg_never_runs() {
        let cmd = Arc::new(RequiresArg::new());
        let mut registry = CommandRegistry::new();
        registry.register(cmd.clone());

        let plan = vec![stage("needs", &[])];
        let result = run_plan(&plan, empty_input(), &empty_ctx(), &registry).await;

        assert!(matches!(result, Err(ExecError::MissingArg { .. })));
        assert_eq!(cmd.ran.load(Ordering::SeqCst), 0, "command must not run");
    }

    /// `first 3` over a 1000-item producer pulls 3 then drops the upstream;
    /// the producer must stop producing (lazy, pull-based cancellation).
    #[tokio::test]
    async fn early_termination_drops_upstream() {
        let producer = Arc::new(CountingProducer::new(1000));
        let first = Arc::new(ProbeFirst);
        let mut registry = CommandRegistry::new();
        registry.register(producer.clone());
        registry.register(first.clone());

        let plan = vec![stage("count", &[]), stage("first", &["3"])];
        let result = run_plan(&plan, empty_input(), &empty_ctx(), &registry)
            .await
            .expect("pipeline runs");

        let items = match result {
            PipelineData::ListStream(mut s) => {
                let mut v = Vec::new();
                while let Some(item) = s.next().await {
                    v.push(item.expect("item"));
                }
                v
            }
            other => panic!("expected ListStream, got {:?}", other.type_of()),
        };
        assert_eq!(items.len(), 3, "first 3 yields 3 items");
        assert_eq!(
            producer.produced.load(Ordering::SeqCst),
            3,
            "producer stopped after the consumer dropped the stream"
        );
    }
}
