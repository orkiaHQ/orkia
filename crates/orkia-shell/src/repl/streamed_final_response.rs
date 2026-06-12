// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Passive REPL-side [`FinalResponseSource`] for subscribed mode
//!
//! When the pty-daemon hosts the journal hub, the authoritative
//! `FinalResponseService` (transcript extraction + disk persist + AFR
//! emission) runs daemon-side so pipeline capture survives a REPL restart
//! (see [[project_pipeline_capture_is_stop_hook]]). The resulting
//! `AgentFinalResponse` envelopes stream back to the REPL like any other.
//!
//! The REPL still needs a `FinalResponseSource` for in-process projection
//! capture (`operator_capture`: `@a | shell`, RFC delegation). This passive
//! source reconstructs the typed [`FinalResponseEvent`] from each streamed
//! AFR envelope, caches the latest per job, and fires subscribers — no
//! extraction, no disk I/O, no second AFR emission.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use orkia_shell_types::{
    FinalResponseCallback, FinalResponseEvent, FinalResponseSource, JournalEnvelope,
};

/// Cap on remembered per-job latest events. Projection capture only ever
/// reads the most recent for a single job; a small FIFO bounds memory
/// against a long-lived shell that has spawned many agents.
const LATEST_CAP: usize = 64;

/// REPL-side source fed by daemon-streamed `AgentFinalResponse` envelopes.
/// Public so the binary's pipeline wiring can hand the stage executor a
/// passive source in subscribed mode (the bundle's own FRS never sees a
/// `Stop` there — the daemon owns the stop hook).
#[derive(Default)]
pub struct StreamedFinalResponseSource {
    subscribers: Mutex<Vec<FinalResponseCallback>>,
    latest: Mutex<LatestCache>,
}

impl StreamedFinalResponseSource {
    /// Feed one streamed envelope. Reconstructs the typed event from an
    /// `AgentFinalResponse`, records it as latest-for-job, and fires
    /// subscribers. Ignores non-AFR envelopes and AFRs without a `job_id`
    /// (treat every byte as untrusted — #7).
    pub fn ingest(&self, env: &JournalEnvelope) {
        if env.event.as_deref() != Some("AgentFinalResponse") {
            return;
        }
        let Some(job_id) = env.job_id else {
            return;
        };
        let event = FinalResponseEvent {
            job_id,
            agent: env.agent.clone().unwrap_or_default(),
            session_id: env.session_id.clone(),
            response_path: env.response_path.clone().map(std::path::PathBuf::from),
            response_sha256: env.response_sha256.clone(),
            response_bytes: env.response_bytes.unwrap_or(0),
            response_preview: env.response_preview.clone().unwrap_or_default(),
        };
        if let Ok(mut latest) = self.latest.lock() {
            latest.put(job_id, event.clone());
        }
        if let Ok(subs) = self.subscribers.lock() {
            for cb in subs.iter() {
                cb(event.clone());
            }
        }
    }
}

impl FinalResponseSource for StreamedFinalResponseSource {
    fn subscribe(&self, callback: FinalResponseCallback) {
        if let Ok(mut subs) = self.subscribers.lock() {
            subs.push(callback);
        }
    }

    fn latest_for_job(&self, job_id: u32) -> Option<FinalResponseEvent> {
        self.latest.lock().ok().and_then(|g| g.get(job_id))
    }

    fn ingest_streamed(&self, env: &JournalEnvelope) {
        self.ingest(env);
    }
}

/// Tiny FIFO cache keyed by `job_id` — same shape as the daemon FRS's
/// `LatestCache`, minimal because the access pattern is a single lookup.
#[derive(Default)]
struct LatestCache {
    order: VecDeque<u32>,
    map: HashMap<u32, FinalResponseEvent>,
}

impl LatestCache {
    fn put(&mut self, job_id: u32, event: FinalResponseEvent) {
        if self.map.insert(job_id, event).is_none() {
            self.order.push_back(job_id);
            if self.order.len() > LATEST_CAP
                && let Some(old) = self.order.pop_front()
            {
                self.map.remove(&old);
            }
        }
    }

    fn get(&self, job_id: u32) -> Option<FinalResponseEvent> {
        self.map.get(&job_id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_shell_types::EventType;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn afr(job_id: u32, preview: &str) -> JournalEnvelope {
        let mut env = JournalEnvelope::now(EventType::Hook);
        env.event = Some("AgentFinalResponse".into());
        env.job_id = Some(job_id);
        env.agent = Some("faye".into());
        env.response_preview = Some(preview.into());
        env.response_bytes = Some(preview.len() as u64);
        env
    }

    #[test]
    fn ingest_caches_latest_for_job() {
        let src = StreamedFinalResponseSource::default();
        src.ingest(&afr(7, "hello"));
        let got = src.latest_for_job(7).expect("event cached");
        assert_eq!(got.agent, "faye");
        assert_eq!(got.response_preview, "hello");
        assert!(src.latest_for_job(8).is_none());
    }

    #[test]
    fn ingest_fires_subscribers() {
        let src = StreamedFinalResponseSource::default();
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        src.subscribe(Arc::new(move |_e| {
            h.fetch_add(1, Ordering::SeqCst);
        }));
        src.ingest(&afr(1, "a"));
        src.ingest(&afr(1, "b"));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn ignores_non_afr_and_jobless() {
        let src = StreamedFinalResponseSource::default();
        let mut other = JournalEnvelope::now(EventType::Hook);
        other.event = Some("Stop".into());
        other.job_id = Some(3);
        src.ingest(&other);
        assert!(src.latest_for_job(3).is_none());

        let mut jobless = afr(0, "x");
        jobless.job_id = None;
        src.ingest(&jobless);
        assert!(src.latest_for_job(0).is_none());
    }
}
