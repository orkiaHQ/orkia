// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use orkia_shell::journal::JournalEnvelope;
use serde::{Deserialize, Serialize};

const SOCKET_NAME: &str = "pty-daemon.sock";
pub(super) const PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct WireRequest {
    #[serde(default)]
    pub(super) version: u16,
    #[serde(flatten)]
    pub(super) request: Request,
}

/// Cage wrapper carried over the wire (mirrors `CageWrapper` in
/// `orkia-shell::job::config`). When absent the daemon spawns the
/// command directly — byte-identical to the pre-parity behaviour.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CageWrapperProto {
    /// Launcher binary resolved via `$PATH` by the child (e.g.
    /// `orkia-cage`).
    pub cage_bin: String,
    /// Policy file path passed as `--policy <path>`.
    pub policy_path: String,
}

// The `Spawn` variant is substantially larger than other variants because it
// carries the full set of optional parity fields. This is acceptable: `Request`
// is a per-IPC-call allocation (not stored in a collection), so the extra stack
// size is never paid in a hot path.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub(super) enum Request {
    Status,
    Spawn {
        command: String,
        /// Agent argv (including provider-specific flags like
        /// `--mcp-config`). Absent ⇒ daemon uses its default
        /// `["-c", detached_runtime_command(command)]`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        /// Working directory for the spawned child. Absent ⇒ daemon
        /// inherits its own cwd (today's behaviour).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        working_dir: Option<String>,
        /// Human-readable agent name written to `ORKIA_AGENT_NAME`.
        /// Absent ⇒ env var not set.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_name: Option<String>,
        /// Arbitrary env key-value pairs merged on top of the
        /// daemon's hardcoded env block. Absent ⇒ no extras.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        extra_env: Vec<(String, String)>,
        /// Orkia Cage launcher wrapper. Absent ⇒ spawn `command`
        /// directly (today's behaviour).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cage_wrapper: Option<CageWrapperProto>,
        /// Stdin disposition: `"pty"` (default), `"inherit"`,
        /// `"null"`, or `"initial_bytes"` (requires
        /// `initial_stdin_bytes`). Absent ⇒ PTY stdin.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stdin_mode: Option<String>,
        /// Bytes to write into the PTY master immediately after
        /// spawn. Only meaningful when `stdin_mode = "initial_bytes"`.
        /// Base64-encoded in the JSON wire format via serde's
        /// `Vec<u8>` serialiser (becomes an array of integers).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        initial_stdin_bytes: Vec<u8>,
        /// Process group mode: `"new_session"` (default) or
        /// `"inherit"`. Absent ⇒ new session (today's behaviour).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        process_group: Option<String>,
        /// Terminal width hint (columns). Absent ⇒ daemon queries
        /// its own terminal.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        terminal_cols: Option<usize>,
        /// Terminal height hint (rows). Absent ⇒ daemon queries its
        /// own terminal.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        terminal_rows: Option<usize>,
        /// When `true`, set the OSC-133 capability env trio:
        /// `ORKIA=1`, `ORKIA_PROTOCOL_VERSION=1`,
        /// `TERM_PROGRAM=orkia`. The callbacks themselves are not yet
        /// wired; this flag only controls the env vars.
        /// Absent / `false` ⇒ `TERM=xterm-256color` is set instead.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        osc133: bool,
    },
    List,
    Gc,
    Inspect {
        id: u32,
    },
    Logs {
        id: u32,
        limit: Option<usize>,
    },
    Tell {
        id: u32,
        target: String,
        message: String,
    },
    Stop {
        id: u32,
    },
    Kill {
        id: u32,
        target: Option<String>,
    },
    Attach {
        id: u32,
        target: Option<String>,
        /// The attaching terminal's size (TIOCGWINSZ), so the runtime can
        /// resize the agent PTY before the catch-up paint. Additive
        /// (`serde(default)`): an old client omits it and the agent keeps
        /// its spawn-time size — today's behavior.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cols: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rows: Option<u16>,
    },
    /// Open a journal-envelope stream (REPL→daemon). After the daemon
    /// accepts this request it keeps the connection open and writes a
    /// continuous sequence of [`JournalStreamFrame`] NDJSON lines until
    /// receives every envelope the daemon-resident hub fans out — for its
    /// REPL-resident consumers (approval, attention, enrichment, oneshot,
    /// sink). Additive: an old daemon rejects it as an unknown method,
    /// which the REPL handles by falling back to owning the socket itself.
    Subscribe,
    /// Emit an in-process journal envelope into the daemon-resident hub
    /// (REPL→daemon). Replaces the in-process `JournalHub::sender()` path
    /// once the hub lives in the daemon: shell-originated events (SEAL
    /// shell records, scope changes, tells) that the REPL produces locally
    /// are pushed here so they join the same bus the socket feeds.
    JournalEmit {
        envelope: JournalEnvelope,
    },
    /// Bind a job to its project for the daemon-resident SEAL consumer
    /// (REPL→daemon). Mirrors the REPL-owned `JobProjects` map
    /// (`job_id → project`). Pushed at spawn time, before the
    /// `agent.spawn` SEAL record, so the consumer can resolve the
    /// project of events that don't carry it inline. `project = None`
    /// clears the binding (job teardown).
    JobScope {
        job_id: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<String>,
    },
    /// Reply to a [`StreamFrame::McpProxy`] (REPL→daemon). The daemon owns
    /// `orkia.sock`, so MCP/RFC JSON-RPC frames land at the daemon — but
    /// dispatch needs REPL-resident state (RFC services, knowledge graph,
    /// PTY bridge). The daemon forwards each frame to the REPL as an
    /// `McpProxy` stream frame keyed by `corr_id`; the REPL dispatches and
    /// returns the serialized JSON-RPC `response` here. The daemon writes
    /// `response` back to the parked agent connection and, when
    /// `accessed_node_ids` is non-empty, emits a `KnowledgeAccess` envelope.
    McpProxyReply {
        corr_id: u64,
        response: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        accessed_node_ids: Vec<String>,
    },
    /// A detached runtime forwards one of its in-process `JobEvent`s up to the
    /// Option C). The runtime owns the job's lifecycle/terminal signals; it
    /// pushes each here and the daemon relays it to the main REPL subscriber as
    /// a [`StreamFrame::JobEvent`] — so the REPL learns a daemon-owned job's
    /// transitions WITHOUT polling. Fire-and-return, like [`Self::JournalEmit`]
    /// (no Subscribe upgrade). Additive: an older daemon rejects the unknown
    /// method; the runtime treats the error as "no relay" and continues.
    JobEventEmit {
        event: DaemonJobEvent,
    },
    Shutdown,
}

