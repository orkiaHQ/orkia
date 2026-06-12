// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Tamper-detection integration tests: every kind of modification to
//! the JSONL must produce a specific verifier error.

use orkia_forge_seal::{SealWriter, VerifyError, verify_chain};
use tempfile::TempDir;

fn populated() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let w = SealWriter::open(tmp.path()).unwrap();
    for i in 0..5 {
        w.append("kind", serde_json::json!({"i": i})).unwrap();
    }
    drop(w);
    tmp
}

fn read_lines(tmp: &TempDir) -> Vec<String> {
    let p = tmp.path().join("events.jsonl");
    std::fs::read_to_string(p)
        .unwrap()
        .lines()
        .map(String::from)
        .collect()
}

fn write_lines(tmp: &TempDir, lines: &[String]) {
    let mut s = lines.join("\n");
    s.push('\n');
    std::fs::write(tmp.path().join("events.jsonl"), s).unwrap();
}

#[test]
fn baseline_passes() {
    let tmp = populated();
    let r = verify_chain(tmp.path()).unwrap();
    assert_eq!(r.events, 5);
}

/// Modifying a single byte of the `data` field breaks the hash check.
#[test]
fn modify_data_breaks_hash() {
    let tmp = populated();
    let mut lines = read_lines(&tmp);
    // Replace the second record's data `{"i":1}` with `{"i":99}` —
    // breaks the hash but signature still verifies (sig is over `hash`,
    // but the hash on disk doesn't match the recomputed content).
    lines[1] = lines[1].replace("\"i\":1", "\"i\":99");
    write_lines(&tmp, &lines);
    let err = verify_chain(tmp.path()).unwrap_err();
    assert!(
        matches!(err, VerifyError::HashMismatch { id: 2 }),
        "got {err:?}"
    );
}

/// Modifying the recorded `hash` field also fails — either the
/// signature check or the chain link breaks. (We test the signature
/// path: change just `hash` so chain still links from previous, but
/// hash no longer matches sig.)
#[test]
fn modify_hash_breaks_signature() {
    let tmp = populated();
    let mut lines = read_lines(&tmp);
    // Tweak one hex digit in the third record's hash.
    let l = &mut lines[2];
    let hash_idx = l.find("\"hash\":\"sha256:").unwrap() + "\"hash\":\"sha256:".len();
    let mut chars: Vec<char> = l.chars().collect();
    chars[hash_idx] = if chars[hash_idx] == 'a' { 'b' } else { 'a' };
    *l = chars.into_iter().collect();
    write_lines(&tmp, &lines);
    let err = verify_chain(tmp.path()).unwrap_err();
    // Either hash mismatch (if first byte of recomputed) or chain
    // broken (next record's prev_hash now lies). The verifier walks
    // records in order, so it sees hash mismatch first on this record.
    assert!(
        matches!(
            err,
            VerifyError::HashMismatch { id: 3 } | VerifyError::ChainBroken { id: 4 }
        ),
        "got {err:?}"
    );
}

/// Deleting a record breaks the id sequence.
#[test]
fn delete_record_breaks_chain() {
    let tmp = populated();
    let mut lines = read_lines(&tmp);
    lines.remove(2); // drop record 3
    write_lines(&tmp, &lines);
    let err = verify_chain(tmp.path()).unwrap_err();
    // Either IdSkip (expected 3, got 4) or ChainBroken (4's prev_hash != 2's hash).
    assert!(
        matches!(
            err,
            VerifyError::IdSkip {
                expected: 3,
                found: 4
            } | VerifyError::ChainBroken { id: 4 }
        ),
        "got {err:?}"
    );
}

/// Inserting a fabricated record fails because its signature won't
/// verify against the per-app key.
#[test]
fn inserted_record_fails_signature() {
    let tmp = populated();
    let mut lines = read_lines(&tmp);
    // Insert a forged record between 3 and 4. Even a syntactically
    // valid record won't have a valid signature.
    let forged = r#"{"id":4,"ts":"2026-05-23T10:00:00Z","prev_hash":"sha256:0000000000000000000000000000000000000000000000000000000000000000","kind":"forged","data":{},"hash":"sha256:0000000000000000000000000000000000000000000000000000000000000000","sig":"3044022000000000000000000000000000000000000000000000000000000000000000000220000000000000000000000000000000000000000000000000000000000000000000"}"#;
    lines.insert(3, forged.into());
    write_lines(&tmp, &lines);
    let err = verify_chain(tmp.path()).unwrap_err();
    // Will fail either on chain link (forged prev_hash is genesis but
    // expected real hash) or on bad signature. Either way verification rejects.
    assert!(
        matches!(
            err,
            VerifyError::ChainBroken { .. }
                | VerifyError::BadSignature { .. }
                | VerifyError::IdSkip { .. }
        ),
        "got {err:?}"
    );
}

/// Swapping two records breaks the chain.
#[test]
fn swap_records_breaks_chain() {
    let tmp = populated();
    let mut lines = read_lines(&tmp);
    lines.swap(1, 2);
    write_lines(&tmp, &lines);
    let err = verify_chain(tmp.path()).unwrap_err();
    // After swap, id sequence is 1, 3, 2, 4, 5 → IdSkip on the second
    // record (expected 2, got 3).
    assert!(
        matches!(
            err,
            VerifyError::IdSkip {
                expected: 2,
                found: 3
            }
        ),
        "got {err:?}"
    );
}

/// Replacing the signing key invalidates every existing signature.
#[test]
fn replacing_key_invalidates_all_signatures() {
    let tmp = populated();
    // Wipe the key and generate a fresh one.
    let key_path = tmp.path().join("signing.pem");
    std::fs::remove_file(&key_path).unwrap();
    // Open a new writer — generates a fresh key. We don't append; the
    // existing chain's signatures should now all fail.
    let _w = SealWriter::open(tmp.path()).unwrap();
    let err = verify_chain(tmp.path()).unwrap_err();
    assert!(
        matches!(err, VerifyError::BadSignature { id: 1 }),
        "got {err:?}"
    );
}
