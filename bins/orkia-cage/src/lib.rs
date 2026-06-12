// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `orkia-cage` — the execution-boundary launcher for vendor agents.
//!
//! Parses args + loads the `Policy`, then `launch`es the agent under per-OS
//! via `sandbox-exec` (`sbpl`); **Linux** → an unprivileged minimal-rootfs
//! sandbox (`linux_sb`); other unix → passthrough. Sub-command mediation
//! (`orkia-sh`) and the SEAL verdict tap are not yet implemented.
//!
//! Usage:
//! ```text
//! orkia-cage --policy policy.toml -- claude [agent args...]
//! ```
//!
//! Fail-closed: any error (missing `--policy`, missing `--`/empty agent argv,
//! unreadable/unparseable policy, or — on Linux — unavailable namespaces with no
//! `allow_unconfined`) prints to stderr and exits non-zero **without** running
//! the agent.

// Used by the macOS + other-unix passthrough `exec`; on Linux the namespaced
// exec lives in `linux_sb`, so this import would be unused there.
#[cfg(not(target_os = "linux"))]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::Parser;
use orkia_shell_types::{
    NoopTrustAdjuster, PendingStore, Policy, TrustAdjuster, TrustScope, UnlockStore, apply_trust,
    resolve_project_id,
};

#[cfg(target_os = "linux")]
mod linux_sb;
#[cfg(target_os = "macos")]
mod sbpl;
#[cfg(target_os = "linux")]
mod toolset;

#[derive(Debug, Parser)]
#[command(
    name = "orkia-cage",
    about = "Run a vendor agent inside the Orkia execution boundary",
    long_about = "Run a vendor agent inside the Orkia execution boundary.\n\n\
        Enforcement posture (V1):\n\
        - macOS: per-binary deprivation (kernel-enforced Seatbelt) + best-effort \
        per-command mediation via the agent's tool protocol (bypassable by an \
        autonomous shell) + SEAL audit.\n\
        - Linux: filesystem boundary (minimal rootfs) + reliable per-command \
        mediation via orkia-sh (the only shell in the cage) + SEAL audit.\n\
        Reliable per-command granularity is Linux-only in V1. Network is not \
        scoped in V1."
)]
pub struct Args {
    #[arg(long, value_name = "PATH")]
    policy: PathBuf,

    /// The agent command and its arguments, everything after `--`.
    #[arg(last = true, value_name = "AGENT")]
    agent: Vec<String>,
}

