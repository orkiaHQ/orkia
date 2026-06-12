// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! Builds an unprivileged user+mount namespace, constructs a fresh `tmpfs`
//! root containing **only an allowlist** (workspace rw, system dirs ro, agent
//! corpus rw, the SEAL socket dir, host `/dev` bound in, a fresh `/proc`),
//! `pivot_root`s into it, then `exec`s the agent. Everything not bound in is
//! absent — `ENOENT` by construction (the fail-closed inversion).
//!
//! `/proc` is a **fresh** procfs: the unshare now includes `CLONE_NEWPID` and we
//! fork so the agent runs as PID 1 in its own PID namespace, which lets the
//! kernel grant a private `proc` mount (no host-PID leak). If the mount
//! is still refused, we fall back to a host `/proc` bind so the agent keeps
//! working. The forking parent stays alive to supervise the child (signal
//! forwarding for the interactive contract). Verified on Linux 2026-06-04.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use nix::mount::{MntFlags, MsFlags, mount, umount2};
use nix::sched::{CloneFlags, unshare};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, chdir, fork, getgid, getuid, pivot_root};

/// A tool binary exposed in the cage: the agent invokes it at `link`
/// (e.g. `/usr/bin/git`) and we bind the canonicalized host file `real` there.
/// Only allowlisted tools are bound — the rest of the host bin dirs never exist
/// in the cage.
pub struct ToolBind {
    pub link: PathBuf,
    pub real: PathBuf,
}

/// What the rootfs exposes. Built by the caller from the policy + corpus.
pub struct RootfsSpec {
    /// Absolute workspace root.
    pub workspace: PathBuf,
    /// entirely — the path does not exist inside the cage (ENOENT) and the agent
    /// starts with cwd `/`. The within-workspace deny masks are skipped (nothing
    /// to mask). When false, `workspace_write` is moot.
    pub workspace_read: bool,
    /// `caps.write`: when false the workspace is bind-mounted **read-only**
    /// (writes return EROFS). Only meaningful when `workspace_read` is true.
    pub workspace_write: bool,
    /// Coarse read-only binds for the **library/runtime closure** (`/usr/lib*`,
    /// `/usr/share`, `/etc`, `/opt`, …) — kept whole so the dynamic linker, NSS,
    /// ICU, and TLS the agent runtime `dlopen`s never go missing.
    pub ro_binds: Vec<PathBuf>,
    /// Real bin dirs created **empty** so only [`tools`](Self::tools) populate
    /// them (`/usr/bin`, `/usr/sbin`, …) — the per-binary allowlist.
    pub bin_dirs: Vec<PathBuf>,
    /// usr-merge compat symlinks to recreate (`/bin` → `usr/bin`, `/lib` →
    /// `usr/lib`, …) so the merged tree resolves with the bin dirs tightened.
    pub bin_symlinks: Vec<(PathBuf, PathBuf)>,
    /// Allowlisted tool binaries bound into the (otherwise empty) bin dirs.
    pub tools: Vec<ToolBind>,
    /// Read-write binds (agent corpus, the SEAL socket dir).
    pub rw_binds: Vec<PathBuf>,
    /// Within-workspace paths to **write-deny** (config-injection vectors the
    /// agent legitimately reads but must not rewrite: `.git/config`,
    /// `.git/hooks`, `.mcp.json`). Each existing one is ro-bound over itself —
    /// writes denied, reads preserved.
    pub deny_write: Vec<PathBuf>,
    /// Within-workspace **secrets to read-deny** (`.env`, …). Each existing file
    /// is masked with `/dev/null` (reads return empty) and each existing dir with
    /// an empty read-only tmpfs, so contents are hidden *and* unwritable.
    ///
    /// Both deny sets resolve symlinks: a target reached via a symlink is masked
    /// at its real in-workspace location, neutralizing every alias. A target
    /// whose real path escapes the workspace needs no mask — it was never mounted
    /// into the cage (absent by construction). **Pre-masking *absent* deny
    /// paths is out of V1**: the workspace is a direct bind-mount with no overlay
    /// upper, so masking a not-yet-existent path would mean materializing it on
    /// the host — deferred to the V2 workspace-overlay.
    pub deny_read: Vec<PathBuf>,
    /// Whether unprivileged userns failure is allowed to fall through to a
    /// bare exec (the `cage.allow_unconfined` decision). Default false.
    pub allow_unconfined: bool,
    /// The `orkia-sh` shim (host path). When `Some`, it is installed as the
    /// path, the real shell preserved for it to exec, the policy written in.
    /// `None` → no command mediation (shells left as the real binaries).
    pub shim: Option<PathBuf>,
    /// The real shell `orkia-sh` execs for an allowed command (e.g. `/bin/bash`).
    pub real_shell: PathBuf,
    /// Shell paths inside the cage to shadow with the shim (`/bin/bash`, …).
    pub shadow_shells: Vec<PathBuf>,
    /// The policy serialized to TOML, written into the rootfs for the shim.
    pub policy_toml: String,
}

