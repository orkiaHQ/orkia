// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `$kernel` shell builtin — daemon control surface.
//!
//!
//! - `$kernel` (no args) — status: version, socket, install state
//! - `$kernel update`    — fetch latest manifest, install if newer
//! - `$kernel reinstall` — force re-download even if up to date
//! - `$kernel logs`      — tail the kernel's stdout (last 200 lines)
//! - `$kernel --uninstall` — stop daemon, remove binary + service
//!
//! All long-running operations (download, install) delegate to
//! `orkia-kernel-installer`; this module is plumbing + formatting.

use std::path::Path;
use std::sync::Arc;

use orkia_capabilities::{Capability, CapabilityResolver};
use orkia_kernel_installer::{InstallLayout, Installer, current_platform, daemon};
use orkia_shell_types::BlockContent;

use crate::classifier::AdaptiveHandle;

const LOG_TAIL_LINES: usize = 200;

/// The kernel-manifest URL the installer talks to. Routed through
/// the canonical resolver (`ORKIA_BACKEND_URL` env override, with
/// `ORKIA_API_URL` honored as a deprecated alias for one release →
/// [`orkia_shell_types::backend::DEFAULT_BACKEND_URL`]). On a
/// misconfigured URL we fall back to the canonical default rather
/// than crashing the shell startup path.
fn api_url() -> String {
    orkia_shell_types::backend::resolve_backend_url(None)
        .unwrap_or_else(|_| orkia_shell_types::backend::DEFAULT_BACKEND_URL.to_string())
}

fn header(label: impl Into<String>) -> BlockContent {
    BlockContent::SystemInfo(format!(" {}", label.into()))
}

fn line(text: impl Into<String>) -> BlockContent {
    BlockContent::Text(text.into())
}

/// Dispatch `$kernel <subcommand>`. Returns rendered output the
/// caller pipes into `Outcome::BuiltinOutput`.
pub async fn dispatch(
    args: &[String],
    auth: Option<&Arc<dyn orkia_auth::AuthProvider>>,
    resolver: Option<&Arc<dyn CapabilityResolver>>,
    adaptive: Option<&AdaptiveHandle>,
) -> Vec<BlockContent> {
    let sub = args.first().map(String::as_str).unwrap_or("");
    match sub {
        "" | "status" => status(adaptive),
        "update" => update(auth, resolver).await,
        "reinstall" => reinstall(auth, resolver).await,
        "logs" => logs(),
        "models" => models(args.get(1..).unwrap_or(&[])).await,
        "benchmark" => benchmark(args.get(1..).unwrap_or(&[])).await,
        "--uninstall" | "uninstall" => uninstall(adaptive),
        other => vec![BlockContent::SystemInfo(format!(
            " ✗ unknown subcommand: kernel {other}"
        ))],
    }
}

async fn models(args: &[String]) -> Vec<BlockContent> {
    let sub = args.first().map(String::as_str).unwrap_or("list");
    match sub {
        "list" | "" => models_list(),
        "pull" => match args.get(1) {
            Some(id) => models_pull(id),
            None => vec![header("✗ usage: kernel models pull <id>")],
        },
        "cancel" => match args.get(1) {
            Some(id) => models_cancel(id),
            None => vec![header("✗ usage: kernel models cancel <id>")],
        },
        "gc" => models_gc(),
        other => vec![header(format!(
            "✗ unknown subcommand: kernel models {other}"
        ))],
    }
}

fn models_cancel(id: &str) -> Vec<BlockContent> {
    let Some(rpc) = orkia_kernel_client::discover() else {
        return vec![header("✗ kernel not running")];
    };
    use orkia_shell_types::KernelCancelOutcome::*;
    match rpc.cancel_pull(id) {
        Ok(Cancelled { id }) => vec![header(format!("✓ cancellation requested for {id}"))],
        Ok(NotFound { id }) => vec![header(format!("✗ no active download for {id}"))],
        Ok(Unsupported) => vec![header("✗ kernel does not support cancellation")],
        Err(e) => vec![header(format!("✗ {e}"))],
    }
}

fn models_list() -> Vec<BlockContent> {
    let Some(rpc) = orkia_kernel_client::discover() else {
        return vec![header("✗ kernel not running — `$login` to install")];
    };
    match rpc.list_models() {
        Ok(models) if models.is_empty() => vec![header("no models registered")],
        Ok(models) => {
            let mut out = vec![header("models")];
            for m in models {
                let mark = if m.installed { "✓" } else { "·" };
                out.push(line(format!(
                    "  {mark} {} v{} ({} MB) backend={}",
                    m.id,
                    m.version,
                    m.size_bytes / 1_000_000,
                    m.backend.unwrap_or_else(|| "?".into())
                )));
                if let Some(p) = m.path {
                    out.push(line(format!("       path: {p}")));
                }
            }
            out
        }
        Err(e) => vec![header(format!("✗ {e}"))],
    }
}

