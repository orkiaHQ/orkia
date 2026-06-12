// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

//! Async task host for the pty-daemon process.
//!
//! `TaskHost` wraps a `tokio::runtime::Handle` so that future consumers (journal
//! hub, SEAL consumer, final-response subscriber) can be spawned as long-lived
//! async tasks without changing the synchronous accept loop.
//!
//! # Design notes
//!
//! The underlying `Runtime` is owned by the caller (`run_server`), not by this
//! struct, so drop ordering is explicit: tasks are aborted first, then the
//! runtime shuts down, then the sync actor is joined, then the socket file is
//! removed.  This matches the existing cleanup sequence.

use std::future::Future;

use tokio::runtime::Handle;
use tokio::task::{AbortHandle, JoinHandle};

/// Host for async tasks running inside the pty-daemon.
///
/// Create with [`TaskHost::new`] and call [`TaskHost::shutdown`] before
/// dropping the `tokio::runtime::Runtime` that backs the handle.
pub(super) struct TaskHost {
    // SEAL consumer, final-response subscriber).  Suppressing dead-code here
    // is intentional: the handle is retained for those future consumers.
    #[allow(dead_code)]
    handle: Handle,
    /// Abort handles for all spawned tasks. `AbortHandle` is `Clone` and does
    /// not require owning the `JoinHandle`, so callers keep full ownership of
    /// the join handle while we retain the ability to cancel.
    abort_handles: Vec<AbortHandle>,
}

impl TaskHost {
    /// Create a new `TaskHost` bound to `handle`.
    pub(super) fn new(handle: Handle) -> Self {
        Self {
            handle,
            abort_handles: Vec::new(),
        }
    }

    /// Spawn a future as a background task.
    ///
    /// The `JoinHandle` is returned to the caller. An `AbortHandle` is kept
    /// internally so that [`TaskHost::shutdown`] can cancel all outstanding
    /// tasks.
    // No caller yet — retained for the future async consumers the daemon
    // will host (journal hub, SEAL consumer, final-response subscriber).
    #[allow(dead_code)]
    pub(super) fn spawn<F>(&mut self, future: F) -> JoinHandle<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let join = self.handle.spawn(future);
        self.abort_handles.push(join.abort_handle());
        join
    }

    /// Abort all outstanding tasks.
    ///
    /// Call this before dropping the `Runtime` that was used to construct the
    /// backing `Handle`.  Aborting is non-blocking; the runtime's implicit
    /// drop then drains in-flight work via `shutdown_background`.
    pub(super) fn shutdown(self) {
        for handle in self.abort_handles {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn task_runs_to_completion() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();

        let flag = Arc::new(AtomicBool::new(false));
        let flag2 = Arc::clone(&flag);

        let mut host = TaskHost::new(rt.handle().clone());
        let jh = host.spawn(async move {
            flag2.store(true, Ordering::SeqCst);
        });

        // Drive the task to completion before shutting down.
        rt.block_on(jh).unwrap();

        assert!(flag.load(Ordering::SeqCst), "task must have run");
        host.shutdown();
    }
}