/// Proprietary trust-adjuster hook (Model A): the OSS `orkia-cage` bin passes `None` (→
/// `NoopTrustAdjuster`, inert), while an enterprise bin passes
/// `Some(Arc::new(AtlasScorer::new(...)))`. The adjuster only ever yields an
/// `AskOutcome` through the public `apply_trust`, so it cannot bypass the
/// Deny-untouchable / sensitive-needs-unlock guarantees.
pub fn run(args: Args, adjuster: Option<Arc<dyn TrustAdjuster>>) -> Result<()> {
    let (program, agent_args) = split_agent(&args.agent)?;
    let base_policy = load_policy(&args.policy)?;

    // holds a DI slot for the adjuster and calls the **public** `apply_trust` with
    // it. The OSS build registers nothing (`None` → `NoopTrustAdjuster`), so the
    // returned policy equals the loaded file — **inert in V1**. An enterprise
    // build registers its scoring adjuster in that slot; because it supplies an
    // *adjuster* (an `AskOutcome`), never a `Policy`, it cannot bypass
    // `apply_trust`: that function structurally cannot turn a `Deny` into an
    // `Allow`, promote a *sensitive* capability without a recorded human unlock,
    // or promote anything for an untrusted scope (empty agent / unresolved
    // project). A promoted policy is re-serialized for the macOS hook by
    // `effective_policy_path`, so per-command mediation matches the SBPL profile.
    let scope = trust_scope();
    // Make the stable project id visible to the in-cage `orkia-sh` shim so it can
    // stamp `cage.verdict` evidence with the project the trust scorer keys on. It
    // rides the `ORKIA_` env allowlist into both the macOS and Linux agent env.
    if let Some(project) = &scope.project {
        // SAFETY: the cage is single-threaded here — no threads are spawned before
        // `launch`, which then `exec`s. No concurrent env access can race.
        unsafe {
            std::env::set_var("ORKIA_PROJECT_ID", &project.0);
        }
    }
    let unlocks = UnlockStore::load(&unlock_store_path());
    let registered = adjuster;
    let noop = NoopTrustAdjuster;
    let adjuster: &dyn TrustAdjuster = match &registered {
        Some(a) => a.as_ref(),
        None => &noop,
    };
    let policy = apply_trust(&base_policy, &scope, adjuster, &unlocks);

    // Surface the scorer's eligibility signals for cold human review: rewrite
    // *this* scope's entries in the pending list. The OSS `NoopTrustAdjuster`
    // reports none, so this is inert (no file is created/changed) unless an
    // enterprise scorer is registered. It proposes; it never grants.
    surface_eligibility(adjuster, &base_policy, &scope, &unlocks);

    // The path the macOS PreToolUse hook re-reads for per-command mediation.
    // Absolute so it resolves regardless of the agent's cwd. Linux instead
    // sees promotions; macOS must be pointed at the *promoted* policy explicitly,
    // else the hook would mediate against the base file while the SBPL profile
    // (built from `policy`) reflects promotions — an inconsistency the moment a
    // real adjuster promotes. When inert (Noop ⇒ `policy == base_policy`) we keep
    // the original file untouched, so OSS behaviour stays byte-identical.
    let canonical = args
        .policy
        .canonicalize()
        .unwrap_or_else(|_| args.policy.clone());
    let policy_path = effective_policy_path(&canonical, &policy, &base_policy)?;

    tracing::info!(
        policy = %args.policy.display(),
        workspace = %policy.workspace.root.display(),
        caps = policy.capabilities.len(),
        default = ?policy.default_verdict,
        agent = %program,
        trust = if registered.is_some() { "scoring" } else { "noop" },
        "orkia-cage: policy loaded"
    );

    // one that predates `[caps]` and so defaults all-false — leaves the agent
    // unable to see its workspace or run any command. Warn loudly; this is
    // correct fail-closed behaviour, but silently bricking a pre-caps policy
    // would read as a bug. (The cage still proceeds — deny is the safe default.)
    if !policy.caps.read && !policy.caps.write && !policy.caps.exec {
        tracing::warn!(
            "all capability classes are off (read+write+exec) — the agent cannot see its \
             workspace (ENOENT) or run any command (exec denied). If this policy predates \
             per-capability classes, add a [caps] block to grant classes (e.g. `cap @<agent> +read \
             +exec`); otherwise this fail-closed posture is intentional."
        );
    }

    launch(program, agent_args, &policy, &policy_path)
}

/// what the adjuster reports — recompute-at-spawn. Best-effort and **inert under
/// Noop** (it reports no eligibility, and `update_scope` reports no change, so no
/// file is written). Only a trusted scope (concrete agent + project) is recorded.
fn surface_eligibility(
    adjuster: &dyn TrustAdjuster,
    base_policy: &Policy,
    scope: &TrustScope,
    unlocks: &UnlockStore,
) {
    let Some(project) = &scope.project else {
        return;
    };
    let agent = scope.agent.trim();
    if agent.is_empty() {
        return;
    }
    let eligible = adjuster.eligibility(base_policy, scope, unlocks);
    let path = pending_store_path();
    let mut pending = PendingStore::load(&path);
    if pending.update_scope(agent, project, &eligible) {
        let _ = pending.save(&path);
    }
}

