// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! OS notifications via `notify-rust` + per-app rate limiter.
//!
//! limits are not user-configurable — they exist to prevent abuse, not
//! to express user preference. Per-app counters live in memory; they
//! reset when the viewer restarts (V2 acceptable; persistent counters
//! are V3).

use std::time::{Duration, Instant};

use orkia_forge_types::{BridgeError, NotifIcon};
use parking_lot::Mutex;

/// Public surface — call from the Tauri command. Performs the rate
/// check, then sends the notification, then returns.
pub fn send(
    rate: &NotificationRateLimiter,
    title: &str,
    body: &str,
    icon: NotifIcon,
    silent: bool,
) -> Result<(), BridgeError> {
    rate.check()?;
    let mut n = notify_rust::Notification::new();
    n.summary(title).body(body);
    match icon {
        NotifIcon::Info => n.icon("dialog-information"),
        NotifIcon::Success => n.icon("emblem-default"),
        NotifIcon::Warning => n.icon("dialog-warning"),
        NotifIcon::Error => n.icon("dialog-error"),
    };
    if silent {
        // notify-rust 4.x has Hint::SoundName / SuppressSound. Just
        // not setting a sound is the cross-platform fallback.
    }
    n.show()
        .map(|_| ())
        .map_err(|e| BridgeError::RuntimeError(format!("notify: {e}")))
}

/// Per-app rate limiter. Two windows: 5/min and 100/hour.
///
/// Implementation is a deque of recent send timestamps. On each
/// `check()` we drop timestamps older than 1 hour, then count entries
/// within the last minute and last hour respectively.
pub struct NotificationRateLimiter {
    inner: Mutex<Vec<Instant>>,
}

impl Default for NotificationRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl NotificationRateLimiter {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Vec::new()),
        }
    }

    /// Records an attempt + returns `Ok(())` if allowed, `PolicyDenied`
    /// if rate-limited.
    pub fn check(&self) -> Result<(), BridgeError> {
        let mut log = self.inner.lock();
        let now = Instant::now();
        // Drop anything older than 1 hour.
        log.retain(|t| now.duration_since(*t) <= Duration::from_secs(3600));
        let last_min = log
            .iter()
            .filter(|t| now.duration_since(**t) <= Duration::from_secs(60))
            .count();
        let last_hour = log.len();
        if last_min >= 5 {
            return Err(BridgeError::PolicyDenied(
                "notification rate limit: max 5 per minute".into(),
            ));
        }
        if last_hour >= 100 {
            return Err(BridgeError::PolicyDenied(
                "notification rate limit: max 100 per hour".into(),
            ));
        }
        log.push(now);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_first_five_per_minute() {
        let r = NotificationRateLimiter::new();
        for _ in 0..5 {
            r.check().unwrap();
        }
    }

    #[test]
    fn denies_sixth_per_minute() {
        let r = NotificationRateLimiter::new();
        for _ in 0..5 {
            r.check().unwrap();
        }
        let err = r.check().unwrap_err();
        assert!(matches!(err, BridgeError::PolicyDenied(_)));
    }

    #[test]
    fn second_limiter_independent() {
        // Each app has its own NotificationRateLimiter instance, so
        // apps don't share quota. Verify by exhausting one + using the
        // other.
        let a = NotificationRateLimiter::new();
        let b = NotificationRateLimiter::new();
        for _ in 0..5 {
            a.check().unwrap();
        }
        assert!(a.check().is_err());
        b.check().unwrap(); // unaffected
    }
}
