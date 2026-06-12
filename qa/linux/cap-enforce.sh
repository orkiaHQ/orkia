#!/usr/bin/env bash
# ============================================================================
# SPEC-ORKIA-CAP — Linux runtime enforcement gate
# ============================================================================
# Asserts that the per-agent capability classes are *actually enforced* by the
# real cage (mount namespace) + real `orkia-sh` shim — not just unit-tested:
#
#   read=false   → workspace omitted (ENOENT), agent cwd = /
#   write=false  → workspace mounted read-only (EROFS on write)
#   exec=false   → every command denied by the shim ("exec class disabled")
#
# Run on a Linux host with unprivileged user namespaces (the Lima `orkia-cage`
# VM, or a GitHub ubuntu-latest runner). Skips cleanly (exit 0) where userns is
# unavailable — the cage cannot run there and that is not a cap regression.
#
#   qa/linux/cap-enforce.sh            # build release cage/sh, then assert
#   SKIP_BUILD=1 qa/linux/cap-enforce.sh   # reuse already-built target/release
# ============================================================================
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ORKIA_DIR="$(cd "$HERE/../.." && pwd)"
cd "$ORKIA_DIR"

# --- userns precondition: skip (not fail) if the kernel won't allow it --------
if ! unshare -Urm true 2>/dev/null; then
  echo "SKIP: unprivileged user namespaces unavailable — cannot run the cage here."
  exit 0
fi

# --- build -------------------------------------------------------------------
if [ "${SKIP_BUILD:-0}" != "1" ]; then
  cargo build --release --bin orkia-cage --bin orkia-sh
fi
CAGE="$ORKIA_DIR/target/release/orkia-cage"
[ -x "$CAGE" ] || { echo "FAIL: $CAGE not built"; exit 1; }

WS="$(mktemp -d)/ws"; mkdir -p "$WS"; echo "SECRET" > "$WS/existing.txt"
POL="$(mktemp)"
fails=0
pol() { printf 'default_verdict = "allow"\n\n[caps]\nread = %s\nwrite = %s\nexec = %s\n\n[workspace]\nroot = "%s"\n' "$1" "$2" "$3" "$WS" > "$POL"; }
cage() { "$CAGE" --policy "$POL" -- bash -c "$1" >/tmp/cap_out 2>/dev/null; echo $?; }
check() { # $1=label $2=expected $3=actual
  if [ "$2" = "$3" ]; then echo "  ok: $1"; else echo "  FAIL: $1 (want $2, got $3)"; fails=$((fails+1)); fi
}

# The cage's Sprint-6 verdict tap fail-closes on the `allow` path unless the
# journal socket is served (a real `orkia` session does this). Stand up a
# draining listener so allowed commands actually run and we can observe the
# kernel mount behaviour. `deny` uses a best-effort path and needs no listener.
SOCK_DIR="$HOME/.orkia/run"; mkdir -p "$SOCK_DIR"
python3 - "$SOCK_DIR/orkia.sock" <<'PY' &
import socket, os, sys
p = sys.argv[1]
try: os.unlink(p)
except FileNotFoundError: pass
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.bind(p); s.listen(64)
while True:
    c, _ = s.accept()
    while c.recv(65536): pass
    c.close()
PY
LPID=$!
sleep 1

echo "T1 read=t write=t exec=t"
pol true true true
check "read succeeds"  0 "$(cage 'cat existing.txt')"
check "write succeeds" 0 "$(cage 'echo HI > newfile.txt')"

echo "T2 read=t write=f exec=t (read-only)"
pol true false true
check "read succeeds" 0 "$(cage 'cat existing.txt')"
rc=$(cage 'echo HI > blocked.txt'); [ "$rc" != "0" ] && echo "  ok: write blocked (rc=$rc, EROFS)" || { echo "  FAIL: write was NOT blocked"; fails=$((fails+1)); }

echo "T3 read=f (workspace absent)"
pol false false true
rc=$(cage "ls $WS"); [ "$rc" != "0" ] && echo "  ok: workspace ENOENT (rc=$rc)" || { echo "  FAIL: workspace still visible"; fails=$((fails+1)); }

echo "T4 exec=f (class closed)"
pol true true false
check "exec denied (126)" 126 "$(cage 'echo nope')"

echo "leak check (host workspace)"
[ -f "$WS/newfile.txt" ] && echo "  ok: allowed write persisted" || { echo "  FAIL: allowed write missing"; fails=$((fails+1)); }
[ ! -f "$WS/blocked.txt" ] && echo "  ok: blocked write never landed" || { echo "  FAIL: read-only write leaked"; fails=$((fails+1)); }

kill $LPID 2>/dev/null
if [ "$fails" -eq 0 ]; then echo "PASS: cap enforcement verified on real Linux"; exit 0; fi
echo "FAIL: $fails assertion(s) failed"; exit 1
