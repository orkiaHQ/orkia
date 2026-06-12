// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `login`, `logout`, `whoami`, `plan` shell builtins.
//!
//! These are shell-level builtins (not CLI subcommands) so they can
//! mutate the live REPL's capability state without restart. They run
//! against any [`AuthProvider`] the binary injects — proprietary
//! Orkia OAuth, env-var bearer, mock for tests, etc. The shell crate
//! itself names no backend.
//!
//! All output uses `BlockContent::SystemInfo`/`Text` so it renders
//! consistently with the rest of the shell.

use std::sync::Arc;

use orkia_auth::{AuthError, AuthEvent, AuthProvider, SessionInfo};
use orkia_capabilities::{
    Capability, CapabilityResolver, CapabilitySet, Plan, capabilities_for_plan,
};
use orkia_kernel_client::discover;
use orkia_shell_types::BlockContent;

use crate::classifier::AdaptiveHandle;

/// A read-only [`AuthView`] over the live auth handles. Built at command
/// dispatch and carried on `CommandCtx`, it lets the migrated `whoami`/`plan`
/// Commands render identity state without `orkia-shell-types` depending on the
pub struct ShellAuthView {
    pub auth: Option<Arc<dyn AuthProvider>>,
    pub resolver: Option<Arc<dyn CapabilityResolver>>,
    pub adaptive: Option<AdaptiveHandle>,
}

impl orkia_shell_types::AuthView for ShellAuthView {
    fn whoami_lines(&self) -> Vec<String> {
        crate::exec::commands::blocks_adapter::blocks_to_lines(whoami(
            self.auth.as_ref(),
            self.resolver.as_ref(),
            self.adaptive.as_ref(),
        ))
    }

    fn plan_lines(&self) -> Vec<String> {
        crate::exec::commands::blocks_adapter::blocks_to_lines(plan(
            self.auth.as_ref(),
            self.resolver.as_ref(),
        ))
    }
}

fn header(label: impl Into<String>) -> BlockContent {
    BlockContent::SystemInfo(format!(" {}", label.into()))
}

fn line(text: impl Into<String>) -> BlockContent {
    BlockContent::Text(text.into())
}

fn err(msg: impl Into<String>) -> Vec<BlockContent> {
    vec![BlockContent::SystemInfo(format!(" ✗ {}", msg.into()))]
}

/// `login` — run the configured auth provider, refresh capabilities,
/// attempt to attach a kernel if the resulting plan unlocks one.
pub async fn login(
    auth: Option<&Arc<dyn AuthProvider>>,
    resolver: Option<&Arc<dyn CapabilityResolver>>,
    adaptive: Option<&AdaptiveHandle>,
) -> Vec<BlockContent> {
    let Some(provider) = auth else {
        return err(
            "login: no auth provider configured (run the `orkia` binary, which wires the magic-link login)",
        );
    };
    let provider = provider.clone();
    let mut blocks: Vec<BlockContent> = Vec::new();
    let (login_result, captured_url) = {
        let p = provider.clone();
        let captured: std::sync::Arc<std::sync::Mutex<Option<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured_clone = captured.clone();
        let res = tokio::task::spawn_blocking(move || {
            let mut sink = |ev: AuthEvent| {
                if let AuthEvent::OpeningBrowser { auth_url } = ev
                    && let Ok(mut slot) = captured_clone.lock()
                {
                    *slot = Some(auth_url);
                }
            };
            p.login(&mut sink)
        })
        .await
        .unwrap_or_else(|_| Err(AuthError::Backend("login worker join failed".into())));
        let url = captured.lock().ok().and_then(|mut g| g.take());
        (res, url)
    };
    match login_result {
        Ok(session) => {
            if let Some(u) = captured_url {
                blocks.push(line(format!("  ▸ opened browser: {u}")));
            }
            blocks.push(header(format!(
                "✓ signed in as @{} (plan: {})",
                session.display_name, session.plan
            )));
            let plan = Plan::parse(&session.plan);
            let caps_preview = capabilities_for_plan(plan);
            if caps_preview.is_empty() {
                blocks.push(line(
                    "  welcome — you're on the free tier; cognitive features require Solo Pro or above",
                ));
            }
            if let Some(r) = resolver {
                r.refresh();
                let install_blocks =
                    crate::kernel_builtins::auto_install_after_login(auth, Some(r)).await;
                blocks.extend(install_blocks);
            }
            attach_kernel(resolver, adaptive, &mut blocks);
            blocks
        }
        Err(AuthError::Cancelled) => err("login timed out or was cancelled"),
        Err(AuthError::Misconfigured(msg)) => err(format!("login misconfigured: {msg}")),
        Err(e) => err(format!("login failed: {e}")),
    }
}

