// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use std::path::{Path, PathBuf};

pub(super) struct DaemonLock {
    path: PathBuf,
}

impl DaemonLock {
    pub(super) fn acquire(data_dir: &Path) -> Result<Self, String> {
        let path = data_dir.join("run").join("pty-daemon.lock");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        let body = std::process::id().to_string();
        // At most one stale-lock reclaim: if the create races `AlreadyExists`
        // and the recorded holder is dead (gone or a zombie), remove it and try
        // once more. A second `AlreadyExists` means a live competitor or an
        // unwritable dir — fail closed rather than spin forever.
        for attempt in 0..2 {
            match create_lock(&path) {
                Ok(mut file) => {
                    use std::io::Write;
                    let _ = writeln!(file, "{body}");
                    return Ok(Self { path });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    if attempt == 0 && stale_lock(&path) {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    return Err(format!("daemon lock already exists at {}", path.display()));
                }
                Err(err) => {
                    return Err(format!("create daemon lock {}: {err}", path.display()));
                }
            }
        }
        Err(format!("daemon lock already exists at {}", path.display()))
    }
}

fn create_lock(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

fn stale_lock(path: &Path) -> bool {
    let Ok(body) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(pid) = body.trim().parse::<u32>() else {
        return false;
    };
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    let gone = rc != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    // A zombie still answers `kill(pid, 0)` with success (the kernel keeps it in
    // the process table until it is reaped), so the `ESRCH` check alone treats a
    // dead-but-unreaped daemon as a live lock holder. Detect it explicitly.
    gone || is_zombie(pid)
}

/// Whether `pid` names a process that has exited but not yet been reaped.
///
/// `proc_pidinfo` is unusable here: its kernel `proc_find` skips zombies, so it
/// returns `ESRCH` for exactly the state we need to detect. Query the BSD process
/// table via sysctl instead — the path `ps` uses, which does report zombies. The
/// result is a `kinfo_proc`, whose embedded `extern_proc` holds the process state
/// in a `char p_stat`. On LP64 macOS that field sits at offset 36
/// (union `p_un`[16] + `p_vmspace`[8] + `p_sigacts`[8] + `p_flag`[4]).
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn is_zombie(pid: u32) -> bool {
    const P_STAT_OFFSET: usize = 36;
    let mut mib = [
        libc::CTL_KERN,
        libc::KERN_PROC,
        libc::KERN_PROC_PID,
        pid as libc::c_int,
    ];
    let mut size: usize = 0;
    // SAFETY: `mib` holds the 4 elements its length reports; a null `oldp` asks
    // sysctl to write only the required buffer size into `size`.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size <= P_STAT_OFFSET {
        return false;
    }
    let mut buf = vec![0u8; size];
    let mut got = size;
    // SAFETY: `buf` owns `size` bytes; sysctl writes at most `got` (= size) into
    // it and updates `got` to the bytes actually written, checked below.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            buf.as_mut_ptr().cast::<libc::c_void>(),
            &mut got,
            std::ptr::null_mut(),
            0,
        )
    };
    rc == 0 && got > P_STAT_OFFSET && buf[P_STAT_OFFSET] == libc::SZOMB as u8
}

#[cfg(target_os = "linux")]
fn is_zombie(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(content) => proc_stat_state(&content) == Some('Z'),
        Err(_) => false,
    }
}

/// Parse the process state field from the contents of `/proc/<pid>/stat`.
///
/// The format is `pid (comm) state ...`; `comm` may itself contain spaces and
/// parentheses, so anchor on the **last** `)` and read the first non-space
/// character after it.
#[cfg(target_os = "linux")]
fn proc_stat_state(content: &str) -> Option<char> {
    content.rsplit_once(')')?.1.trim_start().chars().next()
}

#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "linux")))]
fn is_zombie(_pid: u32) -> bool {
    false
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_pid_is_not_zombie_and_not_stale() {
        let me = std::process::id();
        assert!(!is_zombie(me), "the running test process is not a zombie");

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("pty-daemon.lock");
        std::fs::write(&path, format!("{me}\n")).expect("write lock");
        assert!(!stale_lock(&path), "a lock held by a live pid is not stale");
    }

    #[test]
    fn zombie_holder_is_stale_and_acquire_reclaims() {
        // A child we never reap becomes a zombie under this test process.
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn `true`");
        let pid = child.id();

        let mut became_zombie = false;
        for _ in 0..200 {
            if is_zombie(pid) {
                became_zombie = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(became_zombie, "child {pid} should be a zombie before reaping");

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("run")).expect("mkdir run");
        let path = dir.path().join("run").join("pty-daemon.lock");
        std::fs::write(&path, format!("{pid}\n")).expect("write lock");
        assert!(stale_lock(&path), "a zombie-held lock must read as stale");

        // The reclaim path removes the stale lock and re-creates it for us.
        let lock = DaemonLock::acquire(dir.path()).expect("acquire reclaims stale lock");
        let body = std::fs::read_to_string(&path).expect("read reclaimed lock");
        assert_eq!(body.trim(), std::process::id().to_string());
        drop(lock);

        // Reap the zombie so the test leaves no corpse behind.
        let _ = child.wait();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_stat_state_handles_tricky_comm() {
        assert_eq!(proc_stat_state("1234 (a) b) Z 1 0 0"), Some('Z'));
        assert_eq!(proc_stat_state("42 (cat) R 1 42 42"), Some('R'));
        assert_eq!(proc_stat_state("7 ((odd )name)) S 1"), Some('S'));
        assert_eq!(proc_stat_state("no paren here"), None);
    }
}
