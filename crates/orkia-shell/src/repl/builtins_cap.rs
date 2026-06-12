// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! `cap` is the `ls -l`/`chmod` of agents: it shows and sets the coarse
//! [`ClassCaps`] master switches each agent runs under. Every bit it shows is a
//! bit the system enforces — `read`/`write` by the cage mount, `exec` by the
//! shim. The `spawn`/`reach` **frontier** is shown (`·`) but refused on set: it
//! is vocabulary with no enforcement layer yet, so it can never be persisted.
//!
//! Storage is per-agent (`<agent.dir>/policy.toml`); a mutation reads the
//! agent's own policy (seeded from the global `[cage].policy` when it has none),
//! flips the `[caps]` bits, and writes it back — siblings (`default_verdict`,
//! `workspace`, `capabilities`) preserved by round-tripping the typed [`Policy`].

use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use orkia_shell_types::{
    AgentInfo, BlockContent, CellStyle, ClassCaps, Outcome, Policy, StyledCell, Verdict,
    WorkspaceScope,
};

use super::*;

/// The reserved frontier classes — displayed, never settable.
const FRONTIER: &[&str] = &["spawn", "reach"];

/// Where an agent's effective caps come from — surfaced so the operator knows
/// whether they are looking at the agent's own policy or an inherited one.
#[derive(Clone, Copy)]
enum CapSource {
    Own,
    Inherited,
    Default,
}

impl CapSource {
    fn label(self) -> &'static str {
        match self {
            CapSource::Own => "own policy",
            CapSource::Inherited => "inherited from global [cage].policy",
            CapSource::Default => "no policy — fail-closed default (all off)",
        }
    }
}

impl Repl {
    /// `cap` (grid) · `cap @agent` (detail) · `cap @agent +read -write …` (set).
    pub(crate) fn handle_cap(&mut self, args: &[String]) -> Outcome {
        let Some(first) = args.first() else {
            return Outcome::BuiltinOutput {
                blocks: self.cap_grid(),
            };
        };
        if matches!(first.as_str(), "help" | "--help" | "-h") {
            return Outcome::BuiltinOutput { blocks: cap_help() };
        }
        let name = first.trim_start_matches('@').to_string();
        let ops = &args[1..];
        if ops.is_empty() {
            match self.cap_detail(&name) {
                Ok(blocks) => Outcome::BuiltinOutput { blocks },
                Err(e) => Outcome::Error(e),
            }
        } else if ops.iter().any(|o| o == "--reset") {
            if ops.len() != 1 {
                return Outcome::Error("cap: --reset cannot be combined with +/-class ops".into());
            }
            self.cap_reset(&name)
        } else {
            self.cap_mutate(&name, ops)
        }
    }

    /// The grid: one row per agent. `AGENT ARCH TRUST` then the `domain`
    /// (R W X) and `frontier` (S N) glyph groups, then the `MODE` summary.
    /// Granted classes are `●`, denied `○`, the frontier `·` (n/a in v1).
    fn cap_grid(&self) -> Vec<BlockContent> {
        if self.agents.is_empty() {
            return vec![BlockContent::SystemInfo("cap: no agents defined".into())];
        }
        let w = self.grid_widths();
        let mut blocks = grid_header_blocks(&w);
        for a in &self.agents {
            let (caps, _) = self.effective_caps(a);
            blocks.push(BlockContent::TableRow(row_cells(
                &a.name,
                &a.archetype,
                &caps,
                &w,
            )));
        }
        blocks.push(grid_footer());
        blocks
    }

    /// Column widths sized to the live agent set (name+`@`, archetype), with
    /// `MODE` fixed to its fixed-content header.
    fn grid_widths(&self) -> GridWidths {
        let agent = self
            .agents
            .iter()
            .map(|a| a.name.chars().count() + 1)
            .max()
            .unwrap_or(6)
            .max(5);
        let arch = self
            .agents
            .iter()
            .map(|a| a.archetype.chars().count())
            .max()
            .unwrap_or(4)
            .max(4);
        GridWidths {
            agent,
            arch,
            mode: 5,
        }
    }

