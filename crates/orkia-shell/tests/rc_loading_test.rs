// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Tests for the `~/.bashrc` + `~/.profile` + `~/.orkiarc` load chain.
//!
//! `clippy::await_holding_lock` is allowed here intentionally: each test
//! uses the default `current_thread` tokio runtime, so the `.await` does
//! not move work to another thread; holding the std `Mutex` for the
//! lifetime of the test is the simplest way to serialise `$HOME`
//! mutations across parallel tests.
#![allow(clippy::await_holding_lock)]

//!
//! Each test overrides `$HOME` to a tempdir, writes the RC files it
//! cares about, builds a `ShellEngine` with explicit options, and
//! inspects the resulting brush state via `execute` (output captured to
//! a tempfile) and `exported_env`.
//!
//! These tests share the process env (`HOME`), so they must not run in
//! parallel against the same fixture — `cargo test` defaults to thread
//! parallelism per binary. We synchronise via a per-binary mutex.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Mutex;

use brush_core::openfiles::{OpenFile, OpenFiles};
use orkia_shell::engine::{ShellEngine, ShellEngineOptions};
use tempfile::{NamedTempFile, TempDir};

/// `$HOME` overrides are process-global. Serialise tests in this file.
/// Recover from poison so a single failing test doesn't cascade.
static HOME_LOCK: Mutex<()> = Mutex::new(());

fn lock_home() -> std::sync::MutexGuard<'static, ()> {
    match HOME_LOCK.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Returns `(engine, stdout_file, home_dir_to_keep_alive, _home_guard)`.
