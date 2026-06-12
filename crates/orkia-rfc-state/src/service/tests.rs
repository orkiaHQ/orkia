// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use tempfile::tempdir;

    use orkia_rfc_core::{AgentId, ContentHash, RfcError, RfcId, RfcState, RfcStore, SectionPath};

    use crate::events::{EventSink, RecordingSink, RfcEvent};

    use super::super::{AskRequest, EditRequest, RfcStateService};

    fn svc() -> (tempfile::TempDir, Arc<RecordingSink>, RfcStateService) {
        let dir = tempdir().expect("tmpdir");
        let store = RfcStore::new(dir.path().to_path_buf());
        let sink = Arc::new(RecordingSink::new());
        let sink_box: Box<dyn EventSink> = Box::new(SharedSink(sink.clone()));
        (dir, sink, RfcStateService::new(store, sink_box))
    }

    fn svc_with_lock_timeout(
        timeout: std::time::Duration,
    ) -> (tempfile::TempDir, Arc<RecordingSink>, RfcStateService) {
        let dir = tempdir().expect("tmpdir");
        let store = RfcStore::new(dir.path().to_path_buf());
        let sink = Arc::new(RecordingSink::new());
        let sink_box: Box<dyn EventSink> = Box::new(SharedSink(sink.clone()));
        (
            dir,
            sink,
            RfcStateService::with_lock_timeout(store, sink_box, timeout),
        )
    }

    // Tiny adapter so the recording sink can be both observed and used as a Box.
    struct SharedSink(Arc<RecordingSink>);
    impl EventSink for SharedSink {
        fn emit(&self, event: RfcEvent) {
            self.0.emit(event);
        }
    }

    fn faye() -> AgentId {
        AgentId::new("faye")
    }
    fn sage() -> AgentId {
        AgentId::new("sage")
    }
    fn human() -> AgentId {
        AgentId::new("issam")
    }

    #[test]
    fn create_emits_created() {
        let (_d, sink, s) = svc();
        let id = RfcId::new("auth-pkce");
        s.create(&id, &human(), Some("PKCE")).expect("create");
        let ev = sink.drain();
        assert_eq!(ev.len(), 1);
        assert!(matches!(ev[0], RfcEvent::Created { .. }));
    }

    #[test]
    fn ask_requires_rationale() {
        let (_d, _sink, s) = svc();
        let id = RfcId::new("x");
        s.create(&id, &human(), None).expect("create");
        let req = AskRequest {
            rfc_id: id,
            agent: faye(),
            question: "iOS?".into(),
            rationale: "".into(),
        };
        assert!(matches!(
            s.ask(req),
            Err(RfcError::RationaleRequired { .. })
        ));
    }

    #[test]
    fn ask_resolve_auto_promotes_from_draft_empty() {
        let (_d, sink, s) = svc();
        let id = RfcId::new("x");
        s.create(&id, &human(), None).expect("create");
        let did = s
            .ask(AskRequest {
                rfc_id: id.clone(),
                agent: faye(),
                question: "iOS?".into(),
                rationale: "need to scope".into(),
            })
            .expect("ask");
        s.resolve_clarification(&id, &did, &human(), "both")
            .expect("resolve");
        let ctx = s.get_context(&id).expect("ctx");
        assert_eq!(ctx.state, RfcState::DraftActive);
        let names: Vec<&str> = sink.drain().iter().map(|e| e.name()).collect();
        assert!(names.contains(&"rfc.state_changed"));
    }

    #[test]
    fn propose_edit_refused_in_draft_empty() {
        let (_d, _sink, s) = svc();
        let id = RfcId::new("x");
        s.create(&id, &human(), None).expect("create");
        let r = s.propose_edit(EditRequest {
            rfc_id: id,
            agent: faye(),
            section: SectionPath::new("Context"),
            new_body: "hi".into(),
            linked_decisions: vec![],
            if_hash_matches: None,
        });
        assert!(matches!(r, Err(RfcError::InvalidState { .. })));
    }

    #[test]
    fn propose_edit_locks_and_blocks_other_agents() {
        let (_d, _sink, s) = svc();
        let id = RfcId::new("x");
        s.create(&id, &human(), None).expect("create");
        // Move to DraftActive by resolving (no clarifications exist, so we
        // simulate by directly asking + resolving).
        let did = s
            .ask(AskRequest {
                rfc_id: id.clone(),
                agent: faye(),
                question: "?".into(),
                rationale: "x".into(),
            })
            .expect("ask");
        s.resolve_clarification(&id, &did, &human(), "ok")
            .expect("resolve");
        // faye edits.
        s.propose_edit(EditRequest {
            rfc_id: id.clone(),
            agent: faye(),
            section: SectionPath::new("Context"),
            new_body: "by faye".into(),
            linked_decisions: vec![],
            if_hash_matches: None,
        })
        .expect("faye edits");
        // sage blocked.
        let r = s.propose_edit(EditRequest {
            rfc_id: id,
            agent: sage(),
            section: SectionPath::new("Context"),
            new_body: "by sage".into(),
            linked_decisions: vec![],
            if_hash_matches: None,
        });
        assert!(matches!(r, Err(RfcError::Locked { .. })));
    }

    #[test]
    fn stale_snapshot_refused() {
        let (_d, _sink, s) = svc();
        let id = RfcId::new("x");
        s.create(&id, &human(), None).expect("create");
        let did = s
            .ask(AskRequest {
                rfc_id: id.clone(),
                agent: faye(),
                question: "?".into(),
                rationale: "x".into(),
            })
            .expect("ask");
        s.resolve_clarification(&id, &did, &human(), "ok")
            .expect("resolve");
        let r = s.propose_edit(EditRequest {
            rfc_id: id,
            agent: faye(),
            section: SectionPath::new("Context"),
            new_body: "x".into(),
            linked_decisions: vec![],
            if_hash_matches: Some(ContentHash("sha256:bogus".into())),
        });
        assert!(matches!(r, Err(RfcError::StaleSnapshot { .. })));
    }

    #[test]
    fn force_release_drops_lock_and_emits_unlocked() {
        let (_d, sink, s) = svc();
        let id = RfcId::new("x");
        s.create(&id, &human(), None).expect("create");
        let did = s
            .ask(AskRequest {
                rfc_id: id.clone(),
                agent: faye(),
                question: "?".into(),
                rationale: "x".into(),
            })
            .expect("ask");
        s.resolve_clarification(&id, &did, &human(), "ok")
            .expect("resolve");
        s.propose_edit(EditRequest {
            rfc_id: id.clone(),
            agent: faye(),
            section: orkia_rfc_core::SectionPath::new("Context"),
            new_body: "by faye".into(),
            linked_decisions: vec![],
            if_hash_matches: None,
        })
        .expect("faye edits");
        let _ = sink.drain();
        let released = s.force_release(&id);
        assert_eq!(released, Some(faye()));
        let names: Vec<&str> = sink.drain().iter().map(|e| e.name()).collect();
        assert!(names.contains(&"rfc.unlocked"));
        // Sage can now edit.
        s.propose_edit(EditRequest {
            rfc_id: id,
            agent: sage(),
            section: orkia_rfc_core::SectionPath::new("Context"),
            new_body: "by sage".into(),
            linked_decisions: vec![],
            if_hash_matches: None,
        })
        .expect("sage edits after force release");
    }

    #[test]
    fn reap_expired_locks_emits_timeout_unlocked() {
        let (_d, sink, s) = svc_with_lock_timeout(std::time::Duration::from_millis(1));
        let id = RfcId::new("x");
        s.create(&id, &human(), None).expect("create");
        let did = s
            .ask(AskRequest {
                rfc_id: id.clone(),
                agent: faye(),
                question: "?".into(),
                rationale: "x".into(),
            })
            .expect("ask");
        s.resolve_clarification(&id, &did, &human(), "ok")
            .expect("resolve");
        s.propose_edit(EditRequest {
            rfc_id: id.clone(),
            agent: faye(),
            section: orkia_rfc_core::SectionPath::new("Context"),
            new_body: "by faye".into(),
            linked_decisions: vec![],
            if_hash_matches: None,
        })
        .expect("acquire lock");
        std::thread::sleep(std::time::Duration::from_millis(20));
        let _ = sink.drain();
        let released = s.reap_expired_locks(std::time::SystemTime::now());
        assert_eq!(released, 1);
        let events = sink.drain();
        let timeout_evt = events
            .iter()
            .find(|e| matches!(e, RfcEvent::Unlocked { .. }))
            .expect("rfc.unlocked");
        if let RfcEvent::Unlocked { reason, by, .. } = timeout_evt {
            assert!(matches!(reason, crate::events::SerdeUnlockReason::Timeout));
            assert_eq!(by, &faye());
        }
    }

    #[test]
    fn promote_then_reopen_archives_v1() {
        let (_d, sink, s) = svc();
        let id = RfcId::new("x");
        s.create(&id, &human(), None).expect("create");
        let did = s
            .ask(AskRequest {
                rfc_id: id.clone(),
                agent: faye(),
                question: "?".into(),
                rationale: "x".into(),
            })
            .expect("ask");
        s.resolve_clarification(&id, &did, &human(), "ok")
            .expect("resolve");
        s.promote(&id, &human()).expect("promote");
        let state = s.get_context(&id).expect("ctx").state;
        assert_eq!(state, RfcState::Active);
        s.reopen(&id, &human()).expect("reopen");
        let ctx = s.get_context(&id).expect("ctx");
        assert_eq!(ctx.version, 2);
        assert_eq!(ctx.state, RfcState::DraftActive);
        let names: Vec<&str> = sink.drain().iter().map(|e| e.name()).collect();
        assert!(names.contains(&"rfc.promoted"));
        assert!(names.contains(&"rfc.reopened"));
    }
}