/// One frame in the daemon→REPL stream opened by [`Request::Subscribe`].
/// A tagged enum so the daemon can multiplex two flows on the single
/// stream connection: the journal envelope fanout AND the MCP proxy
/// request/reply (Option A — the daemon owns `orkia.sock` but RFC/KG
/// dispatch is REPL-resident). New frame kinds — heartbeats, catch-up
/// markers — can be added additively without breaking the decode.
// The `Envelope` variant dominates the enum size (a full `JournalEnvelope`),
// but `StreamFrame` is a per-frame allocation written/read one at a time on
// the subscribe stream — never stored in a collection — so the unused stack
// headroom in the smaller `McpProxy` frames is never paid in a hot path.
// Same rationale as `Request::Spawn` above; boxing would only add a heap
// alloc per streamed envelope.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub(super) enum StreamFrame {
    /// A journal envelope fanned out by the daemon-resident hub. The REPL
    /// fires its live handlers (router/printer/envelope-hook) and queues
    /// it for the main-loop drain (approval, oneshot, sink, store).
    Envelope { envelope: JournalEnvelope },
    /// A JSON-RPC MCP frame received on `orkia.sock` that must be
    /// dispatched against REPL-resident RFC/KG state. The REPL replies
    /// with [`Request::McpProxyReply`] carrying the same `corr_id`.
    McpProxy {
        corr_id: u64,
        line: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        peer_job_id: Option<u32>,
    },
    /// A daemon-owned job's lifecycle transition (spawn / state change / exit /
    /// attention), pushed so a subscribed REPL learns it WITHOUT polling
    /// `List`/`Inspect`. The REPL maps this back onto its in-process `JobEvent`
    /// channel so the existing drains (lifecycle envelope, state machine,
    /// attention) run unchanged. Daemon does not emit it yet (wired in 3b);
    /// the REPL pump skips it until 3c.
    JobEvent { event: DaemonJobEvent },
}

