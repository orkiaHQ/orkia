// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Test-only command doubles used to prove engine invariants
//! (type-check-before-run, early-termination via drop, arg-error-before-run).
//! Not compiled outside tests.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use indexmap::IndexMap;
use orkia_shell_types::exec::command::{Command, CommandCtx, EvaluatedCall};
use orkia_shell_types::exec::pipeline_data::PipelineData;
use orkia_shell_types::{ExecError, PositionalArg, Signature, Type, Value};

/// A bare context for tests.
pub fn empty_ctx() -> CommandCtx {
    CommandCtx {
        cwd: PathBuf::from("."),
        env: HashMap::new(),
        data_dir: PathBuf::from("."),
        agents: Vec::new(),
        jobs: Vec::new(),
        journal: None,
        auth: None,
        attention: Vec::new(),
        attention_control: None,
        capabilities: orkia_shell_types::CapabilitySet::shell_default(),
    }
}

/// Producer (`Nothing → Table`) emitting up to `max` records lazily, counting
/// each record it actually produces.
pub struct CountingProducer {
    pub produced: Arc<AtomicUsize>,
    max: usize,
}

impl CountingProducer {
    pub fn new(max: usize) -> Self {
        Self {
            produced: Arc::new(AtomicUsize::new(0)),
            max,
        }
    }
}

#[async_trait]
impl Command for CountingProducer {
    fn signature(&self) -> Signature {
        Signature::builder("count")
            .io(Type::Nothing, Type::Table)
            .build()
    }
    fn description(&self) -> &str {
        "test producer"
    }
    async fn run(
        &self,
        _ctx: &CommandCtx,
        _call: &EvaluatedCall,
        _input: PipelineData,
    ) -> Result<PipelineData, ExecError> {
        let counter = self.produced.clone();
        let max = self.max;
        let stream = stream::unfold(0usize, move |i| {
            let counter = counter.clone();
            async move {
                if i >= max {
                    return None;
                }
                counter.fetch_add(1, Ordering::SeqCst);
                let mut record = IndexMap::new();
                record.insert("n".to_string(), Value::Int(i as i64));
                Some((Ok(Value::Record(record)), i + 1))
            }
        });
        Ok(PipelineData::ListStream(stream.boxed()))
    }
}

/// Early-termination consumer: pulls `count` items then drops the upstream.
pub struct ProbeFirst;

#[async_trait]
impl Command for ProbeFirst {
    fn signature(&self) -> Signature {
        Signature::builder("first")
            .io(Type::Table, Type::Table)
            .required(PositionalArg::new("count", Type::Int, "items to take"))
            .build()
    }
    fn description(&self) -> &str {
        "test first"
    }
    async fn run(
        &self,
        _ctx: &CommandCtx,
        call: &EvaluatedCall,
        input: PipelineData,
    ) -> Result<PipelineData, ExecError> {
        let count = match call.req(0)? {
            Value::Int(n) => *n as usize,
            _ => 0,
        };
        let mut taken = Vec::new();
        if let PipelineData::ListStream(mut upstream) = input {
            for _ in 0..count {
                match upstream.next().await {
                    Some(item) => taken.push(item?),
                    None => break,
                }
            }
            drop(upstream); // cancel the producer
        }
        Ok(PipelineData::ListStream(
            stream::iter(taken.into_iter().map(Ok)).boxed(),
        ))
    }
}

/// Consumer that only accepts `String` input — used to force a type mismatch.
#[derive(Default)]
pub struct TypedConsumer {
    pub ran: Arc<AtomicUsize>,
}

impl TypedConsumer {
    pub fn new() -> Self {
        Self {
            ran: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl Command for TypedConsumer {
    fn signature(&self) -> Signature {
        Signature::builder("typed")
            .io(Type::String, Type::String)
            .build()
    }
    fn description(&self) -> &str {
        "test typed consumer"
    }
    async fn run(
        &self,
        _ctx: &CommandCtx,
        _call: &EvaluatedCall,
        input: PipelineData,
    ) -> Result<PipelineData, ExecError> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        Ok(input)
    }
}

/// Command with a required argument — used to force a binding error.
#[derive(Default)]
pub struct RequiresArg {
    pub ran: Arc<AtomicUsize>,
}

impl RequiresArg {
    pub fn new() -> Self {
        Self {
            ran: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl Command for RequiresArg {
    fn signature(&self) -> Signature {
        Signature::builder("needs")
            .io(Type::Any, Type::Any)
            .required(PositionalArg::new("x", Type::String, "needed"))
            .build()
    }
    fn description(&self) -> &str {
        "test requires arg"
    }
    async fn run(
        &self,
        _ctx: &CommandCtx,
        _call: &EvaluatedCall,
        input: PipelineData,
    ) -> Result<PipelineData, ExecError> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        Ok(input)
    }
}