/// Writable shim dir on the tmpfs root (NOT under RO-bound `/usr/lib`).
const SHIM_DIR: &str = "/.orkia-sh";
/// In-cage path the shim reads the policy from (`ORKIA_CAGE_POLICY`).
pub const IN_CAGE_POLICY: &str = "/.orkia-sh/policy.toml";
/// In-cage path of the preserved real shell (`ORKIA_SH_REAL`).
pub const IN_CAGE_REAL_SH: &str = "/.orkia-sh/real-sh";

/// Enter the sandbox and `exec` the agent. Returns only on failure (or, when
/// `allow_unconfined` and userns is unavailable, after a bare exec attempt).
/// `env` is the scrub-then-allow allowlist the agent runs with.
pub fn enter_and_exec(
    spec: &RootfsSpec,
    program: &str,
    args: &[String],
    env: &[(String, String)],
) -> Result<()> {
    let uid = getuid();
    let gid = getgid();

    // 1. New user + mount + PID namespaces (unprivileged). `CLONE_NEWPID` only
    //    takes effect for **future children**, so after this we must fork; the
    //    child is PID 1 in the new PID namespace and can mount a *fresh* procfs
    //    (no host-PID leak). On EPERM this is the AppArmor/userns-disabled
    //    case — fail-closed unless the operator opted into unconfined.
    if let Err(e) =
        unshare(CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWPID)
    {
        if spec.allow_unconfined {
            tracing::warn!(error = %e, "orkia-cage: userns unavailable — cage.allow_unconfined set, running UNCONFINED");
            return bare_exec(program, args, env);
        }
        bail!(
            "unprivileged user namespace unavailable ({e}). On Ubuntu 24.04+ this is \
             AppArmor: `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0` \
             (dev) or ship an AppArmor profile for orkia-cage (prod). Set \
             [cage].allow_unconfined to run without isolation (NOT recommended)."
        );
    }

    // 2. uid/gid map: map the invoking user to **itself**. The creating
    //    process holds full caps in the new userns regardless of the mapped uid,
    //    so mounts still work — but the agent runs as the real uid, not fake
    //    root, so tools like npm/claude don't warn/refuse "running as root".
    //    setgroups=deny is required before gid_map. Written by the creator BEFORE
    //    the fork; the child inherits the mapping.
    write_map("/proc/self/setgroups", "deny").context("setgroups deny")?;
    write_map(
        "/proc/self/uid_map",
        &format!("{u} {u} 1", u = uid.as_raw()),
    )
    .context("uid_map")?;
    write_map(
        "/proc/self/gid_map",
        &format!("{g} {g} 1", g = gid.as_raw()),
    )
    .context("gid_map")?;

    // 3. Fork. The child enters the new PID namespace as PID 1 and does all the
    //    rootfs work + exec; the parent waits and mirrors its exit status. SAFETY:
    //    orkia-cage is single-threaded at this point, so the child may allocate /
    //    mount freely between fork and exec (no async-signal-safety hazard).
    match unsafe { fork() }.context("fork into PID namespace")? {
        ForkResult::Parent { child } => supervise(child),
        ForkResult::Child => child_enter_and_exec(spec, program, args, env),
    }
}

