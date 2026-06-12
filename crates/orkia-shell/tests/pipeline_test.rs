// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use orkia_shell::error::ShellError;
use orkia_shell::pipeline::parse_pipeline;

#[test]
fn parses_two_stages() {
    let stages = parse_pipeline("@faye do X | @sage review it").expect("ok");
    assert_eq!(stages.len(), 2);
    assert_eq!(stages[0].agent, "faye");
    assert_eq!(stages[0].body, "do X");
    assert_eq!(stages[1].agent, "sage");
    assert_eq!(stages[1].body, "review it");
}

#[test]
fn parses_three_stages() {
    let stages = parse_pipeline("@a do | @b check | @c ship").expect("ok");
    assert_eq!(stages.len(), 3);
    assert_eq!(stages[2].agent, "c");
    assert_eq!(stages[2].body, "ship");
}

#[test]
fn missing_pipe_is_not_a_pipeline() {
    let err = parse_pipeline("@faye do X").unwrap_err();
    assert!(matches!(err, ShellError::NotAPipeline));
}

#[test]
fn stage_missing_at_errors() {
    let err = parse_pipeline("@faye do X | sage review").unwrap_err();
    assert!(matches!(err, ShellError::PipelineMissingAgent(_)));
}
