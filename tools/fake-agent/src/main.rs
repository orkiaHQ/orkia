// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! `orkia-fake-agent` — a scripted, deterministic TUI agent.
//!
//! The harness installs this binary in place of `claude`/`codex`/`gemini`
//! when running Ring-2 end-to-end tests. It is **not a mock** of those
//! agents — it is a real TUI program that puts the TTY into raw mode,
//! emits OSC-133 prompt markers, calls the real `orkia bridge` hook
//! shim, and reads bytes from its PTY just like a real agent would.
//!
//! Behaviour is fully driven by a YAML script (`--script <path>`).
//! The schema lives in `orkia_test_harness::script` so the harness
//! author and the agent runtime agree byte-for-byte.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use orkia_test_harness::script::{AgentScript, CrashMode, Osc133Marker, ScriptStep};

fn main() {
    if let Err(e) = run() {
        eprintln!("orkia-fake-agent: {e:#}");
        std::process::exit(2);
    }
}

fn run() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mut script_path: Option<PathBuf> = None;
    let mut iter = args.iter().skip(1);
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--script" => {
                let p = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--script needs a path"))?;
                script_path = Some(PathBuf::from(p));
            }
            "--help" | "-h" => {
                println!("orkia-fake-agent --script <yaml>");
                return Ok(());
            }
            other => {
                // Ignore unknown args so the harness can pass things
                // through without breaking the agent.
                eprintln!("orkia-fake-agent: ignoring unknown arg: {other}");
            }
        }
    }
    let path = script_path.ok_or_else(|| anyhow::anyhow!("--script <path> required"))?;
    let script = AgentScript::from_yaml(&std::fs::read_to_string(&path)?)?;

    let _raw_guard = if script.raw_mode {
        Some(RawTermiosGuard::enter()?)
    } else {
        None
    };

    let bridge_bin = std::env::var("ORKIA_BRIDGE_BIN").unwrap_or_else(|_| "orkia".to_string());

    for (i, step) in script.steps.iter().enumerate() {
        if let Err(e) = run_step(step, &bridge_bin) {
            eprintln!("orkia-fake-agent: step {i} ({step:?}) failed: {e:#}");
            return Err(e);
        }
    }
    Ok(())
}

fn run_step(step: &ScriptStep, bridge_bin: &str) -> anyhow::Result<()> {
    match step {
        ScriptStep::Print { text } => {
            let mut out = std::io::stdout().lock();
            out.write_all(text.as_bytes())?;
            out.flush()?;
        }
        ScriptStep::Osc133 { marker, exit_code } => {
            let mut out = std::io::stdout().lock();
            let seq = osc133_sequence(*marker, *exit_code);
            out.write_all(seq.as_bytes())?;
            out.flush()?;
        }
        ScriptStep::Hook { source, payload } => {
            invoke_bridge(bridge_bin, source, payload)?;
        }
        ScriptStep::AwaitInput {
            bytes,
            until,
            timeout_ms,
        } => {
            await_input(*bytes, until.as_deref(), Duration::from_millis(*timeout_ms))?;
        }
        ScriptStep::DrainInput { ms } => {
            drain_input(Duration::from_millis(*ms))?;
        }
        ScriptStep::EchoUntilSubmit { timeout_ms } => {
            echo_until_submit(Duration::from_millis(*timeout_ms))?;
        }
        ScriptStep::Sleep { ms } => {
            std::thread::sleep(Duration::from_millis(*ms));
        }
        ScriptStep::Exit { code } => {
            std::process::exit(*code);
        }
        ScriptStep::Crash { mode } => {
            // Flush stdout so the harness sees pre-crash output before
            // the process disappears.
            let _ = std::io::stdout().lock().flush();
            crash(*mode);
        }
    }
    Ok(())
}

fn crash(mode: CrashMode) -> ! {
    match mode {
        CrashMode::Abort => std::process::abort(),
        CrashMode::Sigsegv => {
            #[cfg(unix)]
            // SAFETY: libc::raise takes a signal number and the SIGSEGV
            // constant is well-defined. The process exits via signal
            // handler, never returning to safe Rust.
            unsafe {
                libc::raise(libc::SIGSEGV);
            }
            // Fallback if raise returned (shouldn't happen) or on
            // non-unix targets.
            std::process::abort();
        }
    }
}