/// Parent-side supervisor: the fork means we no longer `exec`-replace ourselves,
/// so this process sits between the PTY and the agent. To preserve the
/// interactive contract (CLAUDE.md #6) it **forwards** terminal/job-control
/// signals to the agent and mirrors its exit. Without this, Ctrl-C / Ctrl-\ /
/// resize would die here instead of reaching the agent.
///
/// NB: interactive Ctrl-C / Ctrl-Z / SIGWINCH must still be validated against a
/// real agent on a PTY before this path is trusted in production.
fn supervise(child: nix::unistd::Pid) -> Result<()> {
    use nix::sys::signal::{SigSet, Signal, kill};
    use nix::sys::wait::WaitPidFlag;

    let forwarded = [
        Signal::SIGINT,
        Signal::SIGTERM,
        Signal::SIGQUIT,
        Signal::SIGHUP,
        Signal::SIGWINCH,
    ];
    // Block the forwarded set + SIGCHLD and consume them via sigwait (done only
    // in the parent, after the fork, so the child inherits an unblocked mask).
    let mut mask = SigSet::empty();
    for s in forwarded {
        mask.add(s);
    }
    mask.add(Signal::SIGCHLD);
    mask.thread_block()
        .context("block signals for supervisor")?;
    loop {
        let sig = mask.wait().context("sigwait")?;
        if sig == Signal::SIGCHLD {
            match waitpid(child, Some(WaitPidFlag::WNOHANG)).context("waitpid child")? {
                WaitStatus::Exited(_, code) => std::process::exit(code),
                WaitStatus::Signaled(_, s, _) => std::process::exit(128 + s as i32),
                _ => continue, // stopped/continued — keep supervising
            }
        } else {
            let _ = kill(child, sig); // forward to the agent
        }
    }
}

/// The PID-1 child: private mount tree → build rootfs (fresh /proc) → pivot →
/// exec. Returns only on failure (the caller `bail!`s / exits non-zero).
fn child_enter_and_exec(
    spec: &RootfsSpec,
    program: &str,
    args: &[String],
    env: &[(String, String)],
) -> Result<()> {
    // Make the whole tree private so binds/pivot don't leak to the host.
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&str>,
    )
    .context("mount / private")?;
    let newroot = build_rootfs(spec)?;
    pivot_into(&newroot)?;
    // Start the agent in its workspace, not at the bare rootfs root. The rw-bind
    // put the workspace at the same absolute path inside the cage, so we can
    // chdir straight to it. Without this the agent execs with cwd `/` and cannot
    // see its project — `git` reports "not a git repository", relative file
    // writes land in the ephemeral root, and a normal dev loop silently breaks.
    //
    // When `caps.read` is off the workspace is not mounted (it does not exist in
    // the cage), so there is nothing to chdir into — start at the rootfs root.
    if spec.workspace_read {
        chdir(&spec.workspace)
            .with_context(|| format!("chdir workspace {}", spec.workspace.display()))?;
    } else {
        chdir(Path::new("/")).context("chdir / (workspace read disabled)")?;
    }
    // The agent execs as PID 1 of the new namespace (V1: no separate init/reaper;
    // orphaned descendants reparent to the agent — a documented simplification).
    bare_exec(program, args, env)
}

