// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use super::*;

pub(crate) struct ProjectionCapture {
    source: Option<Arc<dyn orkia_shell_types::FinalResponseSource>>,
    rx: tokio::sync::mpsc::UnboundedReceiver<orkia_shell_types::FinalResponseEvent>,
    agent: String,
    pub(crate) existing_job: Option<JobId>,
    job_id: JobId,
    started_at: SystemTime,
}

impl ProjectionCapture {
    pub(crate) fn for_job(mut self, job_id: JobId) -> Self {
        self.job_id = job_id;
        self
    }

    fn matches(&self, event: &orkia_shell_types::FinalResponseEvent) -> bool {
        event.job_id == self.job_id.0 && event.agent == self.agent
    }

    fn matches_fresh(&self, event: &orkia_shell_types::FinalResponseEvent) -> bool {
        self.matches(event) && final_response_is_fresh(event, self.started_at)
    }
}

impl Repl {
    pub(crate) fn prepare_projection_capture(&self, agent: &str) -> ProjectionCapture {
        let source = self.final_response_source.clone();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        if let Some(source) = &source {
            source.subscribe(Arc::new(move |event| {
                let _ = tx.send(event);
            }));
        }
        ProjectionCapture {
            source,
            rx,
            agent: agent.to_string(),
            existing_job: self.jobs.find_live_agent_by_name(agent),
            job_id: JobId(0),
            started_at: SystemTime::now(),
        }
    }

    pub(crate) async fn wait_for_projection_final_response(
        &self,
        mut capture: ProjectionCapture,
        timeout: Duration,
    ) -> Option<orkia_shell_types::FinalResponseEvent> {
        if let Some(source) = &capture.source
            && let Some(event) = source.latest_for_job(capture.job_id.0)
            && capture.matches_fresh(&event)
        {
            return Some(event);
        }
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => return None,
                maybe_event = capture.rx.recv() => {
                    let event = maybe_event?;
                    if capture.matches(&event) {
                        return Some(event);
                    }
                }
            }
        }
    }
}

fn final_response_is_fresh(
    event: &orkia_shell_types::FinalResponseEvent,
    started_at: SystemTime,
) -> bool {
    let Some(path) = &event.response_path else {
        return false;
    };
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .is_ok_and(|modified| modified >= started_at)
}