/// Wire-stable projection of the REPL's `orkia_shell_types::JobEvent` for a
/// daemon-owned job. Flat, String-tagged (consistent with `DaemonStageInfo`),
/// every optional `skip_serializing_if` so the frame stays additive: an older
/// REPL that cannot decode it skips the whole frame (the `Err => continue`
/// guard in `journal_pump::reader_loop`, #7), an older daemon never sends it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct DaemonJobEvent {
    pub job_id: u32,
    /// One of: `spawned` | `attached` | `detached` | `stopped` | `continued`
    /// | `completed`. Unknown tags are tolerated (REPL maps only what it knows).
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct Response {
    pub(super) version: u16,
    pub(super) ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) status: Option<DaemonStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) job: Option<DaemonJobInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) jobs: Vec<DaemonJobInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) logs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DaemonStatus {
    pub state: String,
    pub protocol_version: u16,
    pub pid: Option<u32>,
    pub socket: String,
    pub jobs: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DaemonJobInfo {
    pub id: u32,
    pub agent: String,
    pub state: String,
    pub pid: Option<u32>,
    pub label: String,
    pub runtime_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_socket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pty_owner_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lost_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seal_path: Option<String>,
    #[serde(default)]
    pub attachable: bool,
    #[serde(default)]
    pub stages: Vec<DaemonStageInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DaemonStageInfo {
    #[serde(default)]
    pub id: u32,
    pub target: String,
    pub state: String,
    pub pid: Option<u32>,
    pub runtime_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lost_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub attachable: bool,
}

pub(crate) fn socket_path(data_dir: &Path) -> PathBuf {
    data_dir.join("run").join(SOCKET_NAME)
}

pub(super) fn send_request(stream: &mut UnixStream, req: &Request) -> Result<(), String> {
    let wire = WireRequest {
        version: PROTOCOL_VERSION,
        request: req.clone_for_wire(),
    };
    let mut line = serde_json::to_string(&wire).map_err(|e| format!("serialize request: {e}"))?;
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(|e| format!("write request: {e}"))
}

pub(super) fn write_ok(stream: &mut UnixStream) -> std::io::Result<()> {
    write_response(
        stream,
        Response {
            version: PROTOCOL_VERSION,
            ok: true,
            error: None,
            status: None,
            job: None,
            jobs: Vec::new(),
            logs: Vec::new(),
        },
    )
}

pub(super) fn write_error(stream: &mut UnixStream, error: String) -> std::io::Result<()> {
    write_response(
        stream,
        Response {
            version: PROTOCOL_VERSION,
            ok: false,
            error: Some(error),
            status: None,
            job: None,
            jobs: Vec::new(),
            logs: Vec::new(),
        },
    )
}

pub(super) fn write_response(stream: &mut UnixStream, resp: Response) -> std::io::Result<()> {
    let mut line = serde_json::to_string(&resp)?;
    line.push('\n');
    stream.write_all(line.as_bytes())
}

/// Write one [`StreamFrame`] as an NDJSON line on the daemon→REPL
/// subscribe connection. Used by the daemon's `Subscribe` handler to
/// stream envelopes and MCP proxy requests to the REPL.
pub(super) fn write_stream_frame(
    stream: &mut UnixStream,
    frame: &StreamFrame,
) -> std::io::Result<()> {
    let mut line = serde_json::to_string(frame)?;
    line.push('\n');
    stream.write_all(line.as_bytes())
}

impl Request {
    fn clone_for_wire(&self) -> Self {
        match self {
            Self::Status => Self::Status,
            Self::Spawn {
                command,
                args,
                working_dir,
                agent_name,
                extra_env,
                cage_wrapper,
                stdin_mode,
                initial_stdin_bytes,
                process_group,
                terminal_cols,
                terminal_rows,
                osc133,
            } => Self::Spawn {
                command: command.clone(),
                args: args.clone(),
                working_dir: working_dir.clone(),
                agent_name: agent_name.clone(),
                extra_env: extra_env.clone(),
                cage_wrapper: cage_wrapper.clone(),
                stdin_mode: stdin_mode.clone(),
                initial_stdin_bytes: initial_stdin_bytes.clone(),
                process_group: process_group.clone(),
                terminal_cols: *terminal_cols,
                terminal_rows: *terminal_rows,
                osc133: *osc133,
            },
            Self::List => Self::List,
            Self::Gc => Self::Gc,
            Self::Inspect { id } => Self::Inspect { id: *id },
            Self::Logs { id, limit } => Self::Logs {
                id: *id,
                limit: *limit,
            },
            Self::Tell {
                id,
                target,
                message,
            } => Self::Tell {
                id: *id,
                target: target.clone(),
                message: message.clone(),
            },
            Self::Stop { id } => Self::Stop { id: *id },
            Self::Kill { id, target } => Self::Kill {
                id: *id,
                target: target.clone(),
            },
            Self::Attach {
                id,
                target,
                cols,
                rows,
            } => Self::Attach {
                id: *id,
                target: target.clone(),
                cols: *cols,
                rows: *rows,
            },
            Self::Subscribe => Self::Subscribe,
            Self::JournalEmit { envelope } => Self::JournalEmit {
                envelope: envelope.clone(),
            },
            Self::JobScope { job_id, project } => Self::JobScope {
                job_id: *job_id,
                project: project.clone(),
            },
            Self::McpProxyReply {
                corr_id,
                response,
                accessed_node_ids,
            } => Self::McpProxyReply {
                corr_id: *corr_id,
                response: response.clone(),
                accessed_node_ids: accessed_node_ids.clone(),
            },
            Self::JobEventEmit { event } => Self::JobEventEmit {
                event: event.clone(),
            },
            Self::Shutdown => Self::Shutdown,
        }
    }
}

pub(super) fn agent_labels(command: &str) -> Vec<String> {
    command
        .split('|')
        .filter_map(|stage| {
            let rest = stage.trim_start().strip_prefix('@')?;
            let name: String = rest
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == '-')
                .collect();
            if name.is_empty() { None } else { Some(name) }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        CageWrapperProto, DaemonJobEvent, Request, StreamFrame, WireRequest, agent_labels,
    };
    use orkia_shell::journal::{EventType, JournalEnvelope};

    #[test]
    fn labels_direct_agent() {
        assert_eq!(agent_labels("@faye audit repo"), vec!["faye"]);
    }

    #[test]
    fn labels_agent_pipeline() {
        assert_eq!(agent_labels("@a | @b review"), vec!["a", "b"]);
    }

    #[test]
    fn labels_shell_to_agent_pipeline() {
        assert_eq!(agent_labels("git diff | @sage"), vec!["sage"]);
    }

    /// A minimal `Spawn { command }` JSON (no new fields) must still
    /// deserialize cleanly. This is the backward-compat guarantee:
    /// an old sender talking to a new daemon must work unchanged.
    #[test]
    fn spawn_minimal_no_new_fields_deserializes() {
        let json = r#"{"version":1,"method":"spawn","command":"@faye audit"}"#;
        let wire: WireRequest = serde_json::from_str(json).unwrap();
        match wire.request {
            Request::Spawn {
                command,
                args,
                working_dir,
                agent_name,
                extra_env,
                cage_wrapper,
                stdin_mode,
                initial_stdin_bytes,
                process_group,
                terminal_cols,
                terminal_rows,
                osc133,
            } => {
                assert_eq!(command, "@faye audit");
                assert!(args.is_empty());
                assert!(working_dir.is_none());
                assert!(agent_name.is_none());
                assert!(extra_env.is_empty());
                assert!(cage_wrapper.is_none());
                assert!(stdin_mode.is_none());
                assert!(initial_stdin_bytes.is_empty());
                assert!(process_group.is_none());
                assert!(terminal_cols.is_none());
                assert!(terminal_rows.is_none());
                assert!(!osc133);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    /// A fully-populated `Spawn` round-trips through JSON without
    /// data loss.
    #[test]
    fn spawn_full_round_trips() {
        let original = Request::Spawn {
            command: "@sage review".to_string(),
            args: vec!["--mcp-config".to_string(), "x.json".to_string()],
            working_dir: Some("/home/user/project".to_string()),
            agent_name: Some("sage".to_string()),
            extra_env: vec![
                ("ORKIA_JOB_ID".to_string(), "42".to_string()),
                ("CLAUDE_SYSTEM_PROMPT".to_string(), "be helpful".to_string()),
            ],
            cage_wrapper: Some(CageWrapperProto {
                cage_bin: "orkia-cage".to_string(),
                policy_path: "/etc/orkia/policy.toml".to_string(),
            }),
            stdin_mode: Some("initial_bytes".to_string()),
            initial_stdin_bytes: b"hello world\n".to_vec(),
            process_group: Some("new_session".to_string()),
            terminal_cols: Some(220),
            terminal_rows: Some(50),
            osc133: true,
        };
        let wire = WireRequest {
            version: 1,
            request: original.clone_for_wire(),
        };
        let json = serde_json::to_string(&wire).unwrap();
        let decoded: WireRequest = serde_json::from_str(&json).unwrap();
        match decoded.request {
            Request::Spawn {
                command,
                args,
                working_dir,
                agent_name,
                extra_env,
                cage_wrapper,
                stdin_mode,
                initial_stdin_bytes,
                process_group,
                terminal_cols,
                terminal_rows,
                osc133,
            } => {
                assert_eq!(command, "@sage review");
                assert_eq!(args, vec!["--mcp-config", "x.json"]);
                assert_eq!(working_dir.as_deref(), Some("/home/user/project"));
                assert_eq!(agent_name.as_deref(), Some("sage"));
                assert_eq!(extra_env.len(), 2);
                let cw = cage_wrapper.unwrap();
                assert_eq!(cw.cage_bin, "orkia-cage");
                assert_eq!(cw.policy_path, "/etc/orkia/policy.toml");
                assert_eq!(stdin_mode.as_deref(), Some("initial_bytes"));
                assert_eq!(initial_stdin_bytes, b"hello world\n");
                assert_eq!(process_group.as_deref(), Some("new_session"));
                assert_eq!(terminal_cols, Some(220));
                assert_eq!(terminal_rows, Some(50));
                assert!(osc133);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    /// Serialising a minimal Spawn omits all new fields from the wire
    /// JSON (skip_serializing_if guarantees old daemons won't see them).
    #[test]
    fn spawn_minimal_serializes_without_new_fields() {
        let req = Request::Spawn {
            command: "@faye".to_string(),
            args: Vec::new(),
            working_dir: None,
            agent_name: None,
            extra_env: Vec::new(),
            cage_wrapper: None,
            stdin_mode: None,
            initial_stdin_bytes: Vec::new(),
            process_group: None,
            terminal_cols: None,
            terminal_rows: None,
            osc133: false,
        };
        let wire = WireRequest {
            version: 1,
            request: req,
        };
        let json = serde_json::to_string(&wire).unwrap();
        // Only "command" should appear in the new-field keys.
        assert!(!json.contains("args"));
        assert!(!json.contains("working_dir"));
        assert!(!json.contains("agent_name"));
        assert!(!json.contains("extra_env"));
        assert!(!json.contains("cage_wrapper"));
        assert!(!json.contains("stdin_mode"));
        assert!(!json.contains("initial_stdin_bytes"));
        assert!(!json.contains("process_group"));
        assert!(!json.contains("terminal_cols"));
        assert!(!json.contains("terminal_rows"));
        assert!(!json.contains("osc133"));
        assert!(json.contains(r#""command":"@faye""#));
    }

    /// `Subscribe` carries no fields and round-trips with the `method`
    /// tag only — the discriminant the daemon matches on.
    #[test]
    fn subscribe_round_trips() {
        let wire = WireRequest {
            version: 1,
            request: Request::Subscribe,
        };
        let json = serde_json::to_string(&wire).unwrap();
        assert!(json.contains(r#""method":"subscribe""#));
        let decoded: WireRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded.request, Request::Subscribe));
    }

    /// `JournalEmit` round-trips the embedded envelope without loss.
    #[test]
    fn journal_emit_round_trips() {
        let mut env = JournalEnvelope::now(EventType::Shell);
        env.job_id = Some(7);
        env.event = Some("ShellCommand".to_string());
        let wire = WireRequest {
            version: 1,
            request: Request::JournalEmit {
                envelope: env.clone(),
            },
        };
        let json = serde_json::to_string(&wire).unwrap();
        assert!(json.contains(r#""method":"journal_emit""#));
        let decoded: WireRequest = serde_json::from_str(&json).unwrap();
        match decoded.request {
            Request::JournalEmit { envelope } => {
                assert_eq!(envelope.job_id, Some(7));
                assert_eq!(envelope.event.as_deref(), Some("ShellCommand"));
                assert_eq!(envelope.event_type, EventType::Shell);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    /// `JobScope` round-trips both the set form (`project = Some`) and
    /// the clear form (`project = None`, omitted from the wire JSON).
    #[test]
    fn job_scope_round_trips_set_and_clear() {
        let set = WireRequest {
            version: 1,
            request: Request::JobScope {
                job_id: 3,
                project: Some("orkia".to_string()),
            },
        };
        let json = serde_json::to_string(&set).unwrap();
        assert!(json.contains(r#""method":"job_scope""#));
        assert!(json.contains(r#""project":"orkia""#));
        let decoded: WireRequest = serde_json::from_str(&json).unwrap();
        match decoded.request {
            Request::JobScope { job_id, project } => {
                assert_eq!(job_id, 3);
                assert_eq!(project.as_deref(), Some("orkia"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }

        let clear = WireRequest {
            version: 1,
            request: Request::JobScope {
                job_id: 3,
                project: None,
            },
        };
        let json = serde_json::to_string(&clear).unwrap();
        assert!(!json.contains("project"));
        let decoded: WireRequest = serde_json::from_str(&json).unwrap();
        match decoded.request {
            Request::JobScope { job_id, project } => {
                assert_eq!(job_id, 3);
                assert!(project.is_none());
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    /// An `Envelope` stream frame round-trips its envelope — this is the
    /// daemon→REPL hot-path payload.
    #[test]
    fn stream_frame_envelope_round_trips() {
        let mut env = JournalEnvelope::now(EventType::Hook);
        env.event = Some("AgentFinalResponse".to_string());
        env.response_path = Some("/home/u/.orkia/agents/a/jobs/1/final-response.md".to_string());
        let frame = StreamFrame::Envelope {
            envelope: env.clone(),
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains(r#""frame":"envelope""#));
        let decoded: StreamFrame = serde_json::from_str(&json).unwrap();
        match decoded {
            StreamFrame::Envelope { envelope } => {
                assert_eq!(envelope.event.as_deref(), Some("AgentFinalResponse"));
                assert_eq!(
                    envelope.response_path.as_deref(),
                    Some("/home/u/.orkia/agents/a/jobs/1/final-response.md")
                );
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    /// An `McpProxy` stream frame round-trips the JSON-RPC line + peer id —
    /// the daemon→REPL half of the MCP proxy (Option A).
    #[test]
    fn stream_frame_mcp_proxy_round_trips() {
        let frame = StreamFrame::McpProxy {
            corr_id: 99,
            line: r#"{"jsonrpc":"2.0","id":1,"method":"orkia_rfc_ask","params":{}}"#.to_string(),
            peer_job_id: Some(42),
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains(r#""frame":"mcp_proxy""#));
        let decoded: StreamFrame = serde_json::from_str(&json).unwrap();
        match decoded {
            StreamFrame::McpProxy {
                corr_id,
                line,
                peer_job_id,
            } => {
                assert_eq!(corr_id, 99);
                assert!(line.contains("orkia_rfc_ask"));
                assert_eq!(peer_job_id, Some(42));
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    /// A `JobEvent` stream frame round-trips a terminal (`completed`) event
    /// with its exit code — the daemon→REPL push that replaces `List`/`Inspect`
    /// polling for a daemon-owned job (3a contract; emitter lands in 3b).
    #[test]
    fn stream_frame_job_event_round_trips() {
        let frame = StreamFrame::JobEvent {
            event: DaemonJobEvent {
                job_id: 1,
                event: "completed".to_string(),
                kind: Some("claude".to_string()),
                pid: Some(4242),
                exit_code: Some(0),
                label: Some("@sage review src/auth.rs".to_string()),
            },
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains(r#""frame":"job_event""#));
        let decoded: StreamFrame = serde_json::from_str(&json).unwrap();
        match decoded {
            StreamFrame::JobEvent { event } => {
                assert_eq!(event.job_id, 1);
                assert_eq!(event.event, "completed");
                assert_eq!(event.kind.as_deref(), Some("claude"));
                assert_eq!(event.pid, Some(4242));
                assert_eq!(event.exit_code, Some(0));
                assert_eq!(event.label.as_deref(), Some("@sage review src/auth.rs"));
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    /// A minimal `JobEvent` (only the two required fields) omits every optional
    /// from the wire JSON and still decodes — proves the additive contract is
    /// not load-bearing on the optionals.
    #[test]
    fn stream_frame_job_event_omits_empty_optionals() {
        let frame = StreamFrame::JobEvent {
            event: DaemonJobEvent {
                job_id: 9,
                event: "spawned".to_string(),
                kind: None,
                pid: None,
                exit_code: None,
                label: None,
            },
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(!json.contains("kind"));
        assert!(!json.contains("pid"));
        assert!(!json.contains("exit_code"));
        assert!(!json.contains("label"));
        let decoded: StreamFrame = serde_json::from_str(&json).unwrap();
        match decoded {
            StreamFrame::JobEvent { event } => {
                assert_eq!(event.job_id, 9);
                assert_eq!(event.event, "spawned");
                assert!(event.kind.is_none());
                assert!(event.pid.is_none());
                assert!(event.exit_code.is_none());
                assert!(event.label.is_none());
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    /// Back-compat: an OLD REPL whose `StreamFrame` predates the `job_event`
    /// arm sees the frame as an unknown variant and serde returns an error —
    /// which `journal_pump::reader_loop` turns into a skipped frame (#7), so an
    /// old REPL drops daemon job pushes instead of mis-decoding them.
    #[test]
    fn job_event_frame_is_unknown_to_old_repl() {
        #[derive(serde::Deserialize)]
        #[serde(tag = "frame", rename_all = "snake_case")]
        #[allow(dead_code)]
        enum OldStreamFrame {
            Envelope { envelope: JournalEnvelope },
        }
        let json = r#"{"frame":"job_event","event":{"job_id":1,"event":"completed"}}"#;
        let res: Result<OldStreamFrame, _> = serde_json::from_str(json);
        assert!(res.is_err());
    }

    /// `McpProxyReply` round-trips the response line + served KG node ids —
    /// the REPL→daemon half of the MCP proxy.
    #[test]
    fn mcp_proxy_reply_round_trips() {
        let reply = Request::McpProxyReply {
            corr_id: 99,
            response: r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#.to_string(),
            accessed_node_ids: vec!["node-a".to_string(), "node-b".to_string()],
        };
        let wire = WireRequest {
            version: 1,
            request: reply.clone_for_wire(),
        };
        let json = serde_json::to_string(&wire).unwrap();
        assert!(json.contains(r#""method":"mcp_proxy_reply""#));
        let decoded: WireRequest = serde_json::from_str(&json).unwrap();
        match decoded.request {
            Request::McpProxyReply {
                corr_id,
                response,
                accessed_node_ids,
            } => {
                assert_eq!(corr_id, 99);
                assert!(response.contains(r#""ok":true"#));
                assert_eq!(accessed_node_ids, vec!["node-a", "node-b"]);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    /// The empty `accessed_node_ids` (common RFC-method case) is omitted
    /// from the wire JSON.
    #[test]
    fn mcp_proxy_reply_omits_empty_node_ids() {
        let wire = WireRequest {
            version: 1,
            request: Request::McpProxyReply {
                corr_id: 1,
                response: "{}".to_string(),
                accessed_node_ids: Vec::new(),
            },
        };
        let json = serde_json::to_string(&wire).unwrap();
        assert!(!json.contains("accessed_node_ids"));
    }

    /// Back-compat: an old daemon (one that predates the subscribe frames) sees
    /// `{"method":"subscribe"}` as an unknown variant. serde must return
    /// an error rather than silently mis-decoding — the REPL relies on
    /// this to detect a stale daemon and fall back to owning the socket.
    #[test]
    fn unknown_method_is_a_decode_error() {
        // Simulate an OLD Request enum that has no `subscribe` arm.
        #[derive(serde::Deserialize)]
        #[serde(tag = "method", rename_all = "snake_case")]
        #[allow(dead_code)]
        enum OldRequest {
            Status,
            List,
        }
        let json = r#"{"method":"subscribe"}"#;
        let res: Result<OldRequest, _> = serde_json::from_str(json);
        assert!(res.is_err());
    }
}