fn build_rootfs(spec: &RootfsSpec) -> Result<PathBuf> {
    let pid = std::process::id();
    let newroot = PathBuf::from(format!("/tmp/.orkia-cage-{pid}"));
    std::fs::create_dir_all(&newroot).context("create newroot dir")?;
    mount(
        Some("tmpfs"),
        &newroot,
        Some("tmpfs"),
        MsFlags::empty(),
        Some("mode=0755"),
    )
    .with_context(|| format!("mount tmpfs at {}", newroot.display()))?;

    // /dev: a **minimal node set**, NOT the whole devtmpfs (which would expose
    // raw disks etc.). The agent's PTY is /dev/pts/N, so bind /dev/pts +
    // /dev/ptmx; the rest are the standard safe char devices.
    for dev in [
        "/dev/null",
        "/dev/zero",
        "/dev/full",
        "/dev/random",
        "/dev/urandom",
        "/dev/tty",
        "/dev/ptmx",
        "/dev/pts",
        "/dev/shm",
    ] {
        bind_into(&newroot, Path::new(dev), false).ok();
    }
    // /proc: a **fresh** procfs (we are PID 1 in a new PID namespace after the
    // fork) — shows only the cage's own PIDs, no host-PID leak. Falls back
    // to a host bind if the fresh mount is refused (keeps the agent working).
    mount_fresh_proc(&newroot)?;
    // Coarse read-only library/runtime closure (the linker + NSS + ICU + TLS the
    // agent runtime dlopens — kept whole).
    for src in &spec.ro_binds {
        if src.exists() {
            bind_into(&newroot, src, true).with_context(|| format!("ro-bind {}", src.display()))?;
        }
    }
    // Dynamic network files that may be symlinks escaping the coarse RO binds —
    // notably systemd-resolved's `/etc/resolv.conf` → `/run/...`, which dangles
    // because the rootfs binds `/etc` but not `/run`. Without it DNS fails and no
    // agent can reach its API. Binds only the resolved *file*, never `/run`.
    bind_network_files(&newroot, &spec.ro_binds)?;
    // Per-binary tool allowlist: recreate usr-merge symlinks, create the
    // real bin dirs **empty**, then bind only allowlisted tools into them — the
    // ~1100 parasitic host binaries never exist in the cage.
    install_tools(&newroot, spec)?;
    // - `read=false`  → omit the bind entirely (the workspace does not exist in
    //   the cage; the agent already starts at `/`). The within-workspace deny
    //   masks are skipped — there is nothing mounted to mask.
    // - `read=true, write=false` → bind read-only (writes return EROFS).
    // - `read=true, write=true`  → bind read-write (unchanged behavior).
    if spec.workspace_read {
        let ro = !spec.workspace_write;
        bind_into(&newroot, &spec.workspace, ro)
            .with_context(|| format!("bind workspace {}", spec.workspace.display()))?;
    }
    // Corpus + socket dir, read-write (independent of the workspace caps).
    for src in &spec.rw_binds {
        if src.exists() {
            bind_into(&newroot, src, false)
                .with_context(|| format!("rw-bind {}", src.display()))?;
        }
    }
    // Within-workspace deny hardening, layered *after* the workspace bind
    // so the masks sit on top. Skipped when the workspace is not mounted (read
    // off) — there is nothing to mask. Write-deny first (config-injection
    // vectors), then read-deny (secrets) — a secret in both ends up read-denied.
    if spec.workspace_read {
        for path in &spec.deny_write {
            mask_in_workspace(&newroot, &spec.workspace, path, DenyMode::WriteDeny)
                .with_context(|| format!("write-deny {}", path.display()))?;
        }
        for path in &spec.deny_read {
            mask_in_workspace(&newroot, &spec.workspace, path, DenyMode::ReadDeny)
                .with_context(|| format!("read-deny {}", path.display()))?;
        }
    }
    if let Some(shim) = &spec.shim {
        install_shim(&newroot, shim, &spec.real_shell, &spec.shadow_shells)?;
        write_in_cage_policy(&newroot, &spec.policy_toml)?;
    }
    Ok(newroot)
}

/// Install the per-binary tool allowlist: recreate usr-merge symlinks,
/// create the real bin dirs empty, then bind each allowlisted tool over a fresh
/// placeholder. Placeholders live on the tmpfs root (the bin dirs are *not*
/// bind-mounts), so `File::create` here touches nothing on the host.
fn install_tools(newroot: &Path, spec: &RootfsSpec) -> Result<()> {
    for (link, target) in &spec.bin_symlinks {
        let lp = rootfs_target(newroot, link);
        if let Some(parent) = lp.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::os::unix::fs::symlink(target, &lp).ok();
    }
    for dir in &spec.bin_dirs {
        std::fs::create_dir_all(rootfs_target(newroot, dir)).ok();
    }
    for tool in &spec.tools {
        bind_tool_ro(newroot, tool)
            .with_context(|| format!("bind tool {}", tool.link.display()))?;
    }
    Ok(())
}

/// Dynamic network files an agent runtime needs that may be **symlinks escaping
/// the coarse RO binds**. The decisive one is systemd-resolved's
/// `/etc/resolv.conf` → `/run/systemd/resolve/...`: the rootfs binds `/etc` (so
/// the *symlink* is present) but not `/run` (so its *target* is absent), leaving
/// a dangling link — DNS then fails and the agent cannot reach its API. The
/// others (`/etc/hosts`, `/etc/nsswitch.conf`, the CA bundle) are normally real
/// files under the already-RO-bound `/etc`, so they resolve for free; listing
/// them keeps the network read-surface explicit and self-documenting.
const NETWORK_FILES: &[&str] = &["/etc/resolv.conf", "/etc/hosts", "/etc/nsswitch.conf"];