fn models_pull(id: &str) -> Vec<BlockContent> {
    let Some(rpc) = orkia_kernel_client::discover() else {
        return vec![header("✗ kernel not running")];
    };
    use orkia_shell_types::KernelPullOutcome::*;
    match rpc.pull_model(id) {
        Ok(Started { id, size_bytes }) => vec![
            header(format!("started downloading {id}")),
            line(format!(
                "  size: {} MB — progress visible in $kernel logs",
                size_bytes / 1_000_000
            )),
        ],
        Ok(AlreadyInstalled { id }) => vec![header(format!("✓ {id} is already installed"))],
        Ok(NotInRegistry { id }) => vec![header(format!("✗ {id} not in registry"))],
        Ok(Unsupported) => vec![header("✗ kernel does not support model pull")],
        Ok(Error { message }) => vec![header(format!("✗ {message}"))],
        Err(e) => vec![header(format!("✗ {e}"))],
    }
}

fn models_gc() -> Vec<BlockContent> {
    use orkia_kernel_installer::InstallLayout;
    let layout = InstallLayout::default_for_user();
    let models_dir = layout.root.join("models");
    let mut out = vec![header("kernel models gc")];

    // from RAM first. Deleting the file alone leaves the model
    // resident in process memory until the next kernel restart,
    // which made the gc command silently lie about freeing RAM.
    if let Some(rpc) = orkia_kernel_client::discover() {
        match rpc.evict_loaded() {
            Ok(orkia_shell_types::KernelEvictOutcome::Evicted { id }) => {
                out.push(line(format!("  ✓ evicted {id} from RAM")));
            }
            Ok(orkia_shell_types::KernelEvictOutcome::Nothing) => {}
            Ok(orkia_shell_types::KernelEvictOutcome::Unsupported) => {
                out.push(line("  ⚠ kernel does not support eviction"));
            }
            Err(e) => out.push(line(format!("  ⚠ evict failed: {e}"))),
        }
    }

    if !models_dir.exists() {
        out.push(line("  no cached models on disk"));
        return out;
    }
    let mut freed: u64 = 0;
    let mut removed = 0u32;
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(3_600);
    if let Ok(entries) = std::fs::read_dir(&models_dir) {
        for e in entries.flatten() {
            if e.path().extension().and_then(|s| s.to_str()) != Some("gguf") {
                continue;
            }
            let Ok(meta) = e.metadata() else { continue };
            let Ok(used) = meta.modified() else { continue };
            if used < cutoff {
                freed += meta.len();
                removed += 1;
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    out.push(line(format!(
        "  removed {removed} files, freed {} MB",
        freed / 1_000_000
    )));
    out
}

async fn benchmark(args: &[String]) -> Vec<BlockContent> {
    let rounds: u32 = args.first().and_then(|s| s.parse().ok()).unwrap_or(50);
    let Some(rpc) = orkia_kernel_client::discover() else {
        return vec![header("✗ kernel not running")];
    };
    use orkia_shell_types::KernelBenchmarkOutcome::*;
    match rpc.benchmark(rounds) {
        Ok(Ran {
            rounds,
            p50_ms,
            p95_ms,
            p99_ms,
            errors,
        }) => vec![
            header(format!("benchmark: {rounds} rounds, {errors} errors")),
            line(format!("  p50  {p50_ms} ms")),
            line(format!("  p95  {p95_ms} ms")),
            line(format!("  p99  {p99_ms} ms")),
        ],
        Ok(Unsupported) => vec![header(
            "✗ no model loaded — `$kernel models pull <id>` first",
        )],
        Err(e) => vec![header(format!("✗ {e}"))],
    }
}

fn status(adaptive: Option<&AdaptiveHandle>) -> Vec<BlockContent> {
    let layout = InstallLayout::default_for_user();
    let installed = layout.binary_path().exists();
    let connected = adaptive.map(|h| h.has_kernel()).unwrap_or(false);

    let mut out = Vec::new();
    out.push(header("orkia-kernel"));
    out.push(line(format!(
        "  binary:    {}",
        layout.binary_path().display()
    )));
    out.push(line(format!(
        "  installed: {}",
        if installed { "yes" } else { "no" }
    )));
    out.push(line(format!(
        "  connected: {}",
        if connected { "yes" } else { "no" }
    )));
    if connected && let Some(h) = adaptive {
        if let Some(rpc) = orkia_kernel_client::discover() {
            let v = rpc.version();
            out.push(line(format!(
                "  version:   {} (protocol {})",
                v.kernel, v.protocol
            )));
        }
        let _ = h;
    }
    out.push(line(format!(
        "  service:   {}",
        daemon::service_file_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "unsupported platform".into())
    )));
    out.push(line(format!(
        "  logs:      {}",
        layout.logs_dir().display()
    )));
    out
}

async fn update(
    auth: Option<&Arc<dyn orkia_auth::AuthProvider>>,
    resolver: Option<&Arc<dyn CapabilityResolver>>,
) -> Vec<BlockContent> {
    let (token, plan_ok) = read_token(auth, resolver);
    if !plan_ok {
        return vec![header(
            "✗ kernel updates require Solo Pro or above — run `login`",
        )];
    }
    let Some(token) = token else {
        return vec![header("✗ not signed in — run `login`")];
    };
    let Some(platform) = current_platform() else {
        return vec![header("✗ unsupported platform")];
    };
    let layout = InstallLayout::default_for_user();
    let installer = match Installer::new(layout.clone()) {
        Ok(i) => i,
        Err(e) => return vec![header(format!("✗ HTTP init failed: {e}"))],
    };
    let current = read_current_version(&layout);
    let mut out = vec![header("updating kernel...")];
    match installer
        .fetch_manifest(&api_url(), &token, platform, current.as_deref())
        .await
    {
        Ok(manifest) => {
            out.push(line(format!(
                "  target:  {} (platform {})",
                manifest.kernel_version,
                platform.as_str()
            )));
            install_and_register(&installer, &manifest, &token, &mut out).await;
        }
        Err(orkia_kernel_installer::InstallError::Manifest(msg))
            if msg.contains("304") || msg.contains("up to date") =>
        {
            out.push(line("  ✓ already up to date"));
        }
        Err(e) => out.push(line(format!("  ✗ {e}"))),
    }
    out
}

async fn reinstall(
    auth: Option<&Arc<dyn orkia_auth::AuthProvider>>,
    resolver: Option<&Arc<dyn CapabilityResolver>>,
) -> Vec<BlockContent> {
    let (token, plan_ok) = read_token(auth, resolver);
    if !plan_ok {
        return vec![header(
            "✗ kernel reinstall requires Solo Pro or above — run `login`",
        )];
    }
    let Some(token) = token else {
        return vec![header("✗ not signed in — run `login`")];
    };
    let Some(platform) = current_platform() else {
        return vec![header("✗ unsupported platform")];
    };
    let layout = InstallLayout::default_for_user();
    let installer = match Installer::new(layout.clone()) {
        Ok(i) => i,
        Err(e) => return vec![header(format!("✗ HTTP init failed: {e}"))],
    };
    let mut out = vec![header("reinstalling kernel from scratch...")];
    match installer
        .fetch_manifest(&api_url(), &token, platform, None)
        .await
    {
        Ok(manifest) => install_and_register(&installer, &manifest, &token, &mut out).await,
        Err(e) => out.push(line(format!("  ✗ {e}"))),
    }
    out
}

async fn install_and_register(
    installer: &Installer,
    manifest: &orkia_kernel_installer::Manifest,
    bearer: &str,
    out: &mut Vec<BlockContent>,
) {
    let bytes = match installer.download(manifest, bearer).await {
        Ok(b) => b,
        Err(e) => {
            out.push(line(format!("  ✗ download: {e}")));
            return;
        }
    };
    if let Err(e) = installer.verify(&bytes, manifest) {
        out.push(line(format!("  ✗ verify: {e}")));
        return;
    }
    out.push(line(format!("  ✓ verified ({} bytes)", bytes.len())));

    // running kernel (if any) to flush + exit. Without this the
    // atomic mv replaces the binary under the running process's
    // feet, then launchctl reaps it mid-RPC. The graceful path
    // lets in-flight inferences complete + the journal flush.
    if let Some(rpc) = orkia_kernel_client::discover() {
        let _ = rpc.shutdown();
        wait_for_socket_close(std::time::Duration::from_secs(10)).await;
        out.push(line("  ✓ existing kernel asked to shut down"));
    }

    if let Err(e) = installer.install(&bytes) {
        out.push(line(format!("  ✗ install: {e}")));
        return;
    }
    out.push(line(format!(
        "  ✓ installed at {}",
        installer.layout().binary_path().display()
    )));
    // The shared-library closure (ggml/llama) ships as extra_files; the
    // kernel references them by @rpath soname and ABORTS at launch when
    // they're missing — install them before starting the daemon.
    if let Err(e) = installer.install_extra_files(manifest, bearer).await {
        out.push(line(format!("  ✗ shared libraries: {e}")));
        return;
    }
    if !manifest.extra_files.is_empty() {
        out.push(line(format!(
            "  ✓ {} shared libraries installed",
            manifest.extra_files.len()
        )));
    }
    match daemon::register(installer.layout()) {
        Ok(()) => out.push(line("  ✓ daemon registered + started")),
        Err(e) => out.push(line(format!("  ⚠ daemon register: {e}"))),
    }
}

async fn wait_for_socket_close(max: std::time::Duration) {
    let sock = orkia_kernel_client::default_socket_path();
    let start = std::time::Instant::now();
    while start.elapsed() < max {
        if !sock.exists() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

fn logs() -> Vec<BlockContent> {
    let layout = InstallLayout::default_for_user();
    let stdout = layout.logs_dir().join("stdout.log");
    let stderr = layout.logs_dir().join("stderr.log");
    let mut out = vec![header("kernel logs (last 200 lines, stdout then stderr)")];
    for (label, path) in [("stdout", stdout.as_path()), ("stderr", stderr.as_path())] {
        out.push(line(format!("  ── {label} ── {}", path.display())));
        match tail_file(path, LOG_TAIL_LINES) {
            Ok(lines) if lines.is_empty() => out.push(line("    (empty)")),
            Ok(lines) => {
                for l in lines {
                    out.push(line(format!("    {l}")));
                }
            }
            Err(e) => out.push(line(format!("    ✗ {e}"))),
        }
    }
    out
}

fn uninstall(adaptive: Option<&AdaptiveHandle>) -> Vec<BlockContent> {
    let mut out = vec![header("uninstalling kernel...")];
    if let Some(rpc) = orkia_kernel_client::discover() {
        let _ = rpc.shutdown();
        out.push(line("  ✓ daemon shutdown requested"));
    }
    if let Some(h) = adaptive {
        h.clear_kernel();
    }
    match daemon::unregister() {
        Ok(()) => out.push(line("  ✓ service file removed")),
        Err(e) => out.push(line(format!("  ⚠ {e}"))),
    }
    let layout = InstallLayout::default_for_user();
    let bin = layout.binary_path();
    if bin.exists() {
        match std::fs::remove_file(&bin) {
            Ok(()) => out.push(line(format!("  ✓ removed {}", bin.display()))),
            Err(e) => out.push(line(format!("  ⚠ remove {}: {e}", bin.display()))),
        }
    }
    out
}

fn tail_file(path: &Path, n: usize) -> std::io::Result<Vec<String>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path)?;
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    Ok(lines[start..].iter().map(|s| (*s).to_string()).collect())
}

fn read_current_version(layout: &InstallLayout) -> Option<String> {
    let _ = layout;
    None
}

fn read_token(
    auth: Option<&Arc<dyn orkia_auth::AuthProvider>>,
    resolver: Option<&Arc<dyn CapabilityResolver>>,
) -> (Option<String>, bool) {
    let token = auth.and_then(|p| p.bearer());
    let plan_ok = resolver
        .map(|r| r.current().has(Capability::CognitiveRouting))
        .unwrap_or(false);
    (token, plan_ok)
}

/// Called by `$login` when the user just authenticated on a paid
/// plan but the kernel isn't installed locally. Surfaces a
/// progress-only block; full install runs inline.
pub async fn auto_install_after_login(
    auth: Option<&Arc<dyn orkia_auth::AuthProvider>>,
    resolver: Option<&Arc<dyn CapabilityResolver>>,
) -> Vec<BlockContent> {
    let (token, plan_ok) = read_token(auth, resolver);
    if !plan_ok {
        return Vec::new();
    }
    let Some(token) = token else {
        return Vec::new();
    };
    let layout = InstallLayout::default_for_user();
    if layout.binary_path().exists() {
        return Vec::new();
    }
    let Some(platform) = current_platform() else {
        return Vec::new();
    };
    let installer = match Installer::new(layout) {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, "kernel auto-install skipped: HTTP client init failed");
            return Vec::new();
        }
    };
    let mut out = vec![header("installing kernel for your plan...")];
    match installer.install_latest(&api_url(), &token, platform).await {
        Ok(manifest) => {
            out.push(line(format!(
                "  ✓ installed orkia-kernel {} ({})",
                manifest.kernel_version,
                platform.as_str()
            )));
            match daemon::register(installer.layout()) {
                Ok(()) => out.push(line("  ✓ daemon registered + started")),
                Err(e) => out.push(line(format!("  ⚠ daemon register: {e}"))),
            }
        }
        Err(e) => out.push(line(format!(
            "  ⚠ kernel install failed: {e}. Run `$kernel update` to retry."
        ))),
    }
    out
}