    /// Detail for one agent — MODE plus a line per class with its meaning.
    fn cap_detail(&self, name: &str) -> Result<Vec<BlockContent>, String> {
        let agent = self.find_agent(name)?;
        let (caps, source) = self.effective_caps(agent);
        let rules = self
            .read_agent_policy(agent)
            .map(|p| p.capabilities.len())
            .unwrap_or(0);
        let exec_note = if caps.exec {
            format!("command rules live ({rules} capability rule(s))")
        } else {
            "rules inert — class closed, every command denied".into()
        };
        Ok(vec![
            BlockContent::SystemInfo(format!("cap @{name}  ({})", source.label())),
            BlockContent::Notice {
                style: CellStyle::Accent,
                text: format!(" MODE   {}", mode_string(&caps)),
            },
            class_line("read", caps.read, "workspace visible"),
            class_line("write", caps.write, "workspace writable"),
            class_line("exec", caps.exec, &exec_note),
            BlockContent::SystemInfo(" spawn  ·     reserved — frontier, not settable".into()),
            BlockContent::SystemInfo(" reach  ·     reserved — frontier, not settable".into()),
        ])
    }

    /// Apply `+class` / `-class` ops to the agent's own policy, then persist.
    fn cap_mutate(&mut self, name: &str, ops: &[String]) -> Outcome {
        let parsed = match parse_ops(ops) {
            Ok(p) => p,
            Err(e) => return Outcome::Error(e),
        };
        // Resolve the agent + its policy path BEFORE mutating (fail-closed: an
        // unknown agent or a frontier op aborts without writing anything). The
        // path is `<data_dir>/agents/<name>/policy.toml` — the same path the cage
        // reads at spawn (`resolve_policy_path`), so a `cap` write lands exactly
        // where enforcement looks.
        let path = match self.find_agent(name) {
            Ok(a) => crate::agent_dir::agent_policy_path(&self.config.data_dir, &a.name),
            Err(e) => return Outcome::Error(e),
        };
        // Hold the writer lock across the whole read-modify-write so a concurrent
        // `cap` on the same agent cannot lose this update.
        let _lock = match PolicyLock::acquire(&path) {
            Ok(l) => l,
            Err(e) => return Outcome::Error(e),
        };
        let mut policy = match self.load_or_seed_policy(&path) {
            Ok(p) => p,
            Err(e) => return Outcome::Error(e),
        };
        let before = policy.caps;
        for (on, class) in &parsed {
            match class.as_str() {
                "read" => policy.caps.read = *on,
                "write" => policy.caps.write = *on,
                "exec" => policy.caps.exec = *on,
                other => return Outcome::Error(frontier_or_unknown(other, name)),
            }
        }
        // Invariant: write requires read (you cannot write what you cannot see).
        // Catches both `+write` without read and `-read` with write left on.
        if policy.caps.write && !policy.caps.read {
            return Outcome::Error(format!(
                "cap: write requires read — `cap @{name} +read +write`, or drop write"
            ));
        }
        if let Err(e) = write_policy(&path, &policy) {
            return Outcome::Error(format!("cap: writing {}: {e}", path.display()));
        }
        // is a security-relevant edit and is recorded like `trust.unlock`.
        self.emit_audit_event(
            JobId(0),
            name,
            "cap.set",
            caps_audit(name, before, policy.caps),
        );
        Outcome::BuiltinOutput {
            blocks: vec![
                BlockContent::Notice {
                    style: CellStyle::Good,
                    text: format!("cap @{name}: {}", mode_string(&policy.caps)),
                },
                next_spawn_note(),
            ],
        }
    }

    /// `cap @agent --reset` — drop the per-agent policy so the agent falls back
    /// to the global `[cage].policy` (or the fail-closed default if none).
    fn cap_reset(&mut self, name: &str) -> Outcome {
        let path = match self.find_agent(name) {
            Ok(a) => crate::agent_dir::agent_policy_path(&self.config.data_dir, &a.name),
            Err(e) => return Outcome::Error(e),
        };
        let _lock = match PolicyLock::acquire(&path) {
            Ok(l) => l,
            Err(e) => return Outcome::Error(e),
        };
        if !path.exists() {
            return Outcome::BuiltinOutput {
                blocks: vec![BlockContent::SystemInfo(format!(
                    "cap @{name}: no per-agent policy — already inheriting global"
                ))],
            };
        }
        if let Err(e) = std::fs::remove_file(&path) {
            return Outcome::Error(format!("cap: removing {}: {e}", path.display()));
        }
        self.emit_audit_event(
            JobId(0),
            name,
            "cap.reset",
            serde_json::json!({ "agent": name }),
        );
        Outcome::BuiltinOutput {
            blocks: vec![
                BlockContent::Notice {
                    style: CellStyle::Good,
                    text: format!("cap @{name}: reset — now inherits global [cage].policy"),
                },
                next_spawn_note(),
            ],
        }
    }