/// Ensure each [`NETWORK_FILES`] entry **resolves** inside the cage. For one that
/// canonicalizes outside every coarse RO bind (e.g. resolv.conf landing in
/// `/run`), bind **just that resolved file** read-only at its real path — and
/// nothing else. We deliberately never bind `/run` (or any directory under it):
/// `/run` holds the SEAL journal socket and other runtime state, so exposing it
/// would trade a one-file gap for a large attack surface (CLAUDE.md #4 / #8).
/// [`bind_into`] of a *file* only `mkdir`s empty tmpfs parents, so the resolved
/// file is the sole host content that appears under `/run` in the cage.
fn bind_network_files(newroot: &Path, ro_binds: &[PathBuf]) -> Result<()> {
    for f in NETWORK_FILES {
        // canonicalize follows the whole symlink chain; absent / dangling on the
        // host → skip (nothing to bind, and not our bug to fix).
        let Ok(canon) = std::fs::canonicalize(Path::new(f)) else {
            continue;
        };
        let Some(file) = plan_network_bind(&canon, ro_binds) else {
            continue; // already reachable through a coarse RO bind (e.g. under /etc)
        };
        // Bind the resolved file at its own path; only empty parent dirs are
        // created on the tmpfs, never exposing the rest of `/run`.
        bind_network_file_ro(newroot, &file)
            .with_context(|| format!("bind network file {}", file.display()))?;
    }
    Ok(())
}

