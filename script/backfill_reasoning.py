#!/usr/bin/env python3
# Copyright 2026 Orkia
# SPDX-License-Identifier: Elastic-2.0
#
# This file is part of the public Orkia shell. Licensed under the
# Elastic License 2.0 — see the top-level LICENSE file for
# the Elastic License 2.0 terms.
#
"""Backfill the Orkia reasoning graph from local Claude Code transcripts.

This parser turns every Orkia-related Claude session under
``~/.claude/projects`` into a stream of `JournalEnvelope` JSON lines — the
*exact* wire type the live hot path consumes. It does NOT touch the reasoning
store itself: it emits a normalized `.jsonl`, then (unless ``--no-ingest``)
shells out to ``orkia reasoning backfill <file>``, which replays the envelopes
through the real `ReasoningConsumer` (same scrub, hash, enum encoding, session
bookkeeping as live capture — one owner of the encoding contract) and flushes
the staged turns to the cloud.

Why a synthetic envelope stream instead of reading the DB directly? Because the
consumer is the single source of truth for how a turn is shaped. Re-deriving
that here would be a second, drifting implementation. We feed it; we don't
reimplement it.

Corpus filter (locked design): a project dir is in scope when its encoded name
contains ``orkia`` (case-insensitive) and does not contain ``obelisk`` /
``oeblisk`` (a different product). Use ``--exclude SUBSTR`` to prune further and
``--list`` to preview the matched dirs without writing anything.

Idempotency note: the consumer assigns a fresh event id per turn, so running
the ingest step twice DOUBLES the rows. This is a one-shot bulk import — run it
once. Re-run only the parse step (``--no-ingest``) freely; it writes nothing to
the store.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path

# Tools whose primary argument is a file path; used to populate the envelope
# `target` (and the consumer's turn metadata) with something meaningful.
FILE_TOOLS = {"Read", "Write", "Edit", "NotebookEdit", "MultiEdit"}

# Cap raw text per turn. The consumer scrubs and summarizes, but there is no
# reason to ship a 200KB tool result into the graph; the summary is what gets
# consolidated. Keeps the staged store and the cloud push lean.
MAX_RAW = 4000


def encoded_dirs(projects: Path) -> list[Path]:
    """Every project dir under ``projects`` (each is one encoded cwd path)."""
    if not projects.is_dir():
        return []
    return sorted(p for p in projects.iterdir() if p.is_dir())


def in_corpus(name: str, excludes: list[str]) -> bool:
    """Locked filter: name has 'orkia', not 'obelisk'/'oeblisk', not excluded."""
    low = name.lower()
    if "orkia" not in low:
        return False
    if "obelisk" in low or "oeblisk" in low:
        return False
    return all(x.lower() not in low for x in excludes)


def select_dirs(projects: Path, excludes: list[str]) -> list[Path]:
    return [d for d in encoded_dirs(projects) if in_corpus(d.name, excludes)]


def truncate(text: str) -> str:
    if len(text) <= MAX_RAW:
        return text
    return text[:MAX_RAW] + f"\n…[truncated {len(text) - MAX_RAW} chars]"


def block_text(content) -> str:
    """Flatten a message `content` (str or list of blocks) to its text."""
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return ""
    parts = []
    for b in content:
        if isinstance(b, dict) and b.get("type") == "text":
            parts.append(b.get("text", ""))
    return "\n".join(p for p in parts if p)


def tool_result_text(block: dict) -> str:
    """Extract displayable text from a tool_result content block."""
    content = block.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = []
        for c in content:
            if isinstance(c, dict) and c.get("type") == "text":
                parts.append(c.get("text", ""))
        return "\n".join(p for p in parts if p)
    return ""


def tool_target(name: str, inp: dict) -> str | None:
    """A human-meaningful `target` for a tool_use: file path or command."""
    if name in FILE_TOOLS:
        return inp.get("file_path") or inp.get("path") or inp.get("notebook_path")
    if name == "Bash":
        cmd = inp.get("command", "")
        return cmd if len(cmd) <= 200 else cmd[:200] + " …"
    if name in {"Grep", "Glob"}:
        return inp.get("pattern")
    return None


def env_line(job_id: int, ts: str, event: str, **fields) -> dict:
    """Build one JournalEnvelope dict. `type`=hook, source/agent=claude."""
    out = {
        "type": "hook",
        "timestamp": ts,
        "job_id": job_id,
        "source": "claude",
        "agent": "claude",
        "event": event,
    }
    for k, v in fields.items():
        if v is not None and v != "":
            out[k] = v
    return out


def parse_transcript(path: Path, job_id: int) -> list[dict]:
    """One transcript file → a list of JournalEnvelope dicts for one session.

    Every byte here is untrusted: malformed JSON lines are skipped, unexpected
    shapes degrade to empty text rather than raising.
    """
    rows: list[dict] = []
    with path.open(encoding="utf-8", errors="replace") as fh:
        for line in fh:
            line = line.strip()
            if line:
                obj = _loads(line)
                if obj is not None:
                    rows.append(obj)
    events = [r for r in rows if r.get("type") in ("user", "assistant") and r.get("timestamp")]
    if not events:
        return []

    first_ts = events[0]["timestamp"]
    last_ts = events[-1]["timestamp"]
    out: list[dict] = [env_line(job_id, first_ts, "SessionStart")]
    tool_names: dict[str, str] = {}  # tool_use_id -> tool name (for PostToolUse)

    for ev in events:
        ts = ev.get("timestamp") or first_ts
        msg = ev.get("message") or {}
        content = msg.get("content")
        if ev["type"] == "user":
            out.extend(_user_envelopes(job_id, ts, content, tool_names))
        else:
            out.extend(_assistant_envelopes(job_id, ts, content, tool_names))

    out.append(env_line(job_id, last_ts, "SessionEnd"))
    return out


def _loads(line: str):
    try:
        return json.loads(line)
    except (json.JSONDecodeError, ValueError):
        return None


def _user_envelopes(job_id, ts, content, tool_names) -> list[dict]:
    """A user line is either a typed prompt (str/text) or tool_result blocks."""
    if isinstance(content, str):
        text = content.strip()
        return [env_line(job_id, ts, "UserPromptSubmit", prompt=truncate(text))] if text else []
    if not isinstance(content, list):
        return []
    out = []
    typed = block_text(content).strip()
    if typed:
        out.append(env_line(job_id, ts, "UserPromptSubmit", prompt=truncate(typed)))
    for b in content:
        if isinstance(b, dict) and b.get("type") == "tool_result":
            name = tool_names.get(b.get("tool_use_id"), "unknown")
            preview = truncate(tool_result_text(b).strip())
            out.append(env_line(job_id, ts, "PostToolUse", tool=name, response_preview=preview))
    return out


def _assistant_envelopes(job_id, ts, content, tool_names) -> list[dict]:
    """An assistant line carries text (→Stop) and/or tool_use (→PreToolUse)."""
    if not isinstance(content, list):
        text = block_text(content).strip()
        return [env_line(job_id, ts, "Stop", message=truncate(text))] if text else []
    out = []
    text = block_text(content).strip()
    if text:
        out.append(env_line(job_id, ts, "Stop", message=truncate(text)))
    for b in content:
        if isinstance(b, dict) and b.get("type") == "tool_use":
            name = b.get("name", "unknown")
            tid = b.get("id")
            if tid:
                tool_names[tid] = name
            inp = b.get("input") or {}
            target = tool_target(name, inp)
            desc = inp.get("description") if isinstance(inp, dict) else None
            out.append(
                env_line(job_id, ts, "PreToolUse", tool=name, target=target, message=desc)
            )
    return out


def build_corpus(dirs: list[Path], out_path: Path) -> tuple[int, int, int]:
    """Parse every transcript in `dirs` into one envelope `.jsonl`.

    Returns (sessions, transcripts_seen, envelopes_written).
    """
    job_id = 0
    transcripts = 0
    envelopes = 0
    with out_path.open("w", encoding="utf-8") as out:
        for d in dirs:
            for path in sorted(d.glob("*.jsonl")):
                transcripts += 1
                job_id += 1
                rows = parse_transcript(path, job_id)
                # A session is only worth emitting if it produced at least one
                # turn (start+end alone carry no graph signal).
                if len(rows) <= 2:
                    continue
                for r in rows:
                    out.write(json.dumps(r, ensure_ascii=False) + "\n")
                    envelopes += 1
    sessions = job_id  # job_id counter == sessions assigned
    return sessions, transcripts, envelopes


def run_ingest(binary: str, out_path: Path, no_push: bool) -> int:
    cmd = [binary, "reasoning", "backfill", str(out_path)]
    if no_push:
        cmd.append("--no-push")
    print(f"\n→ {' '.join(cmd)}", file=sys.stderr)
    try:
        return subprocess.run(cmd, check=False).returncode
    except FileNotFoundError:
        print(
            f"backfill_reasoning: '{binary}' not found on PATH; "
            f"the envelope corpus is written at {out_path}. "
            f"Run `orkia reasoning backfill {out_path}` yourself, or pass "
            f"--binary <path-to-orkia>.",
            file=sys.stderr,
        )
        return 127


def parse_args(argv: list[str]) -> argparse.Namespace:
    p = argparse.ArgumentParser(
        prog="backfill_reasoning.py",
        description="Stage local Claude transcripts into the Orkia reasoning graph.",
    )
    default_projects = Path(os.path.expanduser("~/.claude/projects"))
    p.add_argument(
        "--projects", type=Path, default=default_projects,
        help="Claude projects root (default: ~/.claude/projects).",
    )
    p.add_argument(
        "--out", type=Path, default=Path("/tmp/orkia-backfill-envelopes.jsonl"),
        help="Where to write the normalized envelope corpus.",
    )
    p.add_argument(
        "--exclude", action="append", default=[], metavar="SUBSTR",
        help="Drop project dirs whose name contains SUBSTR (repeatable).",
    )
    p.add_argument(
        "--list", action="store_true",
        help="List matched project dirs (+ transcript counts) and exit.",
    )
    p.add_argument(
        "--no-ingest", action="store_true",
        help="Only write the envelope corpus; do not call `orkia reasoning backfill`.",
    )
    p.add_argument(
        "--no-push", action="store_true",
        help="Pass --no-push to the ingest step (stage locally, do not sync).",
    )
    p.add_argument(
        "--binary", default="orkia",
        help="Path to the orkia binary for the ingest step (default: orkia on PATH).",
    )
    return p.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    dirs = select_dirs(args.projects, args.exclude)
    if not dirs:
        print(
            f"backfill_reasoning: no Orkia project dirs under {args.projects} "
            f"(filter: name~'orkia', not 'obelisk').",
            file=sys.stderr,
        )
        return 1

    if args.list:
        print(f"{len(dirs)} project dir(s) in corpus:")
        for d in dirs:
            n = len(list(d.glob("*.jsonl")))
            print(f"  {n:4d}  {d.name}")
        return 0

    print(f"parsing {len(dirs)} project dir(s) → {args.out}", file=sys.stderr)
    sessions, transcripts, envelopes = build_corpus(dirs, args.out)
    print(
        f"wrote {envelopes} envelope(s) from {transcripts} transcript(s) "
        f"({sessions} session id(s)) → {args.out}",
        file=sys.stderr,
    )
    if envelopes == 0:
        print("backfill_reasoning: no envelopes produced; nothing to ingest.", file=sys.stderr)
        return 1
    if args.no_ingest:
        print("--no-ingest: corpus written, store untouched.", file=sys.stderr)
        return 0
    return run_ingest(args.binary, args.out, args.no_push)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
