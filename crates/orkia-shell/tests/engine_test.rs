// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Headless tests for the brush-backed `ShellEngine`.
//!
//! Each test writes stdout/stderr to a tempfile by installing a `File`-backed
//! `OpenFile` into `Shell::open_files`. After running, we read the file back
//! and assert. This bypasses needing a real PTY.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use brush_core::openfiles::{OpenFile, OpenFiles};
use orkia_shell::ShellEngine;
use tempfile::NamedTempFile;

/// Per-test fixture: an engine plus a tempfile that captures fd1/fd2.
struct Fixture {
    engine: ShellEngine,
    out: NamedTempFile,
}

impl Fixture {
    async fn new() -> Self {
        let mut engine = ShellEngine::new().await.expect("engine new");
        let out = NamedTempFile::new().expect("tempfile");

        // Reopen the file by path; we keep `out` alive so the path stays valid.
        let stdout = File::options()
            .append(true)
            .open(out.path())
            .expect("open stdout file");
        let stderr = File::options()
            .append(true)
            .open(out.path())
            .expect("open stderr file");

        let files = engine.shell_mut().open_files_mut();
        files.set_fd(OpenFiles::STDOUT_FD, OpenFile::File(stdout));
        files.set_fd(OpenFiles::STDERR_FD, OpenFile::File(stderr));

        Self { engine, out }
    }

    fn read_output(&mut self) -> String {
        let mut buf = String::new();
        let mut f = self.out.reopen().expect("reopen for read");
        f.seek(SeekFrom::Start(0)).expect("seek");
        f.read_to_string(&mut buf).expect("read");
        buf
    }

    fn clear_output(&mut self) {
        self.out.as_file().set_len(0).expect("truncate");
    }
}

#[tokio::test]
async fn cd_persists_across_calls() {
    let mut fx = Fixture::new().await;
    let tmp = tempfile::tempdir().expect("tmpdir");
    let path = tmp.path().to_owned();

    let cd = format!("cd {}", path.display());
    fx.engine.execute(&cd).await.expect("cd");
    fx.engine.execute("pwd").await.expect("pwd");

    let out = fx.read_output();
    // pwd may resolve symlinks differently than the input path; check the leaf.
    let leaf = path
        .file_name()
        .expect("leaf")
        .to_string_lossy()
        .into_owned();
    assert!(
        out.contains(&leaf),
        "expected output to contain {leaf:?}, got {out:?}"
    );
    assert_eq!(
        fx.engine.cwd().file_name().unwrap(),
        path.file_name().unwrap()
    );
}

#[tokio::test]
async fn export_persists_across_calls() {
    let mut fx = Fixture::new().await;
    fx.engine
        .execute("export ORKIA_TEST_VAR=hello")
        .await
        .unwrap();
    fx.engine.execute("echo $ORKIA_TEST_VAR").await.unwrap();
    assert_eq!(fx.read_output().trim(), "hello");
}

#[tokio::test]
async fn alias_persists_across_calls() {
    let mut fx = Fixture::new().await;
    // shopt expand_aliases is normally interactive-only; brush enables it
    // when interactive(true). We're non-interactive, so set it explicitly.
    fx.engine.execute("shopt -s expand_aliases").await.unwrap();
    fx.engine.execute("alias greet='echo hi'").await.unwrap();
    fx.engine.execute("greet").await.unwrap();
    assert_eq!(fx.read_output().trim(), "hi");
}

#[tokio::test]
async fn pipe_then_redirect() {
    let mut fx = Fixture::new().await;
    let target = NamedTempFile::new().unwrap();
    let cmd = format!(
        "printf 'foo\\nbar\\nbaz\\n' | grep ba > {}",
        target.path().display()
    );
    let r = fx.engine.execute(&cmd).await.unwrap();
    assert_eq!(r.exit_code, 0);
    let mut content = String::new();
    File::open(target.path())
        .unwrap()
        .read_to_string(&mut content)
        .unwrap();
    assert_eq!(content, "bar\nbaz\n");
}