/// Where the eligibility pending list lives — alongside the unlocks, under
/// `~/.orkia/trust/`. A derived review cache, not authority.
fn pending_store_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".orkia/trust/pending.json")
}

/// `ORKIA_AGENT_NAME`) plus the stable project id resolved from the git root of
/// the current dir — **not** the raw cwd, so a subdir `cd` cannot move scope. An
/// empty agent or an unresolved project yields an untrusted scope (no promotion).
fn trust_scope() -> TrustScope {
    let agent = std::env::var("ORKIA_AGENT_NAME").unwrap_or_default();
    let project = std::env::current_dir()
        .ok()
        .and_then(|cwd| resolve_project_id(&cwd));
    TrustScope { agent, project }
}

/// The policy path the macOS hook re-reads. When `policy` equals `base` (the
/// inert Noop path) it is the original file — unchanged, byte-identical. When a
/// trust adjuster promoted something, the promoted policy is serialized to a
/// stable per-session file under `~/.orkia/run/` and that path is returned, so
/// the per-command hook mediates against exactly what the SBPL profile enforces.
fn effective_policy_path(canonical: &Path, policy: &Policy, base: &Policy) -> Result<PathBuf> {
    if policy == base {
        return Ok(canonical.to_path_buf());
    }
    let run_dir = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".orkia/run");
    std::fs::create_dir_all(&run_dir).context("create ~/.orkia/run for promoted policy")?;
    // Stable for the session: keyed on the job id when present, else this pid, so
    // concurrent caged agents don't clobber each other's promoted policy file.
    let key = std::env::var("ORKIA_JOB_ID").unwrap_or_else(|_| std::process::id().to_string());
    let path = run_dir.join(format!("cage-policy.{key}.toml"));
    let body = toml::to_string(policy).context("serialize promoted policy")?;
    std::fs::write(&path, body)
        .with_context(|| format!("write promoted policy {}", path.display()))?;
    Ok(path)
}

/// loads as an empty store (fail-closed: no unlocks ⇒ no sensitive promotion).
fn unlock_store_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".orkia/trust/unlocks.json")
}

/// policy + per-agent corpus and replace this process with the agent running
/// under `sandbox-exec`. `exec` only returns on failure.
#[cfg(target_os = "macos")]
fn launch(program: &str, agent_args: &[String], policy: &Policy, policy_path: &Path) -> Result<()> {
    let spec = build_profile_spec(program, policy)?;
    let profile = sbpl::build_profile(&spec);
    if std::env::var_os("ORKIA_CAGE_DEBUG_PROFILE").is_some() {
        // Dump the generated profile to stdout and exit WITHOUT exec'ing, so the
        // dump is clean (no agent / sandbox-exec output mixed in). Debug aid.
        println!("{profile}");
        return Ok(());
    }
    tracing::info!(
        workspace = %spec.workspace.display(),
        deny_write = spec.deny_write.len(),
        deny_exec = spec.deny_exec.len(),
        "orkia-cage: macOS SBPL profile generated"
    );

    let mut cmd = std::process::Command::new("/usr/bin/sandbox-exec");
    cmd.arg("-p").arg(&profile).arg(program).args(agent_args);
    apply_env_allowlist(&mut cmd);
    // The PreToolUse mediation hook (`orkia-sh hook`) loads the cage policy from
    // (treats the session as uncaged). Set AFTER the allowlist, which clears the
    // env — Linux instead serializes the policy into the rootfs (`linux_agent_env`).
    cmd.env("ORKIA_CAGE_POLICY", policy_path);
    let err = cmd.exec();
    Err(anyhow::anyhow!(
        "failed to exec sandbox-exec for `{program}`: {err}"
    ))
}

