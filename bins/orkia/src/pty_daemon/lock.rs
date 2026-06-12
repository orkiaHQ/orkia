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
        match create_lock(&path) {
            Ok(mut file) => {
                use std::io::Write;
                let _ = writeln!(file, "{body}");
                Ok(Self { path })
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                if stale_lock(&path) {
                    let _ = std::fs::remove_file(&path);
                    return Self::acquire(data_dir);
                }
                Err(format!("daemon lock already exists at {}", path.display()))
            }
            Err(err) => Err(format!("create daemon lock {}: {err}", path.display())),
        }
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
    rc != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