/// Tests must keep all four around for the duration of their assertions.
struct Fixture {
    engine: ShellEngine,
    out: NamedTempFile,
    _home: TempDir,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl Fixture {
    async fn build(home: TempDir, opts: ShellEngineOptions) -> Self {
        let guard = lock_home();
        // SAFETY: serialised by HOME_LOCK; brush reads $HOME during
        // source_default_rc, so the override must be live by then.
        unsafe {
            std::env::set_var("HOME", home.path());
        }

        let mut engine = ShellEngine::new_with_options(opts)
            .await
            .expect("engine new");

        let out = NamedTempFile::new().expect("tempfile");
        let stdout = File::options()
            .append(true)
            .open(out.path())
            .expect("open stdout");
        let stderr = File::options()
            .append(true)
            .open(out.path())
            .expect("open stderr");
        let files = engine.shell_mut().open_files_mut();
        files.set_fd(OpenFiles::STDOUT_FD, OpenFile::File(stdout));
        files.set_fd(OpenFiles::STDERR_FD, OpenFile::File(stderr));

        // Source RC *after* installing the output sinks so banner /
        // diagnostic output lands in the tempfile, not on test stdout.
        let warnings = engine.source_default_rc(opts).await;
        if !warnings.is_empty() {
            // Print and fail this test — but do NOT panic across the
            // mutex so other tests can still acquire `HOME_LOCK`.
            for (p, e) in &warnings {
                eprintln!("rc warning: {}: {e}", p.display());
            }
            panic!("unexpected RC warnings: {warnings:?}");
        }

        Self {
            engine,
            out,
            _home: home,
            _guard: guard,
        }
    }

    fn read_output(&self) -> String {
        let mut buf = String::new();
        let mut f = self.out.reopen().expect("reopen");
        f.seek(SeekFrom::Start(0)).expect("seek");
        f.read_to_string(&mut buf).expect("read");
        buf
    }
}

fn write_rc(home: &TempDir, name: &str, body: &str) {
    let mut f = File::create(home.path().join(name)).expect("create rc");
    f.write_all(body.as_bytes()).expect("write rc");
}

#[tokio::test]
async fn bashrc_export_visible_after_load() {
    let home = TempDir::new().expect("tmp home");
    write_rc(&home, ".bashrc", "export ORKIA_FROM_BASHRC=yes\n");

    let opts = ShellEngineOptions {
        load_bashrc: true,
        load_profile: false,
        login: false,
    };
    let mut fx = Fixture::build(home, opts).await;
    fx.engine
        .execute("echo $ORKIA_FROM_BASHRC")
        .await
        .expect("exec");
    assert_eq!(fx.read_output().trim(), "yes");

    // Also visible via exported_env (the agent-env propagation path).
    let env = fx.engine.exported_env();
    assert!(
        env.iter()
            .any(|(k, v)| k == "ORKIA_FROM_BASHRC" && v == "yes"),
        "expected ORKIA_FROM_BASHRC=yes in exported_env, got {env:?}",
    );
}

#[tokio::test]
async fn orkiarc_overrides_bashrc() {
    let home = TempDir::new().expect("tmp home");
    write_rc(
        &home,
        ".bashrc",
        "shopt -s expand_aliases\nalias gs='echo from-bashrc'\n",
    );
    write_rc(
        &home,
        ".orkiarc",
        "shopt -s expand_aliases\nalias gs='echo from-orkiarc'\n",
    );

    let opts = ShellEngineOptions {
        load_bashrc: true,
        load_profile: false,
        login: false,
    };
    let mut fx = Fixture::build(home, opts).await;

    // .orkiarc is sourced *after* .bashrc by the production path.
    // Mirror that in the test by sourcing it ourselves now.
    let orkiarc = fx._home.path().join(".orkiarc");
    fx.engine
        .source_if_exists(&orkiarc)
        .await
        .expect("source orkiarc");

    fx.engine.execute("gs").await.expect("exec");
    assert_eq!(fx.read_output().trim(), "from-orkiarc");
}

#[tokio::test]
async fn load_bashrc_false_skips_it() {
    let home = TempDir::new().expect("tmp home");
    write_rc(&home, ".bashrc", "export ORKIA_SHOULD_NOT_BE_SET=oops\n");

    let opts = ShellEngineOptions {
        load_bashrc: false,
        load_profile: false,
        login: false,
    };
    let mut fx = Fixture::build(home, opts).await;
    fx.engine
        .execute("echo \"[${ORKIA_SHOULD_NOT_BE_SET:-empty}]\"")
        .await
        .expect("exec");
    assert_eq!(fx.read_output().trim(), "[empty]");
}

#[tokio::test]
async fn missing_bashrc_is_silent() {
    let home = TempDir::new().expect("tmp home");
    // No .bashrc on disk.
    let opts = ShellEngineOptions {
        load_bashrc: true,
        load_profile: false,
        login: false,
    };
    // Fixture::build asserts warnings.is_empty() — no missing-file warning.
    let _fx = Fixture::build(home, opts).await;
}

#[tokio::test]
async fn login_shell_sources_profile_chain() {
    let home = TempDir::new().expect("tmp home");
    // Bash convention: first of .bash_profile / .bash_login / .profile that
    // exists is sourced. Write only .profile to prove the fallback works.
    write_rc(&home, ".profile", "export ORKIA_FROM_PROFILE=yes\n");

    let opts = ShellEngineOptions {
        load_bashrc: false,
        load_profile: true,
        login: true,
    };
    let mut fx = Fixture::build(home, opts).await;
    fx.engine
        .execute("echo $ORKIA_FROM_PROFILE")
        .await
        .expect("exec");
    assert_eq!(fx.read_output().trim(), "yes");
}

#[tokio::test]
async fn login_shell_prefers_bash_profile() {
    let home = TempDir::new().expect("tmp home");
    write_rc(
        &home,
        ".bash_profile",
        "export ORKIA_RC_SOURCE=bash_profile\n",
    );
    write_rc(&home, ".profile", "export ORKIA_RC_SOURCE=profile\n");

    let opts = ShellEngineOptions {
        load_bashrc: false,
        load_profile: true,
        login: true,
    };
    let mut fx = Fixture::build(home, opts).await;
    fx.engine
        .execute("echo $ORKIA_RC_SOURCE")
        .await
        .expect("exec");
    // .bash_profile wins over .profile per bash convention.
    assert_eq!(fx.read_output().trim(), "bash_profile");
}

#[tokio::test]
async fn non_login_does_not_source_profile() {
    let home = TempDir::new().expect("tmp home");
    write_rc(&home, ".profile", "export ORKIA_FROM_PROFILE=yes\n");

    let opts = ShellEngineOptions {
        load_bashrc: false,
        load_profile: true,
        login: false, // not a login shell
    };
    let mut fx = Fixture::build(home, opts).await;
    fx.engine
        .execute("echo \"[${ORKIA_FROM_PROFILE:-empty}]\"")
        .await
        .expect("exec");
    assert_eq!(fx.read_output().trim(), "[empty]");
}

#[tokio::test]
async fn orkiarc_syntax_error_is_non_fatal() {
    // brush handles parse errors internally: it prints a diagnostic to
    // the shell's stderr and sets last_exit_status to non-zero, but
    // an error but does not prevent shell from starting". This test
    // pins that contract: a malformed `.orkiarc` must not abort startup
    // and the engine must remain usable for subsequent commands.
    let home = TempDir::new().expect("tmp home");
    write_rc(&home, ".orkiarc", "if then fi\n");

    let opts = ShellEngineOptions::default();
    let mut fx = Fixture::build(home, opts).await;

    let orkiarc = fx._home.path().join(".orkiarc");
    let result = fx.engine.source_if_exists(&orkiarc).await;
    assert!(
        result.is_ok(),
        "brush should swallow parse errors internally, got {result:?}",
    );
    // brush sets last_exit_status to non-zero on parse failure.
    assert_ne!(fx.engine.last_exit(), 0, "last_exit should be non-zero");

    // Engine must still be usable after a syntax-rejected rc.
    fx.engine.execute("echo still-alive").await.expect("exec");
    assert!(fx.read_output().contains("still-alive"));
}

#[tokio::test]
async fn bashrc_parse_error_does_not_abort_startup() {
    let home = TempDir::new().expect("tmp home");
    write_rc(&home, ".bashrc", "if then fi\n");

    let _guard = lock_home();
    // SAFETY: serialised by `lock_home()`; brush reads $HOME during
    // source_default_rc, so the override must be live by then.
    unsafe {
        std::env::set_var("HOME", home.path());
    }
    let opts = ShellEngineOptions {
        load_bashrc: true,
        load_profile: false,
        login: false,
    };
    let mut engine = ShellEngine::new_with_options(opts)
        .await
        .expect("engine new");
    // brush prints a parse-error diagnostic to its own stderr (which is
    // currently the process stderr — fine for this test; production
    // wires it through the PTY). The Rust API stays Ok. The contract
    // we pin: `source_default_rc` returns without panicking and the
    // engine is still usable.
    let _warnings = engine.source_default_rc(opts).await;
    let _ = engine.execute("true").await.expect("execute after bad rc");
}
