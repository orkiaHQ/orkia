// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! Per-workspace TTL cache of effective preferences.

use std::time::{Duration, Instant};

use dashmap::DashMap;
use uuid::Uuid;

use crate::dto::PreferenceDto;

const DEFAULT_TTL: Duration = Duration::from_secs(60);

/// Per-workspace cache of effective preferences. Uses `DashMap` (a sharded
/// concurrent map) rather than `Arc<Mutex<HashMap>>`, so there is no single
/// global lock on the enrich hot path.
pub struct PreferenceCache {
    inner: DashMap<Uuid, CachedEntry>,
    ttl: Duration,
}

struct CachedEntry {
    value: Vec<PreferenceDto>,
    fetched_at: Instant,
}

impl PreferenceCache {
    pub fn new() -> Self {
        Self {
            inner: DashMap::new(),
            ttl: DEFAULT_TTL,
        }
    }

    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            inner: DashMap::new(),
            ttl,
        }
    }

    pub fn get(&self, workspace_id: Uuid) -> Option<Vec<PreferenceDto>> {
        let entry = self.inner.get(&workspace_id)?;
        if entry.fetched_at.elapsed() > self.ttl {
            return None;
        }
        Some(entry.value.clone())
    }

    pub fn put(&self, workspace_id: Uuid, value: Vec<PreferenceDto>) {
        self.inner.insert(
            workspace_id,
            CachedEntry {
                value,
                fetched_at: Instant::now(),
            },
        );
    }

    /// Drop every entry older than `ttl`. Called periodically by the daemon.
    pub fn invalidate_stale(&self) {
        self.inner.retain(|_, e| e.fetched_at.elapsed() <= self.ttl);
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl Default for PreferenceCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enums::{Dimension, PreferenceScope};

    fn sample_pref() -> PreferenceDto {
        PreferenceDto {
            dimension: Dimension::Verbosity,
            value: "concise".into(),
            confidence: 0.9,
            observation_count: 3,
            scope: PreferenceScope::Workspace,
        }
    }

    #[test]
    fn put_then_get_returns_value() {
        let cache = PreferenceCache::new();
        let ws = Uuid::from_u128(1);
        cache.put(ws, vec![sample_pref()]);
        assert_eq!(cache.get(ws).unwrap().len(), 1);
    }

    #[test]
    fn get_returns_none_after_ttl() {
        let cache = PreferenceCache::with_ttl(Duration::from_millis(1));
        let ws = Uuid::from_u128(1);
        cache.put(ws, vec![sample_pref()]);
        std::thread::sleep(Duration::from_millis(10));
        assert!(cache.get(ws).is_none());
    }

    #[test]
    fn invalidate_stale_drops_expired_only() {
        let cache = PreferenceCache::with_ttl(Duration::from_millis(5));
        let fresh = Uuid::from_u128(2);
        cache.put(Uuid::from_u128(1), vec![sample_pref()]);
        std::thread::sleep(Duration::from_millis(10));
        cache.put(fresh, vec![sample_pref()]);
        cache.invalidate_stale();
        assert_eq!(cache.len(), 1);
        assert!(cache.get(fresh).is_some());
    }
}
