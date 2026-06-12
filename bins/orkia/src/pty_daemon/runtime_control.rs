// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use super::DaemonStageInfo;

const CONTROL_VERSION: u16 = 1;

pub(super) fn control_socket_path(data_dir: &Path, id: u32) -> PathBuf {
    data_dir
        .join("run")
        .join("jobs")
        .join(id.to_string())
        .join("control.sock")
}

pub(super) fn tell_with_retry(path: &Path, target: &str, message: &str) -> Result<(), String> {
    let request = serde_json::json!({
        "method": "tell",
        "target": runtime_target(target),
        "message": message,
    });
    request_ok_with_retry(path, &request, "tell")
}

pub(super) fn kill_with_retry(path: &Path, target: &str) -> Result<(), String> {
    let request = serde_json::json!({
        "method": "kill",
        "target": runtime_target(target),
    });
    request_ok_with_retry(path, &request, "kill")
}

pub(super) fn attach_proxy(
    path: &Path,
    target: &str,
    winsize: Option<(u16, u16)>,
    mut client: UnixStream,
) -> Result<(), String> {
    // Bounded connect retry (same 20×50 ms budget as `request_ok_with_retry`):
    // an attach issued right after spawn races the runtime binding its control
    // socket, and tell/kill already tolerate that window.
    let mut runtime = match connect_with_retry(path) {
        Ok(stream) => stream,
        Err(err) => {
            let message = format!("connect {}: {err}", path.display());
            write_daemon_attach_error(&mut client, &message);
            return Err(message);
        }
    };
    let request = serde_json::json!({
        "method": "attach",
        "target": runtime_target(target),
        "cols": winsize.map(|(c, _)| c),
        "rows": winsize.map(|(_, r)| r),
    });
    if let Err(err) = write_json_line(&mut runtime, &request) {
        write_daemon_attach_error(&mut client, &err);
        return Err(err);
    }
    match read_json_line(&mut runtime) {
        Err(err) => {
            write_daemon_attach_error(&mut client, &err);
            Err(err)
        }
        Ok(serde_json::Value::Object(obj))
            if obj.get("status").and_then(|v| v.as_str()) == Some("ok") =>
        {
            super::protocol::write_ok(&mut client).map_err(|e| format!("write attach ok: {e}"))?;
            splice(client, runtime);
            Ok(())
        }
        Ok(serde_json::Value::Object(obj)) => {
            let message = obj
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("runtime attach failed");
            write_daemon_attach_error(&mut client, message);
            Err(message.to_string())
        }
        Ok(_) => {
            let message = "runtime attach returned malformed response";
            write_daemon_attach_error(&mut client, message);
            Err(message.to_string())
        }
    }
}

pub(super) fn list(path: &Path) -> Option<Vec<DaemonStageInfo>> {
    let req = serde_json::json!({ "method": "list" });
    let resp = send(path, &req).ok()?;
    let stages = resp.get("stages")?.as_array()?;
    Some(
        stages
            .iter()
            .filter_map(|stage| {
                Some(DaemonStageInfo {
                    target: stage.get("target")?.as_str()?.to_string(),
                    id: stage
                        .get("id")
                        .and_then(|p| p.as_u64())
                        .map(|p| p as u32)
                        .unwrap_or(0),
                    state: stage.get("state")?.as_str()?.to_string(),
                    pid: stage.get("pid").and_then(|p| p.as_u64()).map(|p| p as u32),
                    runtime_secs: stage.get("runtime_secs")?.as_u64()?,
                    lost_reason: stage
                        .get("lost_reason")
                        .and_then(|p| p.as_str())
                        .map(ToString::to_string),
                    exit_code: stage
                        .get("exit_code")
                        .and_then(|p| p.as_i64())
                        .map(|p| p as i32),
                    attachable: stage
                        .get("attachable")
                        .and_then(|p| p.as_bool())
                        .unwrap_or(false),
                })
            })
            .collect(),
    )
}

