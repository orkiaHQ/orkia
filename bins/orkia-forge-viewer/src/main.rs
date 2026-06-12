// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `orkia-forge-viewer` — V0 viewer binary (Tauri 2.x).
//!
//! One process per Forge app instance. Invoked by `orkia app run <name>`
//! with `--app-dir <path> --app-id <bundle-id> --socket <orkia.sock>`.
//!
//! Owns:
//!  * the per-app SQLite storage (single owner — `Mutex<Storage>` only
//!    exists because Tauri's managed state requires `Sync`; in practice
//!    every call runs on the JS thread sequentially)
//!  * the journal client (best-effort NDJSON to `~/.orkia/run/orkia.sock`)
//!  * the Tauri window + webview
//!
//! Does NOT own:
//!  * the manifest (read-only at startup, then immutable for the life of
//!    the process — re-launch the viewer to pick up changes)

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Mutex;

use orkia_forge_permissions::Permissions;
use orkia_forge_types::{
    AgentResult, AgentStatus, BridgeError, FetchResponse, ForgeManifest, HttpMethod, NotifIcon,
};
use orkia_forge_viewer::{JournalClient, Storage, ViewerConfig};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tauri::{Manager, RunEvent, WebviewUrl, WebviewWindowBuilder};
use uuid::Uuid;

mod shim;

/// Tauri-managed storage. `Mutex` because `rusqlite::Connection` is `!Sync`,
/// not because we expect contention — only the JS thread invokes commands
/// and each command returns before the next is dispatched.
struct StorageState(Mutex<Storage>);

/// The journal client lives behind a Mutex too because Tauri state must be
/// `Sync` and `UnixStream` writes are stateful. Same single-owner reality.
struct JournalState(Mutex<JournalClient>);

/// V2: per-app permissions loaded from `manifest.toml` once at startup.
/// Immutable for the life of the viewer process — re-launch to pick up
/// permission changes (consistent with the rest of the manifest).
struct PermissionsState(Permissions);

/// V2: per-app SEAL chain writer. Wraps `orkia-forge-seal::SealWriter`
/// which has its own internal `Mutex` for serializing concurrent
/// appends from parallel Tauri command futures.
struct SealState(orkia_forge_seal::SealWriter);

impl SealState {
    /// Best-effort append. SEAL is informational — a failure to write
    /// the audit log must not fail the user-visible bridge call. We
    /// swallow the result so the caller never sees an audit failure.
    fn append(&self, kind: &str, data: serde_json::Value) -> Option<u64> {
        self.0.append(kind, data).ok()
    }
}

/// V2: per-app notification rate limiter (5/min, 100/hour).
struct NotifLimiterState(orkia_forge_viewer::NotificationRateLimiter);

/// Static metadata returned by `app_meta` so the JS shim can answer
/// `window.orkia.v1.app.{id,name,version}()` synchronously after the
/// initial fetch.
#[derive(Clone, Serialize, Deserialize)]
struct AppMeta {
    id: String,
    name: String,
    version: String,
    api_version: u32,
}

