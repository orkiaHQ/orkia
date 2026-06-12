// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::sync::mpsc;

use crate::engine::BrushSession;

use super::{CompletionProvider, Suggestion};

const REQUEST_TIMEOUT: Duration = Duration::from_millis(500);
const CHANNEL_DEPTH: usize = 4;

struct Request {
    line: String,
    pos: usize,
    reply: std::sync::mpsc::Sender<Suggestion>,
}

pub struct BrushCompletionProvider {
    tx: mpsc::Sender<Request>,
}

impl BrushCompletionProvider {
    /// Spawn a tokio task that owns the brush session lock and serves
    /// completion requests received over an mpsc channel. The returned
    /// provider can be used from any thread (including the blocking
    /// rustyline thread).
    pub fn spawn(session: Arc<Mutex<BrushSession>>) -> Self {
        let (tx, mut rx) = mpsc::channel::<Request>(CHANNEL_DEPTH);
        tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                let Request { line, pos, reply } = req;
                let mut guard = session.lock().await;
                let sug = match guard.complete(&line, pos).await {
                    Ok(c) => Suggestion {
                        insertion_index: c.insertion_index,
                        replace_len: c.delete_count,
                        candidates: c.candidates,
                    },
                    Err(_) => Suggestion::empty(),
                };
                let _ = reply.send(sug);
            }
        });
        Self { tx }
    }
}

impl CompletionProvider for BrushCompletionProvider {
    fn complete(&self, line: &str, pos: usize) -> Suggestion {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel::<Suggestion>();
        let req = Request {
            line: line.to_string(),
            pos,
            reply: reply_tx,
        };
        // The completion callback may run either on a plain OS thread (where
        // `blocking_send` is fine) or on a tokio worker (where it panics).
        // Detect the runtime context and pick the right path so this bridge
        // works from both.
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let tx = self.tx.clone();
                handle.spawn(async move {
                    let _ = tx.send(req).await;
                });
                // We must block this thread waiting for the reply (rustyline's
                // callback is synchronous), but `block_in_place` lets tokio move
                // the other tasks off this worker so it isn't starved for up to
                // REQUEST_TIMEOUT (BUG-067).
                tokio::task::block_in_place(|| {
                    reply_rx.recv_timeout(REQUEST_TIMEOUT).unwrap_or_default()
                })
            }
            Err(_) => {
                if self.tx.blocking_send(req).is_err() {
                    return Suggestion::empty();
                }
                reply_rx.recv_timeout(REQUEST_TIMEOUT).unwrap_or_default()
            }
        }
    }
}
