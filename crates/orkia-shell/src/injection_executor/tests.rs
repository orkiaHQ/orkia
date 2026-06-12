use super::*;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Instant;

/// In-memory `Write` impl whose contents the test can inspect.
/// `Box<dyn Write + Send>` is what `SharedWriter` unboxes to.
struct Sink {
    buf: Arc<Mutex<Vec<u8>>>,
}

impl Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.buf.lock().extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn make_writer() -> (SharedWriter, Arc<Mutex<Vec<u8>>>) {
    let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Sink {
        buf: Arc::clone(&buf),
    };
    let w: SharedWriter = Arc::new(Mutex::new(Box::new(sink) as Box<dyn Write + Send>));
    (w, buf)
}

fn wait_until(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if cond() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    cond()
}

/// What one injected body must look like on the wire: bracketed-paste
/// wrapped (so the agent's TUI inserts it literally — a leading `#`
/// must never open claude's memory dialog), with the submit `\r` as a
/// keystroke OUTSIDE the markers.
fn pasted(body: &str) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"\x1b[200~");
    v.extend_from_slice(body.as_bytes());
    v.extend_from_slice(b"\x1b[201~\r");
    v
}

#[test]
fn inject_writes_payload_with_trailing_cr() {
    let exec = InjectionExecutor::spawn();
    let (writer, buf) = make_writer();
    exec.register(JobId(1), writer, None);
    exec.inject(JobId(1), "faye", "hi");
    assert!(
        wait_until(Duration::from_secs(2), || *buf.lock() == pasted("hi")),
        "expected paste-wrapped 'hi' + CR, got {:?}",
        buf.lock(),
    );
}

#[test]
fn codex_inject_uses_cr_submit() {
    let exec = InjectionExecutor::spawn();
    let (writer, buf) = make_writer();
    exec.register(JobId(11), writer, None);
    exec.inject(JobId(11), "codex", "hi");
    assert!(
        wait_until(Duration::from_secs(2), || *buf.lock() == pasted("hi")),
        "expected codex CR Enter, got {:?}",
        buf.lock(),
    );
}

#[test]
fn held_injection_defers_until_release() {
    let exec = InjectionExecutor::spawn();
    let (writer, buf) = make_writer();
    exec.register(JobId(4), writer, None);
    exec.hold(JobId(4));
    exec.inject(JobId(4), "faye", "wait");
    thread::sleep(Duration::from_millis(80));
    assert!(buf.lock().is_empty());
    exec.release(JobId(4));
    assert!(
        wait_until(Duration::from_secs(2), || *buf.lock() == pasted("wait")),
        "expected deferred prompt after release, got {:?}",
        buf.lock(),
    );
}

#[test]
fn inject_for_unregistered_job_is_silent_noop() {
    let exec = InjectionExecutor::spawn();
    exec.inject(JobId(99), "phantom", "should-disappear");
    thread::sleep(Duration::from_millis(50));
}

#[test]
fn unregister_then_inject_is_noop() {
    let exec = InjectionExecutor::spawn();
    let (writer, buf) = make_writer();
    exec.register(JobId(2), writer, None);
    exec.unregister(JobId(2));
    exec.inject(JobId(2), "faye", "ignored");
    thread::sleep(Duration::from_millis(80));
    assert!(
        buf.lock().is_empty(),
        "expected empty, got {:?}",
        buf.lock()
    );
}

#[test]
fn multiple_injects_preserve_order() {
    let exec = InjectionExecutor::spawn();
    let (writer, buf) = make_writer();
    exec.register(JobId(3), writer, None);
    exec.inject(JobId(3), "faye", "a");
    exec.inject(JobId(3), "faye", "b");
    assert!(
        wait_until(Duration::from_secs(2), || *buf.lock()
            == [pasted("a"), pasted("b")].concat()),
        "expected pasted 'a' then 'b', got {:?}",
        buf.lock(),
    );
}

fn probe_returning(grid: &'static [u8]) -> GridProbe {
    Arc::new(move || grid.to_vec())
}