    // ── helpers ──────────────────────────────────────────────────────────

    fn find_agent(&self, name: &str) -> Result<&AgentInfo, String> {
        self.agents
            .iter()
            .find(|a| a.name == name)
            .ok_or_else(|| format!("cap: unknown agent '{name}'"))
    }

    /// The caps the agent actually runs under: its own policy if present, else
    /// the global policy it inherits, else the fail-closed default.
    fn effective_caps(&self, agent: &AgentInfo) -> (ClassCaps, CapSource) {
        if let Some(p) = self.read_agent_policy(agent) {
            return (p.caps, CapSource::Own);
        }
        if let Some(p) = self.read_global_policy() {
            return (p.caps, CapSource::Inherited);
        }
        (ClassCaps::default(), CapSource::Default)
    }

    fn read_agent_policy(&self, agent: &AgentInfo) -> Option<Policy> {
        read_policy(&crate::agent_dir::agent_policy_path(
            &self.config.data_dir,
            &agent.name,
        ))
    }

    fn read_global_policy(&self) -> Option<Policy> {
        // Expand `~/` exactly as `cage_wrapper` does, so `cap` reads the same
        // global file the cage enforces — otherwise an inherited policy at a
        // `~/…` path would show as the all-off default while the cage runs it.
        let expanded = super::builders::expand_tilde(self.config.cage.policy.as_deref()?);
        read_policy(Path::new(&expanded))
    }

    /// Load the agent's own policy to mutate; if it has none, seed from the
    /// global policy (so its rules carry over), else a minimal fail-closed one.
    fn load_or_seed_policy(&self, path: &Path) -> Result<Policy, String> {
        if path.exists() {
            return read_policy(path)
                .ok_or_else(|| format!("cap: cannot parse policy {}", path.display()));
        }
        Ok(self.read_global_policy().unwrap_or_else(default_policy))
    }
}

/// Note appended to every mutating `cap` outcome: caps are read at cage launch,
/// so a change only binds the next spawn (a live session keeps its old caps).
fn next_spawn_note() -> BlockContent {
    BlockContent::SystemInfo(
        "↻ takes effect at next spawn — a running session keeps its current caps".into(),
    )
}

/// The audit payload for a `cap.set` (before/after, for the SEAL record).
fn caps_audit(name: &str, before: ClassCaps, after: ClassCaps) -> serde_json::Value {
    serde_json::json!({
        "agent": name,
        "before": { "read": before.read, "write": before.write, "exec": before.exec },
        "after": { "read": after.read, "write": after.write, "exec": after.exec },
        "mode": mode_string(&after),
    })
}

fn cap_help() -> Vec<BlockContent> {
    vec![
        BlockContent::SystemInfo("cap — capability classes per agent (chmod for agents)".into()),
        BlockContent::SystemInfo("  cap                          grid of all agents".into()),
        BlockContent::SystemInfo("  cap @agent                   detail for one agent".into()),
        BlockContent::SystemInfo("  cap @agent +read +exec -write  grant / revoke classes".into()),
        BlockContent::SystemInfo(
            "  cap @agent --reset           drop per-agent policy (inherit global)".into(),
        ),
        BlockContent::SystemInfo("classes: read · write · exec    (+write requires read)".into()),
        BlockContent::SystemInfo(
            "frontier: spawn · reach — shown as `·`, reserved, not settable".into(),
        ),
    ]
}

fn read_policy(path: &Path) -> Option<Policy> {
    let raw = std::fs::read_to_string(path).ok()?;
    toml::from_str(&raw).ok()
}

/// Write the policy **atomically**: serialize to a sibling temp file, then
/// `rename` over the target. `rename(2)` is atomic on the same filesystem, so a
/// concurrent reader — the cage loading the policy at spawn — never observes a
/// half-written file (torn read). Pair with [`PolicyLock`] for the read-modify-
/// write so two `cap` mutations cannot lose an update.
fn write_policy(path: &Path, policy: &Policy) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let body = toml::to_string(policy).map_err(|e| e.to_string())?;
    let tmp = path.with_extension(format!("toml.tmp.{}", std::process::id()));
    std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

