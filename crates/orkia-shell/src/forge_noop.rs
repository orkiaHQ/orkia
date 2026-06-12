// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! No-op `ForgeBuilder` for OSS shell builds where the proprietary
//! Forge backend is not wired. Both methods return
//! [`BuilderError::Unavailable`] with a message that directs the user
//! to the premium product.

use std::path::Path;

use async_trait::async_trait;
use orkia_rfc_core::RfcRecord;
use orkia_shell_types::{BuildOutcome, BuilderError, ForgeBuilder, UsageReport};

const UNAVAILABLE_MESSAGE: &str =
    "Forge requires Orkia premium. See https://orkia.io/pricing for details.";

#[derive(Debug, Clone, Copy, Default)]
pub struct NoopForgeBuilder;

impl NoopForgeBuilder {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ForgeBuilder for NoopForgeBuilder {
    async fn build(
        &self,
        _rfc: &RfcRecord,
        _target_dir: &Path,
    ) -> Result<BuildOutcome, BuilderError> {
        Err(BuilderError::Unavailable {
            reason: UNAVAILABLE_MESSAGE.into(),
        })
    }

    async fn usage(&self) -> Result<UsageReport, BuilderError> {
        Err(BuilderError::Unavailable {
            reason: UNAVAILABLE_MESSAGE.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn dummy_rfc() -> RfcRecord {
        use chrono::{FixedOffset, TimeZone};
        use orkia_rfc_core::frontmatter::{ForgeFrontmatterBlock, ForgePermissions, ForgeWindow};
        use orkia_rfc_core::{ContentHash, RfcFrontmatter, RfcId, RfcState};
        let ts = FixedOffset::east_opt(0)
            .unwrap()
            .with_ymd_and_hms(2026, 5, 22, 14, 0, 0)
            .unwrap();
        RfcRecord {
            fm: RfcFrontmatter {
                id: RfcId::new("hello"),
                state: RfcState::DraftEmpty,
                version: 1,
                created_at: ts,
                updated_at: ts,
                content_hash: ContentHash("sha256:0".into()),
                agents: vec![],
                locked_by: None,
                locked_at: None,
                title: None,
                status: None,
                assigned: None,
                kind: Some("forge-app".into()),
                forge: Some(ForgeFrontmatterBlock {
                    name: "hello".into(),
                    description: "x".into(),
                    icon: None,
                    window: ForgeWindow {
                        title: "Hello".into(),
                        width: 480,
                        height: 320,
                        resizable: true,
                    },
                    permissions: ForgePermissions::default(),
                    agent: None,
                }),
                scope: None,
                operator: None,
            },
            body: "body".into(),
        }
    }

    #[tokio::test]
    async fn build_returns_unavailable() {
        let fb = NoopForgeBuilder;
        let err = fb
            .build(&dummy_rfc(), &PathBuf::from("/tmp"))
            .await
            .unwrap_err();
        match err {
            BuilderError::Unavailable { reason } => {
                assert!(reason.contains("Orkia premium"));
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn usage_returns_unavailable() {
        let fb = NoopForgeBuilder;
        let err = fb.usage().await.unwrap_err();
        assert!(matches!(err, BuilderError::Unavailable { .. }));
    }
}