/// `logout` — clear local credentials, ask kernel to shut down,
/// detach the adaptive handle so subsequent classifications go
/// heuristic.
pub async fn logout(
    auth: Option<&Arc<dyn AuthProvider>>,
    resolver: Option<&Arc<dyn CapabilityResolver>>,
    adaptive: Option<&AdaptiveHandle>,
) -> Vec<BlockContent> {
    // Best-effort kernel shutdown before clearing the token.
    if let Some(h) = adaptive {
        if h.has_kernel()
            && let Some(rpc) = discover()
        {
            let _ = rpc.shutdown();
        }
        h.clear_kernel();
    }

    let Some(provider) = auth else {
        return vec![header("✓ logged out · no auth provider configured")];
    };
    let p = provider.clone();
    let result = tokio::task::spawn_blocking(move || p.logout())
        .await
        .unwrap_or_else(|_| Err(AuthError::Backend("logout worker join failed".into())));
    if let Some(r) = resolver {
        r.refresh();
    }
    match result {
        Ok(()) => vec![header("✓ logged out · using local heuristic classifier")],
        Err(e) => vec![
            header("⚠ local logout best-effort; provider reported an error"),
            line(format!("    {e}")),
        ],
    }
}

/// `whoami` — render account + plan + capabilities + kernel status
/// from local state. No network round-trip.
pub fn whoami(
    auth: Option<&Arc<dyn AuthProvider>>,
    resolver: Option<&Arc<dyn CapabilityResolver>>,
    adaptive: Option<&AdaptiveHandle>,
) -> Vec<BlockContent> {
    let session = auth.and_then(|p| p.current());
    let caps = resolver
        .map(|r| r.current())
        .unwrap_or_else(CapabilitySet::empty);
    render_whoami(session, &caps, adaptive)
}

/// `plan` — short list of unlocked capabilities. Useful as a quick
/// "what am I paying for" reference.
pub fn plan(
    auth: Option<&Arc<dyn AuthProvider>>,
    resolver: Option<&Arc<dyn CapabilityResolver>>,
) -> Vec<BlockContent> {
    let session = auth.and_then(|p| p.current());
    let caps = resolver
        .map(|r| r.current())
        .unwrap_or_else(CapabilitySet::empty);
    let mut out = Vec::new();
    match &session {
        Some(s) => out.push(header(format!(
            "plan: {} (account @{})",
            s.plan, s.display_name
        ))),
        None => out.push(header("plan: free (not signed in)")),
    }
    if caps.is_empty() {
        out.push(line(
            "  no premium capabilities — run `login` to unlock cognitive features",
        ));
    } else {
        out.push(line("  unlocked capabilities:"));
        for cap in caps.iter() {
            out.push(line(format!("    • {}", describe(cap))));
        }
    }
    out
}

fn render_whoami(
    session: Option<SessionInfo>,
    caps: &CapabilitySet,
    adaptive: Option<&AdaptiveHandle>,
) -> Vec<BlockContent> {
    let mut out = Vec::new();
    match session {
        Some(s) => {
            out.push(header(format!("@{} · {}", s.display_name, s.email)));
            out.push(line(format!("  plan:    {}", s.plan)));
            out.push(line(format!("  issued:  {}", s.issued_at.to_rfc3339())));
            if let Some(exp) = s.expires_at {
                out.push(line(format!("  expires: {}", exp.to_rfc3339())));
            }
        }
        None => {
            out.push(header("not signed in"));
            out.push(line("  run `login` to authenticate"));
        }
    }

    out.push(line(format!(
        "  kernel:  {}",
        match adaptive {
            Some(h) if h.has_kernel() => "connected",
            Some(_) => "not connected (heuristic only)",
            None => "n/a",
        }
    )));

    if !caps.is_empty() {
        out.push(line("  capabilities:"));
        for cap in caps.iter() {
            out.push(line(format!("    • {}", describe(cap))));
        }
    }
    out
}

fn describe(cap: Capability) -> &'static str {
    match cap {
        Capability::CognitiveRouting => "cognitive routing (local LLM intent classification)",
        Capability::ContextCompression => "context compression (local embeddings + summarization)",
        Capability::CognitiveRouter => "cognitive router (local/cloud arbitration)",
        Capability::TeamPipeline => "team pipeline (@a | @b multi-agent chains)",
        Capability::SealAuditExtended => "extended SEAL audit retention",
        Capability::ForgeBuild => "forge build & generation (premium)",
    }
}

fn attach_kernel(
    resolver: Option<&Arc<dyn CapabilityResolver>>,
    adaptive: Option<&AdaptiveHandle>,
    blocks: &mut Vec<BlockContent>,
) {
    let Some(r) = resolver else {
        return;
    };
    r.refresh();
    let caps = r.current();
    let Some(h) = adaptive else {
        return;
    };
    if !caps.has(Capability::CognitiveRouting) {
        h.clear_kernel();
        blocks.push(line(
            "  on the free tier — cognitive features require Solo Pro or above",
        ));
        return;
    }
    match discover() {
        Some(rpc) => {
            let v = rpc.version();
            h.set_kernel(rpc);
            blocks.push(line(format!(
                "  kernel: connected (v{}, protocol {})",
                v.kernel, v.protocol
            )));
        }
        None => {
            blocks.push(line(
                "  kernel: not installed — install orkia-kernel to enable cognitive features",
            ));
        }
    }
}