fn osc133_sequence(marker: Osc133Marker, exit_code: Option<i32>) -> String {
    // ESC ] 133 ; <letter> [;<arg>] BEL
    match marker {
        Osc133Marker::PromptStart => "\x1b]133;A\x07".to_string(),
        Osc133Marker::CommandStart => "\x1b]133;B\x07".to_string(),
        Osc133Marker::CommandOutput => "\x1b]133;C\x07".to_string(),
        Osc133Marker::CommandEnd => match exit_code {
            Some(c) => format!("\x1b]133;D;{c}\x07"),
            None => "\x1b]133;D\x07".to_string(),
        },
    }
}

fn invoke_bridge(
    bridge_bin: &str,
    source: &str,
    payload: &serde_json::Value,
) -> anyhow::Result<()> {
    // `--scope job` is mandatory: orkia exports `ORKIA_SOCKET_PATH` for
    // every spawned agent, and the real `orkia bridge` self-suppresses any
    // hook that is NOT job-scoped on such a session (it assumes it's the
    // user's duplicate global-settings hook). The hook orkia generates into
    // an agent's project settings always carries `--scope job`; the
    // fake-agent must emulate that, or its envelopes are dropped and never
    // reach the journal.
    let mut child = Command::new(bridge_bin)
        .arg("bridge")
        .arg("--source")
        .arg(source)
        .arg("--scope")
        .arg("job")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn {bridge_bin} bridge: {e}"))?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("bridge stdin"))?;
        let bytes = serde_json::to_vec(payload)?;
        stdin.write_all(&bytes)?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        anyhow::bail!(
            "bridge exited {:?}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

fn await_input(bytes: Option<usize>, until: Option<&str>, timeout: Duration) -> anyhow::Result<()> {
    let want_bytes = bytes.unwrap_or(0);
    let until_bytes: Vec<u8> = until.map(|s| s.as_bytes().to_vec()).unwrap_or_default();
    let mut got: Vec<u8> = Vec::new();
    let start = Instant::now();
    let fd = libc::STDIN_FILENO;
    set_nonblocking(fd, true)?;
    let _restore = scopeguard::ScopeGuard::new(fd, |fd| {
        let _ = set_nonblocking(fd, false);
    });
    loop {
        let remaining = timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            anyhow::bail!(
                "await_input timed out after {:?}; received {} bytes",
                timeout,
                got.len()
            );
        }
        // Block in poll(2) (capped per-tick) so the process is parked
        // on input — what orkia's prompt detector reads as "waiting".
        let tick = remaining.min(Duration::from_millis(200));
        if poll_read(fd, &mut got, tick)? {
            // EOF — treat as satisfied so tests can close stdin to
            // unblock the agent intentionally.
            return Ok(());
        }
        if want_bytes > 0 && got.len() >= want_bytes {
            return Ok(());
        }
        if !until_bytes.is_empty()
            && got
                .windows(until_bytes.len())
                .any(|w| w == until_bytes.as_slice())
        {
            return Ok(());
        }
    }
}

/// Read and discard everything on stdin for `window`, then return. The
/// agent stays parked in `poll(2)` between reads. Models a TUI agent
/// that consumes (and ignores) stdin while it boots — so bytes a caller
/// wrote to the PTY before the agent's input box was ready are lost,
/// not buffered.
fn drain_input(window: Duration) -> anyhow::Result<()> {
    let start = Instant::now();
    let fd = libc::STDIN_FILENO;
    set_nonblocking(fd, true)?;
    let _restore = scopeguard::ScopeGuard::new(fd, |fd| {
        let _ = set_nonblocking(fd, false);
    });
    let mut sink: Vec<u8> = Vec::new();
    while start.elapsed() < window {
        let remaining = window.saturating_sub(start.elapsed());
        let tick = remaining.min(Duration::from_millis(50));
        if poll_read(fd, &mut sink, tick)? {
            break; // EOF
        }
        sink.clear(); // pure discard — never accumulate
    }
    Ok(())
}

/// Read stdin, ECHO each byte back to stdout (so the typed text renders
/// on screen, like a real input box), and return once a submit byte (CR
/// or LF) arrives. Models what the injection executor's grid-confirm
/// relies on: the body becomes visible, and only the trailing `\r`
/// commits it.
fn echo_until_submit(timeout: Duration) -> anyhow::Result<()> {
    let start = Instant::now();
    let fd = libc::STDIN_FILENO;
    set_nonblocking(fd, true)?;
    let _restore = scopeguard::ScopeGuard::new(fd, |fd| {
        let _ = set_nonblocking(fd, false);
    });
    let mut got: Vec<u8> = Vec::new();
    loop {
        let remaining = timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            anyhow::bail!("echo_until_submit timed out after {:?}", timeout);
        }
        let before = got.len();
        let eof = poll_read(fd, &mut got, remaining.min(Duration::from_millis(200)))?;
        if got.len() > before {
            let mut out = std::io::stdout().lock();
            out.write_all(&got[before..])?;
            out.flush()?;
        }
        if got.iter().any(|&b| b == b'\r' || b == b'\n') || eof {
            return Ok(());
        }
    }
}