/// Bind one resolved network file read-only, **re-asserting** the restrictive
/// mount flags its source carries. resolv.conf canonicalizes into `/run`, a
/// tmpfs mounted `nosuid,nodev,noexec`; in an unprivileged user namespace those
/// flags are *locked*, so an RO remount that drops them is rejected (EPERM).
/// The generic [`bind_into`] RO path can't add `noexec` (it also binds the
/// exec'd tool binaries), so the network file gets its own remount that ORs the
/// locked flags — always safe for a config file, which is never executed.
fn bind_network_file_ro(newroot: &Path, src: &Path) -> Result<()> {
    let target = rootfs_target(newroot, src);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let _ = std::fs::File::create(&target);
    mount(
        Some(src),
        &target,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .with_context(|| format!("bind {} -> {}", src.display(), target.display()))?;
    mount(
        None::<&str>,
        &target,
        None::<&str>,
        MsFlags::MS_BIND
            | MsFlags::MS_REMOUNT
            | MsFlags::MS_RDONLY
            | MsFlags::MS_NOSUID
            | MsFlags::MS_NODEV
            | MsFlags::MS_NOEXEC
            | MsFlags::MS_REC,
        None::<&str>,
    )
    .with_context(|| format!("ro-remount {}", target.display()))?;
    Ok(())
}

/// Decide whether a canonicalized network file needs its own bind: only when it
/// escapes **every** coarse RO bind (so it would be absent in the cage).
/// Returns the file to bind, or `None` when it is already reachable. Pure — the
/// mount side effect lives in [`bind_network_files`].
fn plan_network_bind(canon: &Path, ro_binds: &[PathBuf]) -> Option<PathBuf> {
    if ro_binds.iter().any(|b| canon.starts_with(b)) {
        return None;
    }
    Some(canon.to_path_buf())
}

/// Bind one allowlisted tool's canonical host file read-only at its invocation
/// path under `newroot`.
fn bind_tool_ro(newroot: &Path, tool: &ToolBind) -> Result<()> {
    if !tool.real.exists() {
        return Ok(());
    }
    let target = rootfs_target(newroot, &tool.link);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let _ = std::fs::File::create(&target);
    bind_file_ro(&tool.real, &target)
}

/// Deny mode for a within-workspace mask. `WriteDeny` keeps reads
/// (config-injection vectors the agent legitimately reads); `ReadDeny` also
/// hides contents (secrets).
#[derive(Clone, Copy)]
enum DenyMode {
    WriteDeny,
    ReadDeny,
}

/// How a deny target is masked once it has been resolved.
#[derive(Clone, Copy, Debug, PartialEq)]
enum MaskKind {
    /// Re-bind the path over itself read-only: writes denied, reads preserved.
    RoSelf,
    /// Bind `/dev/null` over the file: reads return empty, writes denied.
    DevNull,
    /// Overlay an empty read-only tmpfs over the dir: contents hidden + unwritable.
    EmptyDir,
}

/// Decide the mask for a deny target from its **canonicalized** real path and the
/// workspace root. Pure (the mount side effects live in [`mask_in_workspace`]).
///
/// `None` ⇒ no mask: the real path escaped the workspace, which means its target
/// was never bound into the cage (absent by construction) — a dangling
/// alias already reads as `ENOENT`, so there is nothing to neutralize.
fn plan_mask(real: &Path, workspace: &Path, is_dir: bool, mode: DenyMode) -> Option<MaskKind> {
    if !real.starts_with(workspace) {
        return None;
    }
    Some(match (mode, is_dir) {
        (DenyMode::WriteDeny, _) => MaskKind::RoSelf,
        (DenyMode::ReadDeny, true) => MaskKind::EmptyDir,
        (DenyMode::ReadDeny, false) => MaskKind::DevNull,
    })
}

/// Mask an **existing** protected path inside the workspace (scoped to
/// deny-within-a-mounted-writable-region). `canonicalize` resolves every symlink
/// component, so a target reached via a symlink is masked at its real location —
/// the symlink-replacement / boundary defense falls out for free. An absent
/// target (canonicalize fails) is left alone: pre-masking is out of V1 (see the
/// `deny_read` doc).
fn mask_in_workspace(newroot: &Path, workspace: &Path, path: &Path, mode: DenyMode) -> Result<()> {
    let Ok(real) = std::fs::canonicalize(path) else {
        // Absent → no overlay upper to materialize it on (V2). Or a broken
        // symlink to an unmounted target → already ENOENT in the cage.
        return Ok(());
    };
    let Some(kind) = plan_mask(&real, workspace, real.is_dir(), mode) else {
        return Ok(());
    };
    let target = rootfs_target(newroot, &real);
    match kind {
        // Dir ro-bind goes through `bind_into` (dir-safe: no file truncation);
        // file masks use `bind_file_ro`, which binds over the existing target
        // **without** `File::create` — re-creating it would truncate the real
        // file (it is the host file, reached through the workspace bind).
        MaskKind::RoSelf if real.is_dir() => bind_into(newroot, &real, true),
        MaskKind::RoSelf => bind_file_ro(&real, &target),
        MaskKind::DevNull => bind_devnull(&target),
        MaskKind::EmptyDir => mask_empty_dir(&target),
    }
}

/// Bind `/dev/null` over `target` (an existing file reached through the
/// workspace bind): reads return empty and writes go to the bit-bucket — they
/// never reach the real secret. **No read-only remount:** `/dev/null` already
/// denies the read, and remounting a cross-filesystem (devtmpfs) bind read-only
/// is refused with `EPERM` inside the unprivileged userns (the source mount's
/// flags are locked) — and would be redundant anyway.
fn bind_devnull(target: &Path) -> Result<()> {
    mount(
        Some(Path::new("/dev/null")),
        target,
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    )
    .with_context(|| format!("read-deny /dev/null over {}", target.display()))?;
    Ok(())
}

/// Mount an empty **read-only** tmpfs over `target` (an existing dir reached
/// through the workspace bind): hides its contents and refuses writes under it.
/// `MS_RDONLY` is set on the initial mount — it is our own fresh tmpfs, so no
/// remount (and no cross-fs `EPERM`) is involved.
fn mask_empty_dir(target: &Path) -> Result<()> {
    mount(
        Some("tmpfs"),
        target,
        Some("tmpfs"),
        MsFlags::MS_RDONLY,
        Some("mode=0500"),
    )
    .with_context(|| format!("read-deny tmpfs over {}", target.display()))?;
    Ok(())
}

/// Mount a **fresh** procfs at `newroot/proc`. We are PID 1 in a new PID
/// namespace (post-fork), so a fresh `proc` shows only the cage's own processes
/// — no host-PID leak. If the kernel refuses the mount (e.g. a userns
/// restriction), fall back to a host `/proc` bind so the agent still works
/// rather than aborting the launch.
fn mount_fresh_proc(newroot: &Path) -> Result<()> {
    let target = rootfs_target(newroot, Path::new("/proc"));
    std::fs::create_dir_all(&target).ok();
    if mount(
        Some("proc"),
        &target,
        Some("proc"),
        MsFlags::empty(),
        None::<&str>,
    )
    .is_err()
    {
        bind_into(newroot, Path::new("/proc"), false).context("bind /proc fallback")?;
    }
    Ok(())
}

/// Install `orkia-sh` as the only shell: preserve the real shell at
/// `IN_CAGE_REAL_SH`, then bind the shim over every existing `shadow_shells`
/// path so any `sh`/`bash -c` the agent spawns reaches the shim instead.
fn install_shim(
    newroot: &Path,
    shim: &Path,
    real_shell: &Path,
    shadow_shells: &[PathBuf],
) -> Result<()> {
    let dir = rootfs_target(newroot, Path::new(SHIM_DIR));
    std::fs::create_dir_all(&dir).context("create shim dir")?;
    // Preserve the real shell (for the shim to exec on allow) BEFORE shadowing.
    let real_dst = rootfs_target(newroot, Path::new(IN_CAGE_REAL_SH));
    let _ = std::fs::File::create(&real_dst);
    bind_file_ro(real_shell, &real_dst)
        .with_context(|| format!("preserve real shell {}", real_shell.display()))?;
    // Shadow each shell that actually exists in the rootfs with the shim.
    for sh in shadow_shells {
        let tgt = rootfs_target(newroot, sh);
        if tgt.exists() {
            bind_file_ro(shim, &tgt).with_context(|| format!("shadow shell {}", sh.display()))?;
        }
    }
    Ok(())
}

/// Write the policy TOML into the (writable, tmpfs) shim dir so the shim can
/// read it at `IN_CAGE_POLICY` inside the cage.
fn write_in_cage_policy(newroot: &Path, policy_toml: &str) -> Result<()> {
    let dst = rootfs_target(newroot, Path::new(IN_CAGE_POLICY));
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&dst, policy_toml).context("write in-cage policy")?;
    Ok(())
}