#[tokio::test]
async fn glob_expansion() {
    let mut fx = Fixture::new().await;
    let tmp = tempfile::tempdir().unwrap();
    File::create(tmp.path().join("a.rs")).unwrap();
    File::create(tmp.path().join("b.rs")).unwrap();
    File::create(tmp.path().join("c.txt")).unwrap();
    fx.engine
        .execute(&format!("cd {}", tmp.path().display()))
        .await
        .unwrap();
    fx.clear_output();
    fx.engine.execute("echo *.rs").await.unwrap();
    let out = fx.read_output();
    assert!(out.contains("a.rs") && out.contains("b.rs"), "got {out:?}");
    assert!(!out.contains("c.txt"));
}

#[tokio::test]
async fn expand_to_argv_resolves_env_glob_tilde() {
    // The bg-shell path (`cmd &`) parses + expands the command line
    // via brush and spawns the resulting argv directly. Validate
    // that the underlying brush API delivers exactly the bash
    // semantics we expect — env, glob, tilde, multi-word splitting.
    let mut fx = Fixture::new().await;
    let tmp = tempfile::tempdir().expect("tmp");
    fx.engine
        .execute(&format!("cd {}", tmp.path().display()))
        .await
        .unwrap();
    fx.engine.execute("touch a.rs b.rs c.txt").await.unwrap();
    fx.engine.execute("export GREET=hi").await.unwrap();

    let params = brush_core::ExecutionParameters::default();
    let argv = fx
        .engine
        .shell_mut()
        .full_expand_and_split_string(&params, "echo $GREET *.rs")
        .await
        .expect("expand");
    // argv[0] is the command name; argv[1] is $GREET expansion; the
    // glob expands to the two .rs files in current dir.
    assert_eq!(argv[0], "echo");
    assert_eq!(argv[1], "hi");
    let rest: Vec<&String> = argv[2..].iter().collect();
    assert!(
        rest.iter().any(|s| s.ends_with("a.rs")) && rest.iter().any(|s| s.ends_with("b.rs")),
        "expected a.rs and b.rs in argv, got {argv:?}",
    );
    assert!(
        !rest.iter().any(|s| s.ends_with("c.txt")),
        "non-matching glob target must not appear in argv, got {argv:?}",
    );
}

#[tokio::test]
async fn home_var_expansion() {
    let mut fx = Fixture::new().await;
    fx.engine.execute("echo $HOME").await.unwrap();
    let out = fx.read_output();
    let home = std::env::var("HOME").unwrap_or_default();
    assert_eq!(out.trim(), home.trim());
}

#[tokio::test]
async fn exit_code_propagates() {
    let mut fx = Fixture::new().await;
    let r = fx.engine.execute("false").await.unwrap();
    assert_eq!(r.exit_code, 1);
    assert!(!r.should_exit);

    fx.engine.execute("false; echo $?").await.unwrap();
    assert_eq!(fx.read_output().trim(), "1");
}

#[tokio::test]
async fn command_not_found_is_127() {
    let mut fx = Fixture::new().await;
    let r = fx
        .engine
        .execute("this_command_does_not_exist_anywhere_12345")
        .await
        .unwrap();
    assert_eq!(r.exit_code, 127);
}

#[tokio::test]
async fn exit_builtin_sets_should_exit() {
    let mut fx = Fixture::new().await;
    let r = fx.engine.execute("exit 0").await.unwrap();
    assert!(r.should_exit);
}

#[tokio::test]
async fn exported_env_includes_exports() {
    let mut fx = Fixture::new().await;
    fx.engine
        .execute("export ORKIA_AGENT_VAR=xyz")
        .await
        .unwrap();
    let env = fx.engine.exported_env();
    let hit = env.iter().find(|(k, _)| k == "ORKIA_AGENT_VAR");
    assert_eq!(hit.map(|(_, v)| v.as_str()), Some("xyz"));
}
