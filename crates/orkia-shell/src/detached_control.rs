// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use serde::{Deserialize, Serialize};

/// Terminal-state reset sent to the attach client when the job exits or
/// the client detaches, so the client's host terminal is restored.
/// Matches `emit_detach_cleanup()` in `raw_attach.rs` verbatim.
const DETACH_CLEANUP: &[u8] = concat!(
    "\x1b[?1049l",                                  // leave alt-screen
    "\x1b[?25h",                                    // cursor visible
    "\x1b[0m",                                      // reset SGR
    "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l", // mouse off
    "\x1b[?2004l",                                  // bracketed paste off
    "\x1b[?1004l",                                  // focus reporting off
    "\x1b[H",                                       // cursor to top-left
    "\x1b[2J",                                      // clear screen
    "\x1b[3J",                                      // clear scrollback
)
.as_bytes();

const CONTROL_VERSION: u16 = 1;

#[derive(Debug, Deserialize)]
struct WireEnvelope {
    #[serde(default)]
    version: u16,
    #[serde(flatten)]
    request: WireRequest,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
enum WireRequest {
    List,
    Tell {
        target: String,
        message: String,
    },
    Kill {
        target: String,
    },
    Attach {
        target: String,
        // Attaching terminal's size — additive (`serde(default)` keeps old
        // daemons compatible); `None` keeps the agent's current PTY size.
        #[serde(default)]
        cols: Option<u16>,
        #[serde(default)]
        rows: Option<u16>,
    },
}

#[derive(Debug, Serialize)]
pub struct StageInfo {
    pub id: u32,
    pub target: String,
    pub state: String,
    pub pid: Option<u32>,
    pub runtime_secs: u64,
    pub attachable: bool,
    pub lost_reason: Option<String>,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ControlResponse {
    Ok,
    List { stages: Vec<StageInfo> },
    Error { message: String },
}

pub enum ControlCommand {
    List {
        respond: mpsc::Sender<ControlResponse>,
    },
    Tell {
        target: String,
        message: String,
        respond: mpsc::Sender<ControlResponse>,
    },
    Kill {
        target: String,
        respond: mpsc::Sender<ControlResponse>,
    },
    Attach {
        target: String,
        /// Attaching terminal's `(cols, rows)` — the REPL resizes the
        /// stage engine BEFORE snapshotting so the catch-up paint matches.
        winsize: Option<(u16, u16)>,
        stream: UnixStream,
    },
}

pub fn spawn_from_env() -> Option<mpsc::Receiver<ControlCommand>> {
    let path = std::env::var("ORKIA_DETACHED_CONTROL_SOCK").ok()?;
    let path = PathBuf::from(path);
    spawn(path).ok()
}

/// When this process is a detached agent runtime — the daemon spawned it
/// with `ORKIA_DETACHED_JOB_ID` set (see the bin's `runtime_spawn::build_env`)
/// — return the per-job socket its own journal hub should bind:
/// `<data_dir>/run/jobs/<job_id>/agent.sock`. `None` for the main interactive
/// REPL (no env var), so every existing caller keeps the global
/// `<data_dir>/run/orkia.sock`.
///
/// runtime hosts and consumes its own agent's hooks on this socket — it never
/// subscribes to the daemon hub (which would steal the main REPL's single
/// subscriber slot) — then forwards lifecycle + final response up to the
/// daemon, which relays to the main REPL. The job id is parsed as `u32`
/// (untrusted env, #7): a missing or non-numeric value yields `None`.
pub fn detached_runtime_hub_socket(data_dir: &Path) -> Option<PathBuf> {
    let id: u32 = std::env::var("ORKIA_DETACHED_JOB_ID").ok()?.parse().ok()?;
    Some(
        data_dir
            .join("run")
            .join("jobs")
            .join(id.to_string())
            .join("agent.sock"),
    )
}

fn spawn(path: PathBuf) -> Result<mpsc::Receiver<ControlCommand>, String> {
    prepare_socket(&path)?;
    let listener =
        UnixListener::bind(&path).map_err(|e| format!("bind {}: {e}", path.display()))?;
    set_private_socket_permissions(&path)?;
    let (tx, rx) = mpsc::channel::<ControlCommand>();
    std::thread::Builder::new()
        .name("orkia-detached-control".to_string())
        .spawn(move || accept_loop(listener, tx))
        .map_err(|e| format!("spawn detached control: {e}"))?;
    Ok(rx)
}

fn prepare_socket(path: &Path) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Err(format!("invalid control socket path {}", path.display()));
    };
    std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    let _ = std::fs::remove_file(path);
    Ok(())
}