/// sandbox (user+mount ns, tmpfs root, allowlist binds, pivot_root, /proc),
/// install `orkia-sh` as the sole shell for command mediation, then exec the agent.
#[cfg(target_os = "linux")]
fn launch(
    program: &str,
    agent_args: &[String],
    policy: &Policy,
    _policy_path: &Path,
) -> Result<()> {
    let spec = build_rootfs_spec(program, policy)?;
    tracing::info!(
        workspace = %spec.workspace.display(),
        ro = spec.ro_binds.len(),
        rw = spec.rw_binds.len(),
        mediated = spec.shim.is_some(),
        "orkia-cage: entering Linux minimal-rootfs"
    );
    linux_sb::enter_and_exec(
        &spec,
        program,
        agent_args,
        &linux_agent_env(spec.shim.is_some()),
    )
}

/// The agent env on Linux: the scrub-then-allow allowlist plus, when the shim is
/// installed, the contract `orkia-sh` reads (policy path, preserved real shell,
/// verdict emitter). These ride the agent env so they reach the shim when the
#[cfg(target_os = "linux")]
fn linux_agent_env(mediated: bool) -> Vec<(String, String)> {
    let mut env = allowed_env();
    if mediated {
        env.push(("ORKIA_CAGE_POLICY".into(), linux_sb::IN_CAGE_POLICY.into()));
        env.push(("ORKIA_SH_REAL".into(), linux_sb::IN_CAGE_REAL_SH.into()));
    }
    env
}

/// Build the Linux rootfs allowlist from the policy + per-agent corpus.
#[cfg(target_os = "linux")]
fn build_rootfs_spec(program: &str, policy: &Policy) -> Result<linux_sb::RootfsSpec> {
    let workspace = resolve_workspace(&policy.workspace.root)?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    // D1: the library/runtime closure stays coarse-ro, but the bin dirs are
    // tightened to a per-binary allowlist — only allowlisted tools exist; the
    // ~1100 parasitic host binaries are absent by construction. Sensitive home
    // dirs remain unbound (absent by construction).
    let layout = toolset::system_layout();
    let ro_binds = layout.coarse_ro;
    let tools = toolset::resolve_tools(program, policy, home.as_deref());
    let (_corpus_ro, mut rw_binds) = corpus_for_agent(program, home.as_deref());
    if let Some(h) = &home {
        // isn't silently skipped if it doesn't pre-exist.
        let run_dir = h.join(".orkia/run");
        let _ = std::fs::create_dir_all(&run_dir);
        rw_binds.push(run_dir);
    }
    // Within-workspace deny sets. Write-deny: config-injection vectors the
    // agent reads but must not rewrite. Read-deny: secrets whose contents are
    // hidden entirely.
    let deny_write = [".git/config", ".git/hooks", ".mcp.json"]
        .iter()
        .map(|rel| workspace.join(rel))
        .collect();
    let deny_read = [".env"].iter().map(|rel| workspace.join(rel)).collect();
    // shim is resolvable; serialize the policy so the in-cage shim re-reads it.
    let shim = orkia_sh_path();
    let policy_toml = toml::to_string(policy).context("serialize policy for in-cage shim")?;
    Ok(linux_sb::RootfsSpec {
        workspace,
        workspace_read: policy.caps.read,
        workspace_write: policy.caps.write,
        ro_binds,
        bin_dirs: layout.bin_dirs,
        bin_symlinks: layout.symlinks,
        tools,
        rw_binds,
        deny_write,
        deny_read,
        allow_unconfined: std::env::var_os("ORKIA_CAGE_ALLOW_UNCONFINED").is_some(),
        shim,
        real_shell: real_shell_path(),
        shadow_shells: shadow_shell_paths(),
        policy_toml,
    })
}

/// The real shell `orkia-sh` execs for allowed commands: first existing of the
/// candidates (bash preferred — agents wrap commands for bash). `ORKIA_SH_REAL`
/// overrides the preserved target name only; the binary is chosen here.
#[cfg(target_os = "linux")]
fn real_shell_path() -> PathBuf {
    ["/bin/bash", "/usr/bin/bash", "/bin/sh", "/usr/bin/sh"]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("/bin/bash"))
}