#[tauri::command]
fn storage_get(state: tauri::State<StorageState>, key: String) -> Result<Option<String>, String> {
    state
        .0
        .lock()
        .map_err(|_| "storage lock poisoned".to_string())?
        .get(&key)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn storage_set(
    state: tauri::State<StorageState>,
    key: String,
    value: String,
) -> Result<(), String> {
    state
        .0
        .lock()
        .map_err(|_| "storage lock poisoned".to_string())?
        .set(&key, &value)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn storage_delete(state: tauri::State<StorageState>, key: String) -> Result<(), String> {
    state
        .0
        .lock()
        .map_err(|_| "storage lock poisoned".to_string())?
        .delete(&key)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn storage_keys(state: tauri::State<StorageState>) -> Result<Vec<String>, String> {
    state
        .0
        .lock()
        .map_err(|_| "storage lock poisoned".to_string())?
        .keys()
        .map_err(|e| e.to_string())
}

/// Debug-only hook: write a one-line string to
/// `<app_dir>/.test-result` so external test drivers can verify the JS
/// bridge end-to-end. Compiled **only in debug builds** (`cfg(debug_assertions)`)
/// so the command and its env-var check are entirely absent from release
/// binaries (SEC-076). `ORKIA_FORGE_VIEWER_TEST_HOOK=1` MUST NOT be set
/// in any production deployment.
#[cfg(debug_assertions)]
#[tauri::command]
fn report_result(state: tauri::State<TestHook>, result: String) -> Result<(), String> {
    if !state.enabled {
        return Ok(());
    }
    std::fs::write(&state.path, result.as_bytes()).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(debug_assertions)]
struct TestHook {
    enabled: bool,
    path: std::path::PathBuf,
}

/// Web-asset allow-list for `serve_app_file`. Only these exact filenames
/// (relative to the app-dir root) are served; everything else — including
/// `seal/`, `data/`, `build/` — is denied (SEC-031, SEC-053).
///
/// All entries must be flat filenames (no path separators). The allow-list
/// is checked before any filesystem access.
const ALLOWED_ASSETS: &[&str] = &["app.html", "app.css", "app.js", "icon.png", "icon.svg"];

/// Resolve `forgeapp://localhost/<file>` requests against the per-app
/// directory. Enforces a strict allow-list of web assets (SEC-031/053) and
/// a canonicalize-based containment guard as defense-in-depth (SEC-054).
fn serve_app_file(
    root: &std::path::Path,
    request: tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<std::borrow::Cow<'static, [u8]>> {
    use tauri::http::{Response, StatusCode};

    let forbidden = || {
        Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(b"forbidden".as_slice().into())
            .unwrap_or_else(|_| Response::new(b"forbidden".as_slice().into()))
    };
    let not_found = || {
        Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(b"not found".as_slice().into())
            .unwrap_or_else(|_| Response::new(b"not found".as_slice().into()))
    };

    let url = request.uri();
    let raw = url.path().trim_start_matches('/');

    // Allow-list check: only serve the known web-asset files (SEC-031/053).
    // This is the primary guard — `seal/`, `data/`, `build/` are never in
    // this list and therefore never served regardless of traversal tricks.
    if !ALLOWED_ASSETS.contains(&raw) {
        return forbidden();
    }

    let path = root.join(raw);

    // Canonicalize-based containment guard (SEC-054). Because `raw` is
    // already constrained by the allow-list above, this is defense-in-depth:
    // it catches any future edge case where an allowed name resolves outside
    // `root` (e.g. a symlink in the app-dir pointing outward).
    let real_path = match path.canonicalize() {
        Ok(p) => p,
        Err(_) => return not_found(),
    };
    let real_root = match root.canonicalize() {
        Ok(r) => r,
        Err(_) => return forbidden(),
    };
    if !real_path.starts_with(&real_root) {
        return forbidden();
    }

    let bytes = match std::fs::read(&real_path) {
        Ok(b) => b,
        Err(_) => return not_found(),
    };
    let mime = mime_for(&real_path);
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", mime)
        // V2: strict CSP for everything served via the `forgeapp://`
        // protocol. `connect-src 'none'` is what kills native `fetch`,
        // `XMLHttpRequest`, `WebSocket`, `EventSource`. The only network
        // path out of the app is `window.orkia.v1.network.fetch` which
        // goes through Tauri IPC and bypasses the page's CSP. Inline
        // styles and scripts are allowed because the V1 builder produces
        // an app.css/app.js loaded as `forgeapp://`-protocol resources
        // (`'self'` matches the custom scheme).
        .header("content-security-policy", CSP)
        .body(bytes.into())
        .unwrap_or_else(|_| Response::new(b"".as_slice().into()))
}

#[cfg(test)]
mod tests_serve {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_request(path: &str) -> tauri::http::Request<Vec<u8>> {
        tauri::http::Request::builder()
            .uri(format!("forgeapp://localhost/{path}"))
            .body(vec![])
            .unwrap()
    }

    #[test]
    fn serves_allowed_asset() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("app.html"), b"<html/>").unwrap();
        let resp = serve_app_file(tmp.path(), make_request("app.html"));
        assert_eq!(resp.status(), tauri::http::StatusCode::OK);
    }

    #[test]
    fn blocks_seal_dir() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("seal")).unwrap();
        fs::write(tmp.path().join("seal").join("key"), b"secret").unwrap();
        let resp = serve_app_file(tmp.path(), make_request("seal/key"));
        assert_eq!(resp.status(), tauri::http::StatusCode::FORBIDDEN);
    }

    #[test]
    fn blocks_data_dir() {
        let tmp = TempDir::new().unwrap();
        let resp = serve_app_file(tmp.path(), make_request("data/storage.db"));
        assert_eq!(resp.status(), tauri::http::StatusCode::FORBIDDEN);
    }

    #[test]
    fn blocks_build_response_json() {
        let tmp = TempDir::new().unwrap();
        let resp = serve_app_file(tmp.path(), make_request("build/response.json"));
        assert_eq!(resp.status(), tauri::http::StatusCode::FORBIDDEN);
    }

    #[test]
    fn blocks_dotdot_traversal() {
        let tmp = TempDir::new().unwrap();
        let resp = serve_app_file(tmp.path(), make_request("../etc/passwd"));
        assert_eq!(resp.status(), tauri::http::StatusCode::FORBIDDEN);
    }

    #[test]
    fn blocks_absolute_path() {
        let tmp = TempDir::new().unwrap();
        let resp = serve_app_file(tmp.path(), make_request("/etc/passwd"));
        assert_eq!(resp.status(), tauri::http::StatusCode::FORBIDDEN);
    }

    #[test]
    fn not_found_for_missing_allowed_asset() {
        let tmp = TempDir::new().unwrap();
        // app.html is in the allow-list but not written to disk
        let resp = serve_app_file(tmp.path(), make_request("app.html"));
        assert_eq!(resp.status(), tauri::http::StatusCode::NOT_FOUND);
    }
}