fn accept_loop(listener: UnixListener, tx: mpsc::Sender<ControlCommand>) {
    for incoming in listener.incoming() {
        let Ok(stream) = incoming else { continue };
        let tx = tx.clone();
        let _ = std::thread::Builder::new()
            .name("orkia-detached-control-client".to_string())
            .spawn(move || handle_client(stream, tx));
    }
}

fn handle_client(mut stream: UnixStream, tx: mpsc::Sender<ControlCommand>) {
    if let Err(e) = verify_peer_owner(&stream) {
        let _ = write_response(
            &mut stream,
            &ControlResponse::Error {
                message: format!("unauthorized client: {e}"),
            },
        );
        return;
    }
    let reader_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            let _ = write_response(
                &mut stream,
                &ControlResponse::Error {
                    message: format!("clone stream: {e}"),
                },
            );
            return;
        }
    };
    let mut reader = BufReader::new(reader_stream);
    let mut line = String::new();
    if let Err(e) = reader.read_line(&mut line) {
        let _ = write_response(
            &mut stream,
            &ControlResponse::Error {
                message: format!("read request: {e}"),
            },
        );
        return;
    }
    let wire: WireEnvelope = match serde_json::from_str(&line) {
        Ok(wire) => wire,
        Err(e) => {
            let _ = write_response(
                &mut stream,
                &ControlResponse::Error {
                    message: format!("parse request: {e}"),
                },
            );
            return;
        }
    };
    if wire.version != CONTROL_VERSION {
        let _ = write_response(
            &mut stream,
            &ControlResponse::Error {
                message: format!(
                    "runtime control protocol mismatch: client={} runtime={}",
                    wire.version, CONTROL_VERSION
                ),
            },
        );
        return;
    }
    let cmd = match wire.request {
        WireRequest::List => {
            let (respond, rx) = mpsc::channel::<ControlResponse>();
            if tx.send(ControlCommand::List { respond }).is_err() {
                write_control_stopped(&mut stream);
                return;
            }
            write_control_response(stream, rx);
            return;
        }
        WireRequest::Tell { target, message } => {
            let (respond, rx) = mpsc::channel::<ControlResponse>();
            if tx
                .send(ControlCommand::Tell {
                    target,
                    message,
                    respond,
                })
                .is_err()
            {
                write_control_stopped(&mut stream);
                return;
            }
            write_control_response(stream, rx);
            return;
        }
        WireRequest::Kill { target } => {
            let (respond, rx) = mpsc::channel::<ControlResponse>();
            if tx.send(ControlCommand::Kill { target, respond }).is_err() {
                write_control_stopped(&mut stream);
                return;
            }
            write_control_response(stream, rx);
            return;
        }
        WireRequest::Attach { target, cols, rows } => ControlCommand::Attach {
            target,
            winsize: cols.zip(rows),
            stream,
        },
    };
    if tx.send(cmd).is_err() {
        // Ownership of the stream was moved into the command. If the
        // channel is closed here there is no safe stream left to report on.
    }
}

fn write_response(stream: &mut UnixStream, resp: &ControlResponse) -> std::io::Result<()> {
    let mut line = serde_json::to_string(resp)?;
    line.push('\n');
    stream.write_all(line.as_bytes())
}