/// Advisory exclusive lock guarding a per-agent policy's read-modify-write, so
/// two concurrent `cap` mutations on the same agent cannot clobber each other
/// (lost update). Held via `flock(LOCK_EX)` on a sibling `.lock` file from
/// before the read until after the atomic write; released on drop. (The cage's
/// *read* at spawn is made safe separately by [`write_policy`]'s atomic rename —
/// it does not take this advisory lock, which only serializes writers.)
struct PolicyLock(std::fs::File);

impl PolicyLock {
    fn acquire(policy_path: &Path) -> Result<Self, String> {
        let lock_path = policy_path.with_extension("toml.lock");
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|e| e.to_string())?;
        // SAFETY: f owns a valid fd for the duration of the call; LOCK_EX blocks
        // until the lock is acquired. Released by LOCK_UN in Drop.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(format!(
                "cap: could not lock {}: {}",
                lock_path.display(),
                std::io::Error::last_os_error()
            ));
        }
        Ok(PolicyLock(f))
    }
}

impl Drop for PolicyLock {
    fn drop(&mut self) {
        // SAFETY: self.0 is a valid fd we hold an exclusive flock on.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// A minimal fail-closed policy for an agent with no own and no global policy:
/// all caps off, workspace `"."`. The `"."` is **deliberate, not a placeholder**:
/// the cage resolves the workspace root against the agent's cwd at spawn
/// (`resolve_workspace`), so `"."` means "wherever this agent is launched" —
/// portable across launch dirs, unlike a path baked in at `cap`-edit time.
fn default_policy() -> Policy {
    Policy {
        default_verdict: Verdict::Ask,
        caps: ClassCaps::default(),
        workspace: WorkspaceScope {
            root: PathBuf::from("."),
        },
        capabilities: Vec::new(),
    }
}

/// Parse `+class` / `-class` tokens into `(enable, class)` pairs.
fn parse_ops(ops: &[String]) -> Result<Vec<(bool, String)>, String> {
    ops.iter()
        .map(|op| {
            let (sign, class) = op.split_at(1);
            let on = match sign {
                "+" => true,
                "-" => false,
                _ => return Err(format!("cap: expected +class or -class, got '{op}'")),
            };
            if class.is_empty() {
                return Err(format!("cap: empty class in '{op}'"));
            }
            Ok((on, class.to_string()))
        })
        .collect()
}

/// Message for a non-domain class: a precise refusal for the reserved frontier,
/// a generic unknown otherwise.
fn frontier_or_unknown(class: &str, name: &str) -> String {
    if FRONTIER.contains(&class) {
        format!(
            "cap: '{class}' is a reserved frontier class for @{name} — shown, not settable \
             (no enforcement layer exists yet)"
        )
    } else {
        format!("cap: unknown class '{class}' (use read, write, or exec)")
    }
}

/// Column widths for the `cap` grid — kept in one struct so the row and header
/// builders stay under the 4-argument limit and share the exact same layout.
struct GridWidths {
    agent: usize,
    arch: usize,
    mode: usize,
}

/// The two header lines (group labels over the glyph columns, then the column
/// names). Emitted as single-cell `TableRow`s so they align with the data rows
/// in both renderers (a `SystemInfo` header would gain a leading indent in
/// shell mode and drift off the `TableRow` data).
fn grid_header_blocks(w: &GridWidths) -> Vec<BlockContent> {
    // Offset of the first glyph (`R`) column: AGENT+gap, ARCH+gap.
    let prefix = w.agent + 2 + w.arch + 2;
    // domain (R W X), a wider group gap, then frontier (S N). `frontier` sits
    // over `S`, 11 past `R` (R·W·X = 1+2+2 chars, then the 4-space group gap).
    let group = format!("{}domain     frontier", " ".repeat(prefix));
    let cols = format!(
        "{:<aw$}  {:<rw$}  R  W  X    S  N  {:<mw$}",
        "AGENT",
        "ARCH",
        "MODE",
        aw = w.agent,
        rw = w.arch,
        mw = w.mode,
    );
    // A rule under the column names separates the header from the data rows
    // (matches the target UI). Sized to the column line so it spans the table.
    let rule = "─".repeat(cols.chars().count());
    vec![
        BlockContent::TableRow(vec![StyledCell {
            text: group,
            style: CellStyle::Dim,
        }]),
        BlockContent::TableRow(vec![StyledCell {
            text: cols,
            style: CellStyle::Dim,
        }]),
        BlockContent::TableRow(vec![StyledCell {
            text: rule,
            style: CellStyle::Dim,
        }]),
    ]
}

/// One agent's cells: `@name`, archetype, the R/W/X domain glyphs, a group gap,
/// the S/N frontier `·`, then `MODE`.
fn row_cells(name: &str, archetype: &str, caps: &ClassCaps, w: &GridWidths) -> Vec<StyledCell> {
    vec![
        pad_cell(&format!("@{name}"), w.agent, CellStyle::Plain),
        pad_cell(archetype, w.arch, CellStyle::Dim),
        dot_cell(caps.read),
        dot_cell(caps.write),
        dot_cell(caps.exec),
        group_gap(),
        frontier_cell(),
        frontier_cell(),
        pad_cell(&mode_string(caps), w.mode, CellStyle::Dim),
    ]
}

/// The legend line: `●` enforced, `·` the not-yet-enforceable frontier. The
/// leading `●` is the only coloured cell so it reads as a key (matches the
/// target UI wording).
fn grid_footer() -> BlockContent {
    BlockContent::TableRow(vec![
        StyledCell {
            text: "●".into(),
            style: CellStyle::Good,
        },
        StyledCell {
            text: "enforced   · n/a in v1; spawn: no channel · reach: no netns".into(),
            style: CellStyle::Dim,
        },
    ])
}

/// A left-padded cell — width counts characters, so the multi-byte glyphs and
/// `@name` align the same way in both renderers.
fn pad_cell(text: &str, width: usize, style: CellStyle) -> StyledCell {
    StyledCell {
        text: format!("{text:<width$}"),
        style,
    }
}

/// A domain glyph: `●` (green) when the class is granted, `○` (dim) when denied.
fn dot_cell(on: bool) -> StyledCell {
    if on {
        StyledCell {
            text: "●".into(),
            style: CellStyle::Good,
        }
    } else {
        StyledCell {
            text: "○".into(),
            style: CellStyle::Dim,
        }
    }
}

/// A frontier glyph (`spawn`/`reach`): always `·` — shown, never enforced in v1.
fn frontier_cell() -> StyledCell {
    StyledCell {
        text: "·".into(),
        style: CellStyle::Dim,
    }
}

/// An empty cell that visually separates the `domain` group from the `frontier`
/// group: cells join with two spaces, so a zero-width cell yields a 4-space gap
/// (matches the `R  W  X    S  N` layout in [`grid_header_blocks`]).
fn group_gap() -> StyledCell {
    StyledCell {
        text: String::new(),
        style: CellStyle::Plain,
    }
}

/// The `rwxsn` MODE string: domain bits set or `-`, frontier always `·`.
fn mode_string(c: &ClassCaps) -> String {
    let mut s = String::with_capacity(5);
    s.push(if c.read { 'r' } else { '-' });
    s.push(if c.write { 'w' } else { '-' });
    s.push(if c.exec { 'x' } else { '-' });
    s.push('·'); // spawn — frontier
    s.push('·'); // reach — frontier
    s
}

fn onoff(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

fn class_line(name: &str, on: bool, note: &str) -> BlockContent {
    BlockContent::Notice {
        style: if on { CellStyle::Good } else { CellStyle::Dim },
        text: format!(" {name:<6} {:<4}  {note}", onoff(on)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(r: bool, w: bool, x: bool) -> ClassCaps {
        ClassCaps {
            read: r,
            write: w,
            exec: x,
        }
    }

    fn cell_text(b: &BlockContent) -> String {
        match b {
            BlockContent::TableRow(cells) => cells
                .iter()
                .map(|c| c.text.as_str())
                .collect::<Vec<_>>()
                .join("  "),
            other => panic!("expected TableRow, got {other:?}"),
        }
    }

    #[test]
    fn header_glyph_columns_align_with_data_rows() {
        let w = GridWidths {
            agent: 6,
            arch: 7,
            mode: 5,
        };
        let prefix = w.agent + 2 + w.arch + 2;
        // The `R` column header sits exactly where the first domain glyph lands.
        let header = grid_header_blocks(&w);
        let cols = cell_text(&header[1]);
        assert_eq!(cols.chars().nth(prefix), Some('R'));
        // …and a data row puts its first `●`/`○` at the same offset (cells are
        // joined with two spaces, matching the header's spacing).
        let row = cell_text(&BlockContent::TableRow(row_cells(
            "faye",
            "backend",
            &caps(true, true, false),
            &w,
        )));
        let glyph = row.chars().nth(prefix);
        assert!(matches!(glyph, Some('●') | Some('○')), "got {glyph:?}");
    }

    #[test]
    fn row_uses_at_prefix_glyphs_and_trailing_mode() {
        let w = GridWidths {
            agent: 6,
            arch: 7,
            mode: 5,
        };
        // Cells: 0=@name 1=arch 2=read 3=write 4=exec 5=group-gap 6=spawn 7=reach 8=MODE.
        let cells = row_cells("rex", "docs", &caps(true, true, false), &w);
        assert!(cells[0].text.starts_with("@rex"));
        assert_eq!(cells[2].text, "●"); // read granted
        assert_eq!(cells[4].text, "○"); // exec denied
        assert!(cells[5].text.is_empty()); // domain|frontier group gap
        assert!(cells[8].text.starts_with("rw-··")); // MODE last
    }

    #[test]
    fn dot_cell_glyph_and_style_track_grant() {
        assert_eq!(dot_cell(true).text, "●");
        assert_eq!(dot_cell(true).style, CellStyle::Good);
        assert_eq!(dot_cell(false).text, "○");
        assert_eq!(dot_cell(false).style, CellStyle::Dim);
    }

    #[test]
    fn mode_string_renders_domain_then_frontier() {
        assert_eq!(mode_string(&caps(true, true, true)), "rwx··");
        assert_eq!(mode_string(&caps(true, false, true)), "r-x··");
        assert_eq!(mode_string(&caps(false, false, false)), "---··");
    }

    #[test]
    fn mode_string_never_marks_frontier_settable() {
        // The last two positions are always `·`, regardless of domain bits.
        for c in [caps(true, true, true), caps(false, false, false)] {
            assert!(mode_string(&c).ends_with("··"));
        }
    }

    #[test]
    fn parse_ops_reads_signs_and_classes() {
        let got = parse_ops(&["+read".into(), "-write".into(), "+exec".into()]).unwrap();
        assert_eq!(
            got,
            vec![
                (true, "read".to_string()),
                (false, "write".to_string()),
                (true, "exec".to_string()),
            ]
        );
    }

    #[test]
    fn parse_ops_rejects_unsigned_and_empty() {
        assert!(parse_ops(&["read".into()]).is_err());
        assert!(parse_ops(&["+".into()]).is_err());
    }

    #[test]
    fn frontier_classes_are_refused_with_precise_message() {
        for class in ["spawn", "reach"] {
            let msg = frontier_or_unknown(class, "faye");
            assert!(msg.contains("reserved frontier class"), "{msg}");
            assert!(msg.contains(class));
        }
    }

    #[test]
    fn unknown_class_is_distinct_from_frontier() {
        let msg = frontier_or_unknown("network", "faye");
        assert!(msg.contains("unknown class"));
        assert!(!msg.contains("frontier"));
    }

    #[test]
    fn default_policy_is_fail_closed() {
        let p = default_policy();
        assert_eq!(p.caps, ClassCaps::default());
        assert!(!p.caps.read && !p.caps.write && !p.caps.exec);
        // workspace `.` is intentional: resolved against the agent's cwd at spawn,
        // not baked at cap-edit time. Locked so a "helpful" absolutize doesn't regress it.
        assert_eq!(p.workspace.root, std::path::PathBuf::from("."));
    }

    #[test]
    fn write_policy_is_atomic_and_leaves_no_temp() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("agents/faye/policy.toml");
        write_policy(&path, &default_policy()).unwrap();
        assert!(path.exists());
        // No leftover temp/lock-fd clutter that a reader might trip on.
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn policy_lock_acquire_release_is_reentrant_across_calls() {
        // Sequential acquire/release must not deadlock or error (drop unlocks).
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("agents/faye/policy.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        {
            let _l = PolicyLock::acquire(&path).unwrap();
        }
        let _l2 = PolicyLock::acquire(&path).unwrap();
    }

    #[test]
    fn write_policy_round_trips_caps_and_preserves_siblings() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("agents/faye/policy.toml");
        let mut p = default_policy();
        p.default_verdict = Verdict::Deny;
        p.caps = caps(true, true, false);
        p.capabilities.push(orkia_shell_types::Capability {
            name: "git.push".into(),
            matches: vec!["git push*".into()],
            verdict: Verdict::Deny,
            sensitivity: orkia_shell_types::Sensitivity::Sensitive,
        });
        write_policy(&path, &p).unwrap();
        let back = read_policy(&path).unwrap();
        assert_eq!(back, p); // caps + default_verdict + capabilities all preserved
        assert!(back.caps.read && back.caps.write && !back.caps.exec);
    }
}
