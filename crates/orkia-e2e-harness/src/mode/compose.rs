// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Attach to a running `docker-compose.test.yml` stack.
//!
//! The compose stack is brought up out-of-band (by CI or the developer
//! via `docker-compose -f docker-compose.test.yml up -d`). This module
//! waits for backend health, opens a `sqlx::PgPool`, and hands back an
//! [`OrkiaSession`] ready for flow code.

use std::time::{Duration, Instant};

use sqlx::postgres::PgPoolOptions;

use crate::error::HarnessError;
use crate::session::{OrkiaSession, SessionInner};

/// Backend HTTP URL. Override with `ORKIA_E2E_BACKEND_URL`.
const DEFAULT_BACKEND_URL: &str = "http://localhost:8080";
/// Postgres URL. Override with `ORKIA_E2E_DATABASE_URL`.
/// **Test-only credential** for the local docker-compose stack (localhost,
/// ephemeral DB). Never used in production. Not a secret.
const DEFAULT_DATABASE_URL: &str = "postgres://orkia:orkia_test@localhost:5432/orkia_test";

/// Total wait budget for backend `/health` to return 200.
const HEALTH_WAIT: Duration = Duration::from_secs(30);
/// Poll interval while waiting on `/health`.
const HEALTH_POLL: Duration = Duration::from_millis(500);

pub async fn start_compose() -> crate::Result<OrkiaSession> {
    start_compose_with_env(crate::env::FlowEnv::free()).await
}

pub async fn start_compose_with_env(env: crate::env::FlowEnv) -> crate::Result<OrkiaSession> {
    let backend_url =
        std::env::var("ORKIA_E2E_BACKEND_URL").unwrap_or_else(|_| DEFAULT_BACKEND_URL.into());
    let database_url =
        std::env::var("ORKIA_E2E_DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.into());

    wait_for_backend_health(&backend_url).await?;

    let db_pool = PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await?;

    Ok(OrkiaSession::from_compose(
        SessionInner {
            backend_url,
            db_pool,
        },
        env,
    ))
}

async fn wait_for_backend_health(backend_url: &str) -> crate::Result<()> {
    let url = format!("{}/health", backend_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?;
    let deadline = Instant::now() + HEALTH_WAIT;
    let mut last_err: Option<String> = None;
    while Instant::now() < deadline {
        match client.get(&url).send().await {
            Ok(r) if r.status().is_success() => return Ok(()),
            Ok(r) => last_err = Some(format!("status {}", r.status())),
            Err(e) => last_err = Some(e.to_string()),
        }
        tokio::time::sleep(HEALTH_POLL).await;
    }
    Err(HarnessError::Infra(format!(
        "backend {url} did not become healthy within {}s (last: {})",
        HEALTH_WAIT.as_secs(),
        last_err.unwrap_or_else(|| "no response".into())
    )))
}