/// Shell paths to shadow with the shim inside the cage. Each is shadowed only if
/// it exists in the rootfs; covers the common bash/sh/dash locations.
#[cfg(target_os = "linux")]
fn shadow_shell_paths() -> Vec<PathBuf> {
    [
        "/bin/sh",
        "/bin/bash",
        "/bin/dash",
        "/usr/bin/sh",
        "/usr/bin/bash",
        "/usr/bin/dash",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect()
}

/// Other unix (not macOS, not Linux): no enforcement mechanism — passthrough.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn launch(
    program: &str,
    agent_args: &[String],
    _policy: &Policy,
    _policy_path: &Path,
) -> Result<()> {
    let err = std::process::Command::new(program).args(agent_args).exec();
    Err(anyhow::anyhow!("failed to exec agent `{program}`: {err}"))
}

#[cfg(target_os = "macos")]
fn build_profile_spec(program: &str, policy: &Policy) -> Result<sbpl::ProfileSpec> {
    let workspace = resolve_workspace(&policy.workspace.root)?;
    let home = std::env::var_os("HOME").map(PathBuf::from);

    let (corpus_ro, mut corpus_rw) = corpus_for_agent(program, home.as_deref());
    let mut read_only = corpus_ro;
    // System read surface so toolchains (TLS, DNS, tz) don't break silently…
    for sys in [
        "/private/etc/hosts",
        "/private/etc/resolv.conf",
        "/private/etc/ssl",
        "/private/var/db/timezone",
        "/usr/lib",
        "/usr/share",
        "/System/Library",
    ] {
        read_only.push(PathBuf::from(sys));
    }
    // …and the binary dirs so bare command names resolve through the shell
    // ls` / bare `git` fail and the agent's Bash tool breaks. Coarse for V1
    // (whole dirs); tighten to per-binary later (D1).
    for bindir in [
        "/bin",
        "/sbin",
        "/usr/bin",
        "/usr/sbin",
        "/opt/homebrew/bin",
        "/opt/homebrew/sbin",
        "/usr/local/bin",
    ] {
        read_only.push(PathBuf::from(bindir));
    }
    // keychain is **read-only** — agents read credentials from it (the
    // SecurityServer mach service is already allowed in the preamble), but must
    // not modify the user's keychain.
    if let Some(h) = &home {
        for shared in [".skills", ".agents"] {
            corpus_rw.push(h.join(shared));
        }
        read_only.push(h.join("Library/Keychains"));
        // git aborts (EACCES is fatal, unlike a missing file) if it can't read
        // an existing global config. This is git identity/aliases, not secrets —
        // `~/.git-credentials` stays deny-default. Without it the agent's git is
        // unusable under the cage even though the binary + dylibs now load.
        read_only.push(h.join(".gitconfig"));
        read_only.push(h.join(".config/git"));
        // Agent runtimes install their launcher here (e.g. ~/.local/bin/claude →
        // ~/.local/share/<agent>/…); needed so `sandbox-exec` can resolve+exec
        // the bare agent name. The versioned payload dir is in the corpus.
        read_only.push(h.join(".local/bin"));
        // macOS per-user caches (agents write here during a session).
        corpus_rw.push(h.join("Library/Caches"));
    }
    // The per-user temp dir (macOS `$TMPDIR` = /var/folders/…) — agents write
    // session temp files there; without it a full session breaks silently
    // (a bare `--version` works, but `-p` does not).
    if let Some(tmp) = std::env::var_os("TMPDIR") {
        corpus_rw.push(PathBuf::from(tmp));
    }
    // claude stages its **Bash-tool** scratch under `/tmp/claude-<uid>/` (literal
    // `/tmp`, NOT `$TMPDIR`); without it the agent's shell tool can't write its
    // command files and runs silently fail (closes CAGE-S4.M05). macOS firmlinks
    // `/tmp`→`/private/tmp` and Seatbelt matches the resolved path, so allow the
    // canonical form too. Scoped to claude — other agents don't use this path.
    if orkia_shell_types::ProviderId::from_command(program) == orkia_shell_types::ProviderId::Claude
    {
        let uid = current_uid();
        corpus_rw.push(PathBuf::from(format!("/tmp/claude-{uid}")));
        corpus_rw.push(PathBuf::from(format!("/private/tmp/claude-{uid}")));
    }

    let mut deny_write = sbpl::mandatory_workspace_denies(&workspace);
    // Sensitive home dirs: denied by read-deny-default already; the write/mv
    // hardening here blocks laundering them into the workspace.
    if let Some(h) = &home {
        for sensitive in [".ssh", ".aws", ".config/gcloud", ".gnupg"] {
            deny_write.push(h.join(sensitive));
        }
    }

    Ok(sbpl::ProfileSpec {
        log_tag: log_tag(),
        workspace,
        workspace_read: policy.caps.read,
        workspace_write: policy.caps.write,
        read_only,
        read_write: corpus_rw,
        deny_write,
        deny_exec: derive_deny_exec(policy),
    })
}