/// Block in `poll(2)` on `fd` for up to `tick`, then drain any readable
/// bytes into `got`. Returns `Ok(true)` on EOF. `fd` is expected to be
/// non-blocking so the follow-up read never stalls on a spurious wake.
fn poll_read(fd: libc::c_int, got: &mut Vec<u8>, tick: Duration) -> anyhow::Result<bool> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = i32::try_from(tick.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: `pfd` is a valid single pollfd; count 1 matches it.
    let pr = unsafe { libc::poll(&mut pfd, 1, ms) };
    if pr < 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EINTR) {
            return Ok(false);
        }
        return Err(e.into());
    }
    if pr == 0 || pfd.revents & libc::POLLIN == 0 {
        return Ok(false);
    }
    let mut buf = [0u8; 256];
    // SAFETY: `fd` is a valid descriptor; `buf` is exclusively ours.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
    if n > 0 {
        got.extend_from_slice(&buf[..n as usize]);
    } else if n == 0 {
        return Ok(true); // EOF
    }
    Ok(false)
}

fn set_nonblocking(fd: libc::c_int, on: bool) -> anyhow::Result<()> {
    // SAFETY: `fd` is a caller-owned descriptor; F_GETFL / F_SETFL on
    // a valid fd do not transfer ownership and have no aliasing
    // requirements.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let new = if on {
            flags | libc::O_NONBLOCK
        } else {
            flags & !libc::O_NONBLOCK
        };
        if libc::fcntl(fd, libc::F_SETFL, new) < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

/// Minimal RAII raw-termios guard. Restores the original termios on
/// drop so a panicking script doesn't leave the controlling TTY in
/// raw mode.
struct RawTermiosGuard {
    fd: libc::c_int,
    original: libc::termios,
}

impl RawTermiosGuard {
    fn enter() -> anyhow::Result<Self> {
        let fd = libc::STDIN_FILENO;
        // SAFETY: `termios` is a plain-old-data struct safe to zero.
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: `fd` is STDIN, always open for every Unix process;
        // we hand libc a writable pointer to a stack-allocated termios.
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut raw = original;
        // SAFETY: cfmakeraw only mutates the termios struct in place;
        // no fd interaction.
        unsafe { libc::cfmakeraw(&mut raw) };
        // SAFETY: same as tcgetattr above — STDIN is valid and we own
        // the termios snapshot we pass in.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self { fd, original })
    }
}

impl Drop for RawTermiosGuard {
    fn drop(&mut self) {
        // SAFETY: STDIN is still open at drop time (process lifetime);
        // `self.original` was captured in `enter()` from the same fd.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}

// Tiny inline scope-guard so we don't pull in the `scopeguard` crate.
mod scopeguard {
    pub struct ScopeGuard<T, F: FnMut(T)> {
        value: Option<T>,
        f: F,
    }
    impl<T, F: FnMut(T)> ScopeGuard<T, F> {
        pub fn new(value: T, f: F) -> Self {
            Self {
                value: Some(value),
                f,
            }
        }
    }
    impl<T, F: FnMut(T)> Drop for ScopeGuard<T, F> {
        fn drop(&mut self) {
            if let Some(v) = self.value.take() {
                (self.f)(v);
            }
        }
    }
}
