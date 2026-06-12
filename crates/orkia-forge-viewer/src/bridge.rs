// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Dispatcher for `BridgeMessage`. The webview shell calls this with a
//! decoded message and the result is serialized back into the JS world.
//!
//! Keeping dispatch as a pure function makes the bridge unit-testable
//! without a window.

use orkia_forge_types::{BridgeError, BridgeMessage};

use crate::storage::Storage;

pub fn dispatch(storage: &Storage, msg: BridgeMessage) -> Result<serde_json::Value, BridgeError> {
    match msg {
        BridgeMessage::StorageGet { key } => {
            let v = storage
                .get(&key)
                .map_err(|e| BridgeError::Storage(e.to_string()))?;
            Ok(serde_json::json!(v))
        }
        BridgeMessage::StorageSet { key, value } => {
            storage
                .set(&key, &value)
                .map_err(|e| BridgeError::Storage(e.to_string()))?;
            Ok(serde_json::Value::Null)
        }
        BridgeMessage::StorageDelete { key } => {
            storage
                .delete(&key)
                .map_err(|e| BridgeError::Storage(e.to_string()))?;
            Ok(serde_json::Value::Null)
        }
        BridgeMessage::StorageKeys => {
            let ks = storage
                .keys()
                .map_err(|e| BridgeError::Storage(e.to_string()))?;
            Ok(serde_json::json!(ks))
        }
        // V2 variants are routed through their dedicated bridge modules
        // (network, notification, agent). They never reach this central
        // dispatcher because the Tauri command surface handles each
        // privileged surface as its own `#[tauri::command]` — the
        // central `dispatch` is the storage-only fast path. Surfacing
        // an explicit Invalid error here makes the routing mistake
        // visible during development rather than silently dropping
        // the call.
        BridgeMessage::AgentInvoke { .. }
        | BridgeMessage::AgentCancel { .. }
        | BridgeMessage::NetworkFetch { .. }
        | BridgeMessage::NotificationSend { .. } => Err(BridgeError::Invalid(
            "V2 privileged bridge messages must be routed through their dedicated handler".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_round_trip() {
        let s = Storage::in_memory().unwrap();
        assert!(
            dispatch(
                &s,
                BridgeMessage::StorageSet {
                    key: "k".into(),
                    value: "v".into()
                }
            )
            .unwrap()
            .is_null()
        );
        let got = dispatch(&s, BridgeMessage::StorageGet { key: "k".into() }).unwrap();
        assert_eq!(got, serde_json::json!("v"));
        let keys = dispatch(&s, BridgeMessage::StorageKeys).unwrap();
        assert_eq!(keys, serde_json::json!(["k"]));
        dispatch(&s, BridgeMessage::StorageDelete { key: "k".into() }).unwrap();
        let after = dispatch(&s, BridgeMessage::StorageGet { key: "k".into() }).unwrap();
        assert_eq!(after, serde_json::Value::Null);
    }
}