/// Correlation tag stamped on deny rules. Prefer the shell-injected job id so a
#[cfg(target_os = "macos")]
fn log_tag() -> String {
    if let Some(id) = std::env::var_os("ORKIA_JOB_ID") {
        return format!("orkia-job-{}", id.to_string_lossy());
    }
    if let Some(name) = std::env::var_os("ORKIA_AGENT_NAME") {
        return format!("orkia-agent-{}", name.to_string_lossy());
    }
    "orkia-cage".into()
}

/// Per-agent corpus (read-only, read-write) — the paths each agent silently
/// derived from the program's basename. Kimi and unknown agents get an empty
/// corpus (their config dirs are not granted = workspace-only, fail-closed
/// until a real-agent run establishes what they actually need).
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn corpus_for_agent(program: &str, home: Option<&Path>) -> (Vec<PathBuf>, Vec<PathBuf>) {
    use orkia_shell_types::ProviderId;
    let Some(h) = home else {
        return (Vec::new(), Vec::new());
    };
    let rw = |rels: &[&str]| -> Vec<PathBuf> { rels.iter().map(|r| h.join(r)).collect() };
    match ProviderId::from_command(program) {
        ProviderId::Claude => (
            Vec::new(),
            rw(&[
                ".claude",
                ".claude.json",
                ".mcp.json",
                ".config/claude",
                ".local/state/claude",
                ".local/share/claude",
                ".cache/claude",
            ]),
        ),
        ProviderId::Codex => (Vec::new(), rw(&[".codex", ".cache/codex"])),
        ProviderId::Gemini => (Vec::new(), rw(&[".gemini", ".cache/gemini"])),
        ProviderId::Kimi | ProviderId::Generic => (Vec::new(), Vec::new()),
    }
}

/// Binaries to deny `process-exec`: capabilities whose name is a bare binary
/// (no `.`) with a `Deny` verdict, resolved via `PATH`. (Dotted names like
/// `git.push` are sub-command rules — Linux-only.)
///
/// **Emits BOTH the `$PATH` path and its canonicalized (symlink-resolved)
/// path.** Seatbelt matches `process-exec` on the symlink-resolved path, so a
/// deny on only the symlink (e.g. `/opt/homebrew/bin/docker` →
/// `/Applications/Docker.app/…`) is fail-open — the agent execs the real target
/// unblocked. Denying both closes that hole (fail-closed).
#[cfg(target_os = "macos")]
fn derive_deny_exec(policy: &Policy) -> Vec<PathBuf> {
    use orkia_shell_types::Verdict;
    let mut out: Vec<PathBuf> = Vec::new();
    for cap in &policy.capabilities {
        if cap.verdict != Verdict::Deny || cap.name.contains('.') {
            continue;
        }
        if let Some(p) = which(&cap.name) {
            if let Ok(canon) = std::fs::canonicalize(&p)
                && canon != p
            {
                out.push(canon);
            }
            out.push(p);
        }
    }
    out
}