fn write_control_stopped(stream: &mut UnixStream) {
    let _ = write_response(
        stream,
        &ControlResponse::Error {
            message: "runtime control loop stopped".to_string(),
        },
    );
}

fn write_control_response(mut stream: UnixStream, rx: mpsc::Receiver<ControlResponse>) {
    let resp = rx.recv().unwrap_or(ControlResponse::Error {
        message: "runtime response channel closed".to_string(),
    });
    let _ = write_response(&mut stream, &resp);
}

pub fn pump_stage_attach(
    mut stream: UnixStream,
    history: Vec<u8>,
    snapshot: Vec<u8>,
    rx: mpsc::Receiver<Vec<u8>>,
    writer: orkia_pty::SharedWriter,
) {
    if write_response(&mut stream, &ControlResponse::Ok).is_err() {
        return;
    }
    let mut output = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let output_thread = std::thread::spawn(move || {
        let catch_up = if history.is_empty() {
            snapshot
        } else {
            history
        };
        if !catch_up.is_empty() {
            if output.write_all(&catch_up).is_err() {
                return;
            }
            let _ = output.flush();
        }
        loop {
            if stop_rx.try_recv().is_ok() {
                break;
            }
            match rx.recv_timeout(std::time::Duration::from_millis(50)) {
                Ok(chunk) => {
                    if output.write_all(&chunk).is_err() {
                        break;
                    }
                    let _ = output.flush();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // Job PTY closed — emit cleanup so the client's host
                    // terminal is restored before the stream closes.
                    let _ = output.write_all(DETACH_CLEANUP);
                    let _ = output.flush();
                    break;
                }
            }
        }
    });
    splice_input_to_writer(&mut stream, writer);
    let _ = stop_tx.send(());
    let _ = stream.shutdown(std::net::Shutdown::Both);
    let _ = output_thread.join();
}

pub fn write_attach_error(mut stream: UnixStream, message: String) {
    let _ = write_response(&mut stream, &ControlResponse::Error { message });
}

fn splice_input_to_writer(stream: &mut UnixStream, writer: orkia_pty::SharedWriter) {
    let mut buf = [0_u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let mut w = writer.lock();
                if w.write_all(&buf[..n]).is_err() {
                    break;
                }
                let _ = w.flush();
            }
            Err(_) => break,
        }
    }
}

fn set_private_socket_permissions(path: &Path) -> Result<(), String> {
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

fn verify_peer_owner(stream: &UnixStream) -> Result<(), String> {
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

#[cfg(test)]
mod hub_socket_tests {
    use super::*;

    /// Serializes the process-wide `ORKIA_DETACHED_JOB_ID` mutation so the
    /// three cases below never race each other on the shared env var.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn hub_socket_reflects_detached_job_id() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let data_dir = Path::new("/tmp/orkia-test");
        let prev = std::env::var_os("ORKIA_DETACHED_JOB_ID");

        // SAFETY: process-wide env mutation serialized on `ENV_LOCK`.
        unsafe {
            std::env::set_var("ORKIA_DETACHED_JOB_ID", "42");
        }
        assert_eq!(
            detached_runtime_hub_socket(data_dir),
            Some(PathBuf::from("/tmp/orkia-test/run/jobs/42/agent.sock")),
        );

        // Non-numeric value is rejected (#7 — untrusted env).
        unsafe {
            std::env::set_var("ORKIA_DETACHED_JOB_ID", "../escape");
        }
        assert_eq!(detached_runtime_hub_socket(data_dir), None);

        // Absent var → main REPL → no override (keeps the global socket).
        unsafe {
            std::env::remove_var("ORKIA_DETACHED_JOB_ID");
        }
        assert_eq!(detached_runtime_hub_socket(data_dir), None);

        // SAFETY: restore prior state under the same guard.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("ORKIA_DETACHED_JOB_ID", v),
                None => std::env::remove_var("ORKIA_DETACHED_JOB_ID"),
            }
        }
    }
}