fn runtime_target(target: &str) -> String {
    if target.parse::<u32>().is_ok() {
        target.to_string()
    } else {
        format!("@{target}")
    }
}

fn connect_with_retry(path: &Path) -> std::io::Result<UnixStream> {
    let mut last = None;
    for _ in 0..20 {
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(err) => {
                last = Some(err);
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("connect retries exhausted")))
}

fn request_ok_with_retry(path: &Path, req: &serde_json::Value, op: &str) -> Result<(), String> {
    for _ in 0..20 {
        match send(path, req) {
            Ok(serde_json::Value::Object(obj)) => {
                if obj.get("status").and_then(|v| v.as_str()) == Some("ok") {
                    return Ok(());
                }
                let message = obj
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("runtime control request failed");
                return Err(message.to_string());
            }
            Ok(_) => return Err("runtime control returned malformed response".to_string()),
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
    Err(format!(
        "runtime control {op} unavailable at {}",
        path.display()
    ))
}

fn send(path: &Path, request: &serde_json::Value) -> Result<serde_json::Value, String> {
    let mut stream =
        UnixStream::connect(path).map_err(|e| format!("connect {}: {e}", path.display()))?;
    let timeout = Some(std::time::Duration::from_millis(control_timeout_ms()));
    stream
        .set_read_timeout(timeout)
        .map_err(|e| format!("set control read timeout: {e}"))?;
    stream
        .set_write_timeout(timeout)
        .map_err(|e| format!("set control write timeout: {e}"))?;
    write_json_line(&mut stream, request)?;
    read_json_line(&mut stream)
}

fn write_json_line(stream: &mut UnixStream, request: &serde_json::Value) -> Result<(), String> {
    let wire = serde_json::json!({
        "version": CONTROL_VERSION,
        "method": request.get("method").cloned().unwrap_or(serde_json::Value::Null),
        "target": request.get("target").cloned(),
        "message": request.get("message").cloned(),
        "cols": request.get("cols").cloned(),
        "rows": request.get("rows").cloned(),
    });
    let mut line =
        serde_json::to_string(&strip_nulls(wire)).map_err(|e| format!("serialize control: {e}"))?;
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(|e| format!("write control request: {e}"))
}

fn strip_nulls(value: serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(map) = value else {
        return value;
    };
    serde_json::Value::Object(
        map.into_iter()
            .filter(|(_, value)| !value.is_null())
            .collect(),
    )
}

fn read_json_line(stream: &mut UnixStream) -> Result<serde_json::Value, String> {
    let mut response = Vec::with_capacity(256);
    let mut byte = [0_u8; 1];
    loop {
        let n = stream
            .read(&mut byte)
            .map_err(|e| format!("read control response: {e}"))?;
        if n == 0 {
            return Err("runtime control closed before response".to_string());
        }
        if byte[0] == b'\n' {
            break;
        }
        response.push(byte[0]);
        if response.len() > 64 * 1024 {
            return Err("runtime control response too large".to_string());
        }
    }
    serde_json::from_slice(&response).map_err(|e| format!("parse control response: {e}"))
}

fn splice(mut client: UnixStream, mut runtime: UnixStream) {
    let mut runtime_out = match runtime.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut client_out = match client.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let output = std::thread::spawn(move || {
        let _ = std::io::copy(&mut runtime_out, &mut client_out);
        let _ = client_out.shutdown(std::net::Shutdown::Write);
    });
    let _ = std::io::copy(&mut client, &mut runtime);
    let _ = runtime.shutdown(std::net::Shutdown::Write);
    let _ = output.join();
}

fn write_daemon_attach_error(client: &mut UnixStream, message: &str) {
    let _ = super::protocol::write_error(client, message.to_string());
}

fn control_timeout_ms() -> u64 {
    std::env::var("ORKIA_RUNTIME_CONTROL_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(250)
        .max(50)
}