/// Real uid of the invoking user — for per-uid temp paths (claude's
/// `/tmp/claude-<uid>` Bash-tool staging dir).
#[cfg(target_os = "macos")]
fn current_uid() -> u32 {
    // SAFETY: `getuid()` takes no arguments, never fails, and has no memory
    // effects — it returns the calling process's real user ID.
    unsafe { libc::getuid() }
}

/// Resolve a bare program name to an absolute path via `PATH`.
#[cfg(target_os = "macos")]
fn which(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let p = PathBuf::from(name);
        return p.exists().then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// Absolute, canonicalized workspace root (relative → joined with cwd).
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn resolve_workspace(root: &Path) -> Result<PathBuf> {
    let abs = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()?.join(root)
    };
    Ok(std::fs::canonicalize(&abs).unwrap_or(abs))
}

/// Linux. Keeps TUI vars, locale, the LLM-API key, and the `ORKIA_*` the shell
/// injected (so hooks/SEAL bridge still reach the journal); drops everything
/// else of the host env.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn allowed_env() -> Vec<(String, String)> {
    const EXACT: &[&str] = &[
        "HOME",
        "PATH",
        "USER",
        "LOGNAME",
        "SHELL",
        "TMPDIR",
        "TERM",
        "COLORTERM",
        "TERM_PROGRAM",
        "TERM_PROGRAM_VERSION",
        "LANG",
        "TZ",
        "SSH_AUTH_SOCK",
        "NODE_EXTRA_CA_CERTS",
        "GIT_SSL_CAINFO",
    ];
    const PREFIX: &[&str] = &[
        "ORKIA_",
        "ANTHROPIC_",
        "CLAUDE_",
        "LC_",
        "GEMINI_",
        "OPENAI_",
        "CODEX_",
    ];
    std::env::vars()
        .filter(|(k, _)| EXACT.contains(&k.as_str()) || PREFIX.iter().any(|p| k.starts_with(p)))
        .collect()
}

/// macOS: apply the env allowlist to the `sandbox-exec` command, then layer the
/// advisory shell hint on top.
#[cfg(target_os = "macos")]
fn apply_env_allowlist(cmd: &mut std::process::Command) {
    cmd.env_clear();
    for (k, v) in allowed_env() {
        cmd.env(k, v);
    }
    apply_advisory_shell(cmd);
}

/// Hint the agent's session shell at `orkia-sh` and prepend its dir to
/// `PATH`. Claude's Bash tool calls `/bin/bash` by absolute path and ignores
/// both, so this does not constrain what it runs; it only routes cooperative /
/// bare-name shell-outs through the shim in versions that honor `$SHELL`/`$PATH`.
/// The macOS *guarantee* is the Seatbelt exec-deny; per-command mediation is the
/// PreToolUse hook. No security claim rests on this.
#[cfg(target_os = "macos")]
fn apply_advisory_shell(cmd: &mut std::process::Command) {
    let Some(sh) = orkia_sh_path() else {
        return;
    };
    cmd.env("SHELL", &sh);
    if let Some(dir) = sh.parent() {
        let path = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{}:{path}", dir.display()));
    }
    tracing::info!(
        shim = %sh.display(),
        "orkia-cage: set advisory SHELL/PATH (best-effort only, NOT enforcement)"
    );
}

/// Resolve the `orkia-sh` shim: explicit `ORKIA_SH_BIN`, else a sibling of this
/// binary. `None` if neither is a file. (macOS: advisory shell hint; Linux:
/// the sole-shell install source.)
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn orkia_sh_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("ORKIA_SH_BIN") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let sibling = std::env::current_exe().ok()?.parent()?.join("orkia-sh");
    sibling.is_file().then_some(sibling)
}

