// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use std::os::unix::net::UnixStream;
use std::path::Path;

pub(super) fn set_private_socket_permissions(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms).map_err(|e| format!("chmod {}: {e}", path.display()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

pub(super) fn verify_peer_owner(stream: &UnixStream) -> Result<(), String> {
    let Some(uid) = peer_uid(stream)? else {
        return Ok(());
    };
    let current = unsafe { libc::geteuid() };
    if uid == current {
        Ok(())
    } else {
        Err(format!(
            "peer uid {uid} does not match current uid {current}"
        ))
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd"
))]
fn peer_uid(stream: &UnixStream) -> Result<Option<libc::uid_t>, String> {
    use std::os::fd::AsRawFd;
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if rc == 0 {
        Ok(Some(uid))
    } else {
        Err(format!("getpeereid: {}", std::io::Error::last_os_error()))
    }
}

#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> Result<Option<libc::uid_t>, String> {
    use std::mem::MaybeUninit;
    use std::os::fd::AsRawFd;
    let mut cred = MaybeUninit::<libc::ucred>::uninit();
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            cred.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(format!("SO_PEERCRED: {}", std::io::Error::last_os_error()));
    }
    let cred = unsafe { cred.assume_init() };
    Ok(Some(cred.uid))
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "linux"
)))]
fn peer_uid(_stream: &UnixStream) -> Result<Option<libc::uid_t>, String> {
    Ok(None)
}
