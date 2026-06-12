// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! The `window.orkia.v1` JS shim that gets injected at window setup.
//!
//! The shim routes everything through Tauri's `__TAURI__.core.invoke`
//! (exposed by `withGlobalTauri = true` in `tauri.conf.json`). It does
//! NOT speak the `BridgeMessage` wire format directly — Tauri's command
//! macros already give us a typed JSON-RPC layer, so the shim just

use crate::AppMeta;

pub fn initialization_script(meta: &AppMeta) -> String {
    // Inline the static metadata so `app.id()`, `app.name()`, `app.version()`
    // are synchronous in user code. The bridge commands themselves stay
    // async because storage I/O is async.
    let meta_json = serde_json::to_string(meta).unwrap_or_else(|_| "{}".into());
    format!(
        r#"
(() => {{
    if (typeof window === "undefined") return;
    const META = {meta_json};
    const invoke = (cmd, args) => {{
        if (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke) {{
            return window.__TAURI__.core.invoke(cmd, args || {{}});
        }}
        return Promise.reject(new Error("orkia bridge unavailable"));
    }};

    const readyHandlers = [];
    const closeHandlers = [];

    window.orkia = window.orkia || {{}};
    window.orkia.v1 = {{
        storage: {{
            get:    (key)        => invoke("storage_get",    {{ key }}),
            set:    (key, value) => invoke("storage_set",    {{ key, value }}),
            delete: (key)        => invoke("storage_delete", {{ key }}),
            keys:   ()           => invoke("storage_keys",   {{}}),
        }},
        app: {{
            id:          () => META.id,
            name:        () => META.name,
            version:     () => META.version,
            apiVersion:  () => META.api_version,
        }},
        // V2: privileged surfaces. Each call round-trips through the
        // Tauri IPC bridge which permission-checks against the per-app
        // manifest before forwarding.
        agent: {{
            invoke: (task, payload) =>
                invoke("agent_invoke", {{ args: {{ task: task, payload: payload || null }} }}),
            cancel: (invocation_id) =>
                invoke("agent_cancel", {{ args: {{ invocation_id: invocation_id }} }}),
        }},
        network: {{
            fetch: (url, options) => {{
                const opts = options || {{}};
                return invoke("network_fetch", {{
                    args: {{
                        url: url,
                        method: (opts.method || "GET").toUpperCase(),
                        headers: opts.headers || {{}},
                        body: typeof opts.body === "string"
                            ? opts.body
                            : (opts.body == null ? null : JSON.stringify(opts.body)),
                        timeout_ms: opts.timeout_ms || null,
                    }}
                }});
            }},
        }},
        notification: {{
            send: (title, body, options) => {{
                const opts = options || {{}};
                return invoke("notification_send", {{
                    args: {{
                        title: title,
                        body: body,
                        icon: opts.icon || "info",
                        silent: opts.silent === true,
                    }}
                }});
            }},
        }},
        on: (event, handler) => {{
            if (event === "ready") {{
                if (document.readyState === "complete") {{
                    queueMicrotask(handler);
                }} else {{
                    readyHandlers.push(handler);
                }}
            }} else if (event === "close") {{
                closeHandlers.push(handler);
            }}
        }},
    }};

    // Test-only escape hatch — gated server-side by
    // ORKIA_FORGE_VIEWER_TEST_HOOK=1, so it is a no-op in production.
    window.__orkia_test_report = (result) => invoke("report_result", {{ result }});

    window.addEventListener("DOMContentLoaded", () => {{
        for (const h of readyHandlers.splice(0)) {{
            try {{ h(); }} catch (e) {{ console.error(e); }}
        }}
    }});
    window.addEventListener("beforeunload", () => {{
        for (const h of closeHandlers.splice(0)) {{
            try {{ h(); }} catch (e) {{}}
        }}
    }});
}})();
"#
    )
}