/// Strict CSP for the per-app webview. Locks down every network egress
/// path the app could otherwise use (fetch, XHR, WebSocket, EventSource).
/// The only sanctioned way for an app to reach the network is through
/// `window.orkia.v1.network.fetch`, which travels via Tauri IPC and
/// therefore isn't subject to the page-level CSP.
const CSP: &str = concat!(
    "default-src 'none'; ",
    "script-src 'self' 'unsafe-inline'; ",
    "style-src 'self' 'unsafe-inline'; ",
    "img-src 'self' data:; ",
    "font-src 'self' data:; ",
    "connect-src 'none'; ",
    "frame-src 'none'; ",
    "base-uri 'none'; ",
    "form-action 'none'",
);

fn mime_for(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "application/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("svg") => "image/svg+xml",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

#[tauri::command]
fn app_meta(meta: tauri::State<AppMeta>) -> AppMeta {
    AppMeta {
        id: meta.id.clone(),
        name: meta.name.clone(),
        version: meta.version.clone(),
        api_version: meta.api_version,
    }
}

// ── V2: agent.invoke ────────────────────────────────────────────────

// Fields are deserialized but only read by Stage 8 (orchestrator wiring).
// Tauri's command macro still needs them on the wire so the shim can
// thread real values through. Suppress unread-fields until Stage 8.
#[derive(Deserialize)]
#[allow(dead_code)]
struct AgentInvokeArgs {
    task: String,
    #[serde(default)]
    payload: serde_json::Value,
}

/// `window.orkia.v1.agent.invoke(task, payload)` — fast deny on
/// manifest permission, then (Stage 8) forward to the in-process
/// orchestrator. For now returns `RuntimeError("not_implemented")` on
/// the success path so the deny-path UX is testable in isolation.
#[tauri::command]
async fn agent_invoke(
    perms: tauri::State<'_, PermissionsState>,
    args: AgentInvokeArgs,
) -> Result<AgentResult, BridgeError> {
    perms.0.check_agent()?;
    let _ = args;
    // Stage 8 fills this in by calling the in-process orchestrator at
    // `<api>/v1/forge/agent/invoke`. The shape is final; only the body
    // changes.
    let invocation_id = Uuid::new_v4();
    Err(BridgeError::RuntimeError(format!(
        "agent.invoke not yet wired to orchestrator (invocation_id {invocation_id})"
    )))
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct AgentCancelArgs {
    invocation_id: Uuid,
}

#[tauri::command]
async fn agent_cancel(
    perms: tauri::State<'_, PermissionsState>,
    args: AgentCancelArgs,
) -> Result<(), BridgeError> {
    perms.0.check_agent()?;
    let _ = args;
    // V2: cancel is best-effort; Stage 8 may implement, may not.
    Ok(())
}

/// Helper so the JS-side `_status: AgentStatus::Denied` placeholder
/// stays valid if Stage 8 reintroduces it without a code revisit.
#[allow(dead_code)]
fn _force_agent_status_used(_s: AgentStatus) {}

// ── V2: network.fetch ───────────────────────────────────────────────

#[derive(Deserialize)]
struct NetworkFetchArgs {
    url: String,
    #[serde(default)]
    method: HttpMethod,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u32>,
}

/// `window.orkia.v1.network.fetch(url, options)`. Permission-checks
/// `orkia_forge_viewer::fetch` for the actual HTTPS call with strict
/// redirect/size enforcement. SEAL event appended on every attempt.
#[tauri::command]
async fn network_fetch(
    perms: tauri::State<'_, PermissionsState>,
    seal: tauri::State<'_, SealState>,
    meta: tauri::State<'_, AppMeta>,
    args: NetworkFetchArgs,
) -> Result<FetchResponse, BridgeError> {
    let method_str = args.method.as_str();
    let url = args.url.clone();
    let result = orkia_forge_viewer::fetch(
        &perms.0,
        &meta.name,
        orkia_forge_viewer::FetchArgs {
            url: args.url,
            method: args.method,
            headers: args.headers,
            body: args.body,
            timeout_ms: args.timeout_ms,
        },
    )
    .await;
    let (kind, data) = match &result {
        Ok(r) => (
            "app.network.fetch",
            serde_json::json!({
                "url": url,
                "method": method_str,
                "status": r.status,
                "bytes_in": r.body.len(),
            }),
        ),
        // Redirect off-whitelist gets a dedicated SEAL kind so audit
        // consumers can distinguish policy-driven redirect blocks from
        Err(BridgeError::PolicyDenied(reason)) if reason == "redirect_off_whitelist" => (
            "app.network.fetch.redirect_denied",
            serde_json::json!({"url": url, "method": method_str, "reason": reason}),
        ),
        Err(e) => (
            "app.network.fetch.denied",
            serde_json::json!({"url": url, "method": method_str, "error": e.to_string()}),
        ),
    };
    let _ = seal.append(kind, data);
    result
}

// ── V2: notification.send ───────────────────────────────────────────

#[derive(Deserialize)]
struct NotificationSendArgs {
    title: String,
    body: String,
    #[serde(default)]
    icon: NotifIcon,
    #[serde(default)]
    silent: bool,
}

#[tauri::command]
async fn notification_send(
    perms: tauri::State<'_, PermissionsState>,
    rate: tauri::State<'_, NotifLimiterState>,
    seal: tauri::State<'_, SealState>,
    args: NotificationSendArgs,
) -> Result<(), BridgeError> {
    perms.0.check_notification()?;
    let title = args.title.clone();
    let result = orkia_forge_viewer::send_notification(
        &rate.0,
        &args.title,
        &args.body,
        args.icon,
        args.silent,
    );
    let (kind, data) = match &result {
        Ok(()) => (
            "app.notification.sent",
            serde_json::json!({"title": title, "icon": args.icon}),
        ),
        Err(e) => (
            "app.notification.denied",
            serde_json::json!({"title": title, "error": e.to_string()}),
        ),
    };
    let _ = seal.append(kind, data);
    result
}

fn main() {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let config = match ViewerConfig::from_args(raw_args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("orkia-forge-viewer: {e}");
            eprintln!("usage: orkia-forge-viewer --app-dir <path> --app-id <id> --socket <path>");
            std::process::exit(2);
        }
    };

    let manifest_path = config.app_dir.join("manifest.toml");
    let manifest = match ForgeManifest::load(&manifest_path) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("orkia-forge-viewer: manifest load: {e}");
            std::process::exit(3);
        }
    };

    let storage = match Storage::open(&config.app_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("orkia-forge-viewer: storage open: {e}");
            std::process::exit(3);
        }
    };

    let journal = JournalClient::connect(&config.socket_path, &config.app_id);
    let meta = AppMeta {
        id: config.app_id.clone(),
        name: manifest.forge.name.clone(),
        version: manifest.forge.version.clone(),
        api_version: manifest.forge.api_version,
    };

    let app_dir = config.app_dir.clone();
    let window_cfg = manifest.forge.window.clone();
    let init_script = shim::initialization_script(&meta);
    let protocol_root = app_dir.clone();

    #[cfg(debug_assertions)]
    let test_hook = TestHook {
        enabled: std::env::var("ORKIA_FORGE_VIEWER_TEST_HOOK")
            .ok()
            .as_deref()
            == Some("1"),
        path: config.app_dir.join(".test-result"),
    };

    // V2: per-app SEAL chain. Failure to open the chain is fatal — we
    // don't want the viewer running without an audit log. The first
    // open will generate the per-app signing key.
    let seal_writer = match orkia_forge_seal::SealWriter::open(&config.app_dir.join("seal")) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("orkia-forge-viewer: SEAL open: {e}");
            std::process::exit(5);
        }
    };
    let seal_state = SealState(seal_writer);

    let builder = tauri::Builder::default()
        .register_uri_scheme_protocol("forgeapp", move |_ctx, request| {
            serve_app_file(&protocol_root, request)
        })
        .manage(StorageState(Mutex::new(storage)))
        .manage(JournalState(Mutex::new(journal)))
        .manage(meta)
        .manage(PermissionsState(Permissions::from_manifest(
            &manifest.forge.permissions,
        )))
        .manage(seal_state)
        .manage(NotifLimiterState(
            orkia_forge_viewer::NotificationRateLimiter::new(),
        ));

    // `report_result` and its `TestHook` state are debug-only (SEC-076).
    // In release builds the command is compiled out entirely.
    #[cfg(debug_assertions)]
    let builder = builder
        .manage(test_hook)
        .invoke_handler(tauri::generate_handler![
            storage_get,
            storage_set,
            storage_delete,
            storage_keys,
            app_meta,
            agent_invoke,
            agent_cancel,
            network_fetch,
            notification_send,
            report_result,
        ]);
    #[cfg(not(debug_assertions))]
    let builder = builder.invoke_handler(tauri::generate_handler![
        storage_get,
        storage_set,
        storage_delete,
        storage_keys,
        app_meta,
        agent_invoke,
        agent_cancel,
        network_fetch,
        notification_send,
    ]);

    let result = builder
        .setup(move |app| {
            // Per-app HTML/CSS/JS live outside `frontendDist`. We expose
            // them via a custom `forgeapp://` URI scheme registered above,
            // which serves files relative to the app dir. This is the
            // documented Tauri 2.x pattern for loading runtime-resolved
            // resources.
            let url = WebviewUrl::External(
                "forgeapp://localhost/app.html"
                    .parse::<tauri::Url>()
                    .map_err(|e| format!("bad URL: {e}"))?,
            );
            let _window = WebviewWindowBuilder::new(app, "main", url)
                .title(&window_cfg.title)
                .inner_size(window_cfg.width as f64, window_cfg.height as f64)
                .resizable(window_cfg.resizable)
                .initialization_script(&init_script)
                .build()?;
            if let Some(state) = app.try_state::<JournalState>()
                && let Ok(mut j) = state.0.lock()
            {
                let _ = j.emit("app.window.opened", serde_json::json!({"window": "main"}));
            }
            if let Some(seal) = app.try_state::<SealState>() {
                let _ = seal.append("app.window.opened", serde_json::json!({"window": "main"}));
            }
            Ok(())
        })
        .build(tauri::generate_context!());

    let app = match result {
        Ok(a) => a,
        Err(e) => {
            eprintln!("orkia-forge-viewer: tauri build: {e}");
            std::process::exit(4);
        }
    };

    // Forward SIGINT / SIGTERM into Tauri's event loop so the
    // RunEvent::Exit handler below still fires (which emits the
    // `app.window.closed` journal event). `AppHandle::exit` posts to
    // the event loop and is safe to call from any thread.
    {
        let handle = app.handle().clone();
        if let Err(e) = ctrlc::set_handler(move || handle.exit(0)) {
            eprintln!("orkia-forge-viewer: signal handler: {e}");
        }
    }

    app.run(|handle, event| {
        // Fire on `Exit` only — `ExitRequested` is "user wants to quit;
        // maybe deny it," whereas `Exit` is the final terminal event of
        // the run loop. Listening to both would double-emit.
        if let RunEvent::Exit = event {
            if let Some(state) = handle.try_state::<JournalState>()
                && let Ok(mut j) = state.0.lock()
            {
                let _ = j.emit("app.window.closed", serde_json::json!({"window": "main"}));
            }
            if let Some(seal) = handle.try_state::<SealState>() {
                let _ = seal.append("app.window.closed", serde_json::json!({"window": "main"}));
            }
        }
    });
}
