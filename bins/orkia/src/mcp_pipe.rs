// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `orkia mcp-pipe`: stdio MCP server exposing `submit_pipeline_output`.
//!
//! Spawned per pipeline stage from the stage agent's generated
//! `mcp-config.json` as the structured output safety net. The canonical
//! capture path is the `Stop` hook → final-response channel; this tool
//! lets a cooperative agent hand off earlier. All context comes from env
//! (`ORKIA_PIPELINE_ID` / `ORKIA_STAGE_INDEX` / `ORKIA_JOB_ID` /
//! `ORKIA_AGENT_NAME` / `ORKIA_RUN_DIR` / `ORKIA_SOCKET_PATH`).
//!
//! The protocol implementation lives in the `orkia-mcp-pipe-server`
//! crate; this module is the thin CLI entry that reads env and runs the
//! stdio loop, mirroring `mcp_bridge::run`.

use orkia_mcp_pipe_server::{Server, ServerEnv, run_stdio};

pub(crate) async fn run() -> i32 {
    let env = match ServerEnv::from_env() {
        Ok(env) => env,
        Err(e) => {
            eprintln!("orkia mcp-pipe: {e}");
            return 2;
        }
    };
    match run_stdio(Server::new(env)).await {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("orkia mcp-pipe: {e}");
            1
        }
    }
}