#[test]
fn confirm_matches_body_echoed_into_a_styled_grid() {
    let grid = b"\x1b[2J\x1b[H\x1b[0m\x1b[38;5;7m\xe2\x9d\xaf \x1b[1msay helloooo\x1b[0m   \r\n";
    assert!(confirm_in_grid(&probe_returning(grid), "say helloooo"));
}

#[test]
fn confirm_fails_when_body_absent_from_grid() {
    let grid = b"\x1b[2J\x1b[Hfake-agent ready\r\n";
    let start = Instant::now();
    assert!(!confirm_in_grid(&probe_returning(grid), "DELIVER-XYZ"));
    assert!(start.elapsed() >= CONFIRM_TIMEOUT);
}

#[test]
fn confirm_accepts_collapsed_paste_placeholder_for_multiline_body() {
    // claude renders a large multi-line paste as a placeholder instead
    // of echoing the content — that placeholder IS the landing proof.
    let grid = b"\xe2\x9d\xaf [Pasted text #1 +42 lines]\r\n";
    assert!(confirm_in_grid(
        &probe_returning(grid),
        "# Implementation: staged rollout\n\nline two"
    ));
}

#[test]
fn confirm_rejects_paste_placeholder_for_single_line_body() {
    // A stale placeholder from an earlier paste must not confirm a
    // single-line body that claude would have echoed verbatim.
    let grid = b"\xe2\x9d\xaf [Pasted text #1 +42 lines]\r\n";
    assert!(!confirm_in_grid(&probe_returning(grid), "DELIVER-XYZ"));
}

#[test]
fn confirmed_landing_emits_delivered_after_submit() {
    let (tx, rx) = mpsc::channel::<DetectorEvent>();
    let exec = InjectionExecutor::spawn_with_delivery(tx);
    let (writer, buf) = make_writer();
    let probe = probe_returning(b"\xe2\x9d\xaf say bye\r\n");
    exec.register(JobId(7), writer, Some(probe));
    exec.inject(JobId(7), "faye", "say bye");
    match rx.recv_timeout(Duration::from_secs(3)) {
        Ok(DetectorEvent::Delivered {
            job_id,
            agent_name,
            body,
        }) => {
            assert_eq!(job_id, JobId(7));
            assert_eq!(agent_name, "faye");
            assert_eq!(body, "say bye");
            assert_eq!(*buf.lock(), pasted("say bye"));
        }
        other => panic!("expected Delivered after a confirmed landing, got {other:?}"),
    }
}

#[test]
fn no_delivery_channel_means_no_delivered_event() {
    let exec = InjectionExecutor::spawn();
    let (writer, buf) = make_writer();
    exec.register(JobId(8), writer, None);
    exec.inject(JobId(8), "faye", "hi");
    assert!(wait_until(Duration::from_secs(2), || *buf.lock() == pasted("hi")));
}

#[test]
fn normalize_strips_ansi_and_whitespace_lowercases() {
    let s = normalize(b"\x1b[1m  Say\tHELLO \x1b[0m\r\n");
    assert_eq!(s, "sayhello");
}

#[test]
fn strip_ansi_drops_csi_and_osc_keeps_text() {
    assert_eq!(strip_ansi(b"a\x1b[31mb\x1b]0;title\x07c"), "abc");
    assert_eq!(strip_ansi(b"x\x1b[999"), "x");
}

#[test]
fn confirm_menu_detected_for_trust_dialog_not_for_input_box() {
    let trust = b"Is this a project you trust?\r\n  1. Yes, I trust this folder\r\n  2. No, exit\r\n Enter to confirm \xc2\xb7 Esc to cancel\r\n";
    assert!(grid_shows_confirm_menu(&probe_returning(trust)));
    let box_ = b"\x1b[2J\x1b[H\xe2\x9d\xaf Try \"fix the build\"\r\n";
    assert!(!grid_shows_confirm_menu(&probe_returning(box_)));
}