/// Split the post-`--` argv into (program, args), failing closed when empty.
fn split_agent(agent: &[String]) -> Result<(&str, &[String])> {
    match agent.split_first() {
        Some((program, rest)) => Ok((program.as_str(), rest)),
        None => {
            bail!("no agent command — expected: orkia-cage --policy <PATH> -- <program> [args...]")
        }
    }
}

/// Load and parse the policy file, failing closed on any error.
fn load_policy(path: &Path) -> Result<Policy> {
    if !path.exists() {
        bail!("policy file not found: {}", path.display());
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading policy file {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("parsing policy file {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_shell_types::PolicyDecision;

    const SAMPLE_TOML: &str = r#"
default_verdict = "ask"

[workspace]
root = "."

[[capabilities]]
name = "git.push"
matches = ["git push*"]
verdict = "deny"
"#;

    #[test]
    fn parses_policy_and_agent() {
        let args = Args::try_parse_from([
            "orkia-cage",
            "--policy",
            "p.toml",
            "--",
            "claude",
            "--mcp-config",
            "x.json",
        ])
        .expect("valid args");
        assert_eq!(args.policy, PathBuf::from("p.toml"));
        let (program, rest) = split_agent(&args.agent).expect("non-empty agent");
        assert_eq!(program, "claude");
        assert_eq!(rest, ["--mcp-config", "x.json"]);
    }

    #[test]
    fn missing_policy_flag_is_error() {
        let parsed = Args::try_parse_from(["orkia-cage", "--", "claude"]);
        assert!(parsed.is_err());
    }

    #[test]
    fn empty_agent_fails_closed() {
        let args = Args::try_parse_from(["orkia-cage", "--policy", "p.toml"]).expect("parses");
        assert!(split_agent(&args.agent).is_err());
    }

    #[test]
    fn loads_valid_policy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("policy.toml");
        std::fs::write(&path, SAMPLE_TOML).unwrap();
        let policy = load_policy(&path).expect("loads");
        assert!(matches!(
            policy.evaluate_match("git push origin"),
            PolicyDecision::Deny {
                capability: Some("git.push"),
                ..
            }
        ));
    }

    #[test]
    fn missing_policy_file_fails_closed() {
        let err = load_policy(Path::new("/definitely/not/here.toml")).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn malformed_policy_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        // `verdict` is not a valid Verdict variant → parse error.
        let bad = "[workspace]\nroot = \".\"\n\n[[capabilities]]\nname = \"x\"\nmatches = [\"y*\"]\nverdict = \"nope\"\n";
        std::fs::write(&path, bad).unwrap();
        assert!(load_policy(&path).is_err());
    }

    #[test]
    fn corpus_is_keyed_per_provider() {
        let home = Path::new("/home/u");
        // Known providers get their config dirs; the basename rule means
        // absolute paths resolve identically.
        let (_, claude) = corpus_for_agent("/usr/local/bin/claude", Some(home));
        assert!(claude.contains(&home.join(".claude")));
        let (_, codex) = corpus_for_agent("codex", Some(home));
        assert_eq!(codex, vec![home.join(".codex"), home.join(".cache/codex")]);
        let (_, gemini) = corpus_for_agent("gemini", Some(home));
        assert!(gemini.contains(&home.join(".gemini")));
        // Kimi and unknown agents: empty corpus = workspace-only, fail-closed.
        assert_eq!(corpus_for_agent("kimi", Some(home)), (vec![], vec![]));
        assert_eq!(
            corpus_for_agent("mystery-cli", Some(home)),
            (vec![], vec![])
        );
        // No home → nothing to grant, for any provider.
        assert_eq!(corpus_for_agent("claude", None), (vec![], vec![]));
    }
}