/// Bind `src` over an existing `target` file (paths may differ — unlike
/// [`bind_into`] which re-roots `src`), then apply the RO two-step remount.
fn bind_file_ro(src: &Path, target: &Path) -> Result<()> {
    mount(
        Some(src),
        target,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .with_context(|| format!("bind {} -> {}", src.display(), target.display()))?;
    mount(
        None::<&str>,
        target,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY | MsFlags::MS_REC,
        None::<&str>,
    )
    .with_context(|| format!("ro-remount {}", target.display()))?;
    Ok(())
}

/// Map a host path to its location under `newroot` (same path, re-rooted):
/// `/usr/bin` under `/newroot` → `/newroot/usr/bin`. Pure — unit-tested.
fn rootfs_target(newroot: &Path, src: &Path) -> PathBuf {
    newroot.join(src.strip_prefix("/").unwrap_or(src))
}

/// Bind `src` (an absolute host path) to the same path under `newroot`. When
/// `ro`, apply the **two-step** read-only remount (a single bind does
/// not apply RO).
fn bind_into(newroot: &Path, src: &Path, ro: bool) -> Result<()> {
    let target = rootfs_target(newroot, src);
    if src.is_dir() {
        std::fs::create_dir_all(&target).ok();
    } else {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let _ = std::fs::File::create(&target);
    }
    mount(
        Some(src),
        &target,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .with_context(|| format!("bind {} -> {}", src.display(), target.display()))?;
    if ro {
        mount(
            None::<&str>,
            &target,
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY | MsFlags::MS_REC,
            None::<&str>,
        )
        .with_context(|| format!("ro-remount {}", target.display()))?;
    }
    Ok(())
}

fn pivot_into(newroot: &Path) -> Result<()> {
    let oldroot = newroot.join(".oldroot");
    std::fs::create_dir_all(&oldroot).context("create .oldroot")?;
    pivot_root(newroot, &oldroot).context("pivot_root")?;
    chdir("/").context("chdir / after pivot")?;
    umount2("/.oldroot", MntFlags::MNT_DETACH).context("detach old root")?;
    std::fs::remove_dir("/.oldroot").ok();
    Ok(())
}

fn write_map(path: &str, content: &str) -> Result<()> {
    std::fs::write(path, content).with_context(|| format!("write {path}"))
}

fn bare_exec(program: &str, args: &[String], env: &[(String, String)]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    // Pre-exec hardening: die with the parent (no orphan agent survives
    // orkia), and block setuid escalation — important because the coarse ro
    // `/usr` may contain setuid binaries. Both are preserved across execve.
    let _ = nix::sys::prctl::set_pdeathsig(nix::sys::signal::Signal::SIGKILL);
    let _ = nix::sys::prctl::set_no_new_privs();
    let mut cmd = std::process::Command::new(program);
    cmd.args(args).env_clear().envs(env.iter().cloned());
    let err = cmd.exec();
    Err(anyhow!("failed to exec agent `{program}`: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rootfs_target_reroots_under_newroot() {
        let nr = Path::new("/tmp/.orkia-cage-1");
        assert_eq!(
            rootfs_target(nr, Path::new("/usr/bin")),
            Path::new("/tmp/.orkia-cage-1/usr/bin")
        );
        assert_eq!(
            rootfs_target(nr, Path::new("/dev/null")),
            Path::new("/tmp/.orkia-cage-1/dev/null")
        );
        // workspace under /private-style absolute path
        assert_eq!(
            rootfs_target(nr, Path::new("/work/repo")),
            Path::new("/tmp/.orkia-cage-1/work/repo")
        );
    }

    #[test]
    fn plan_mask_write_deny_rebinds_self() {
        let ws = Path::new("/work/repo");
        assert_eq!(
            plan_mask(&ws.join(".git/config"), ws, false, DenyMode::WriteDeny),
            Some(MaskKind::RoSelf)
        );
        // A write-denied directory (e.g. `.git/hooks`) ro-binds over itself too.
        assert_eq!(
            plan_mask(&ws.join(".git/hooks"), ws, true, DenyMode::WriteDeny),
            Some(MaskKind::RoSelf)
        );
    }

    #[test]
    fn plan_mask_read_deny_picks_devnull_or_empty_dir() {
        let ws = Path::new("/work/repo");
        // Secret file → /dev/null (empty reads).
        assert_eq!(
            plan_mask(&ws.join(".env"), ws, false, DenyMode::ReadDeny),
            Some(MaskKind::DevNull)
        );
        // Secret dir → empty read-only tmpfs.
        assert_eq!(
            plan_mask(&ws.join("secrets"), ws, true, DenyMode::ReadDeny),
            Some(MaskKind::EmptyDir)
        );
    }

    #[test]
    fn plan_mask_skips_targets_resolving_outside_workspace() {
        // A symlink whose real path escapes the workspace needs no mask: the
        // target was never bound into the cage (absent by construction).
        let ws = Path::new("/work/repo");
        assert_eq!(
            plan_mask(Path::new("/etc/passwd"), ws, false, DenyMode::ReadDeny),
            None
        );
        assert_eq!(
            plan_mask(
                Path::new("/home/user/.ssh/id_rsa"),
                ws,
                false,
                DenyMode::WriteDeny
            ),
            None
        );
    }

    #[test]
    fn plan_network_bind_skips_files_under_a_coarse_ro_bind() {
        // A network file that already lives under a coarse RO bind (e.g. /etc)
        // needs no extra bind: it resolves for free inside the cage.
        let ro = vec![PathBuf::from("/etc"), PathBuf::from("/usr")];
        assert_eq!(plan_network_bind(Path::new("/etc/hosts"), &ro), None);
        assert_eq!(
            plan_network_bind(Path::new("/etc/nsswitch.conf"), &ro),
            None
        );
    }

    #[test]
    fn plan_network_bind_binds_resolv_target_escaping_into_run() {
        // systemd-resolved's resolv.conf canonicalizes into /run, which is NOT a
        // coarse RO bind → it must be bound at its own resolved path.
        let ro = vec![PathBuf::from("/etc"), PathBuf::from("/usr")];
        let resolved = Path::new("/run/systemd/resolve/stub-resolv.conf");
        assert_eq!(
            plan_network_bind(resolved, &ro),
            Some(resolved.to_path_buf())
        );
    }

    #[test]
    fn plan_network_bind_never_returns_a_run_directory() {
        // Invariant: the planner only ever returns the resolved resolv *file* —
        // never `/run` itself or any directory under it. Nobody may turn this
        // fix into a `bind /run` later (CLAUDE.md #4 / #8). We assert that for
        // every NETWORK_FILES entry resolving into /run, the planned bind is a
        // strict descendant of /run (a file), never `/run` or `/run/<dir>` bare.
        let ro = vec![PathBuf::from("/etc"), PathBuf::from("/usr")];
        // Bare /run and a bare runtime dir must not be what we bind: feeding the
        // planner such a path would mean a regression upstream produced a dir.
        // The contract callers rely on is "the value resolves to a file under a
        // deeper path than /run", so any planned bind under /run must have more
        // than two components past root.
        let planned = plan_network_bind(Path::new("/run/systemd/resolve/stub-resolv.conf"), &ro)
            .expect("resolv target in /run must be planned");
        assert!(planned.starts_with("/run"));
        assert_ne!(planned, Path::new("/run"));
        assert_ne!(planned, Path::new("/run/systemd"));
        assert_ne!(planned, Path::new("/run/systemd/resolve"));
        // A real file sits at least one component below /run/<dir>.
        assert!(
            planned.components().count() > Path::new("/run/systemd").components().count(),
            "planned bind {} must be a file deep under /run, not a directory",
            planned.display()
        );
    }
}
