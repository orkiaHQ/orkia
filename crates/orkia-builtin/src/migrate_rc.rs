// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `orkia migrate-rc` — turn a `.zshrc` / `.bashrc` / fish config into an
//! `~/.orkiarc` that brush can source.
//!
//! Pure functions. No I/O. The REPL builtin reads the source, calls
//! [`classify_lines`] + [`generate_orkiarc`], and writes the result.
//! Keeping the parser side-effect-free makes testing trivial: feed an
//! input string, assert on the produced output.

use std::path::Path;

/// Per-line classification produced by the parser. The full input file
/// becomes `Vec<LineClass>`; the order is preserved when rendering the
/// output (skipped lines move to the trailer).
#[derive(Debug, Clone, PartialEq)]
pub enum LineClass {
    /// Line is already bash-compatible — copy verbatim.
    Migrate(String),
    /// Line was rewritten for bash. `(original, translated)`.
    Translate(String, String),
    /// Line cannot be safely brought across. `(original, reason)`.
    Skip(String, SkipReason),
    /// Comment line; preserved as-is.
    Comment(String),
    /// Blank line; preserved as-is.
    Blank,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// `setopt` / `unsetopt` — zsh option toggle.
    ZshSetopt,
    /// `autoload` / `compinit` / `compdef`.
    ZshAutoload,
    /// `bindkey` — zsh line editor binding.
    ZshBindkey,
    /// `zplugin` / `zinit` / `antigen` / `zplug` / oh-my-zsh.
    ZshPlugin,
    /// `typeset -A` / `-a` with zsh-specific syntax.
    ZshTypeset,
    /// `${(k)...}`, `${(v)...}`, `${(j)...}` — zsh parameter expansion flags.
    ZshArrayExpansion,
    /// `PROMPT=` / `PS1=` containing zsh-only escapes (`%F`, `%~`, `%n`).
    ZshPrompt,
    /// fish `set` for a non-exported variable (brush has no equivalent).
    FishSet,
    /// fish `function ... end` block.
    FishFunction,
    /// fish `abbr` that couldn't be translated.
    FishAbbr,
    /// Couldn't classify.
    Unknown,
}

/// Source flavour detected by [`detect_source_kind`] or supplied via
/// `--from`. Drives which line classifier is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Zsh,
    Bash,
    Fish,
}

impl SourceKind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Zsh => "zsh",
            Self::Bash => "bash",
            Self::Fish => "fish",
        }
    }
}

/// Guess source flavour from the file path. zsh's config and bash's
/// config both live in `~`, so we match on the file name; fish lives
/// under `~/.config/fish`.
pub fn detect_source_kind(path: &Path) -> Option<SourceKind> {
    let name = path.file_name()?.to_str()?;
    if name == ".zshrc" || name == ".zprofile" || name == ".zshenv" {
        return Some(SourceKind::Zsh);
    }
    if name == ".bashrc" || name == ".bash_profile" || name == ".bash_login" {
        return Some(SourceKind::Bash);
    }
    if name == "config.fish" {
        return Some(SourceKind::Fish);
    }
    // Path-based fallback for non-standard names.
    let path_str = path.to_string_lossy();
    if path_str.contains("/fish/") {
        Some(SourceKind::Fish)
    } else {
        None
    }
}

/// Sniff the source flavour from file *content* when the filename is
/// non-standard. Reuses the exact markers the per-shell classifiers
/// trust ([`zsh_skip_reason`] + the fish `set -gx`/`abbr`/`end` forms),
/// so detection never diverges from migration.
///
/// Falls back to [`SourceKind::Bash`]: absent any zsh-specific marker,
/// the bash and zsh classifiers produce identical output (zsh only
/// differs by *skipping* those markers), so Bash is the safe generic
/// default — no guessing that could change the result.
pub fn detect_source_kind_from_content(content: &str) -> SourceKind {
    let mut saw_zsh = false;
    for raw in content.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if is_fish_marker(trimmed) {
            return SourceKind::Fish;
        }
        if zsh_skip_reason(trimmed).is_some() {
            saw_zsh = true;
        }
    }
    if saw_zsh {
        SourceKind::Zsh
    } else {
        SourceKind::Bash
    }
}

/// Distinctive fish-only syntax: exported `set` forms, `abbr`, and the
/// bare `end` block terminator. These never appear in bash/zsh.
fn is_fish_marker(trimmed: &str) -> bool {
    starts_with_any(
        trimmed,
        &["set -gx ", "set -Ux ", "set -xg ", "set -xU ", "abbr "],
    ) || trimmed == "end"
}

/// Auto-detect a source rc file from `$SHELL` + existence checks. Order:
/// 1. `$SHELL` basename — zsh / bash / fish.
/// 2. First of `~/.zshrc`, `~/.bashrc`, `~/.config/fish/config.fish`
///    that exists.
pub fn auto_detect_source(home: &Path) -> Option<(std::path::PathBuf, SourceKind)> {
    if let Some(shell) = std::env::var_os("SHELL") {
        let shell = shell.to_string_lossy();
        let basename = shell.rsplit('/').next().unwrap_or("");
        let candidate = match basename {
            "zsh" => Some((home.join(".zshrc"), SourceKind::Zsh)),
            "bash" => Some((home.join(".bashrc"), SourceKind::Bash)),
            "fish" => Some((home.join(".config/fish/config.fish"), SourceKind::Fish)),
            _ => None,
        };
        if let Some((p, k)) = candidate
            && p.exists()
        {
            return Some((p, k));
        }
    }
    for (rel, kind) in [
        (".zshrc", SourceKind::Zsh),
        (".bashrc", SourceKind::Bash),
        (".config/fish/config.fish", SourceKind::Fish),
    ] {
        let p = home.join(rel);
        if p.exists() {
            return Some((p, kind));
        }
    }
    None
}

/// Classify an entire input file. Each input line becomes one
/// `LineClass`; trailing `\n` is stripped from the stored string.
pub fn classify_lines(input: &str, kind: SourceKind) -> Vec<LineClass> {
    input.lines().map(|l| classify_line(l, kind)).collect()
}

fn classify_line(line: &str, kind: SourceKind) -> LineClass {
    match kind {
        SourceKind::Bash => classify_bash_line(line),
        SourceKind::Zsh => classify_zsh_line(line),
        SourceKind::Fish => classify_fish_line(line),
    }
}

// ───── bash (trivial: everything is compatible) ─────────────────────

fn classify_bash_line(line: &str) -> LineClass {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return LineClass::Blank;
    }
    if trimmed.starts_with('#') {
        return LineClass::Comment(line.into());
    }
    LineClass::Migrate(line.into())
}

// ───── zsh ─────────────────────────────────────────────────────────

fn classify_zsh_line(line: &str) -> LineClass {
    let trimmed = line.trim();

    if trimmed.is_empty() {
        return LineClass::Blank;
    }
    if trimmed.starts_with('#') {
        return LineClass::Comment(line.into());
    }

    // Skip rules first — these patterns also happen to start with
    // tokens that look bash-ish at first glance (e.g. `PROMPT=` with
    // zsh escapes), so the skip-list takes precedence.
    if let Some(reason) = zsh_skip_reason(trimmed) {
        return LineClass::Skip(line.into(), reason);
    }

    // Migrate: explicit bash-compatible keywords / forms.
    if starts_with_any(
        trimmed,
        &[
            "export ",
            "alias ",
            "unalias ",
            "source ",
            ". ",
            "eval ",
            "PATH=",
            "if ",
            "then",
            "fi",
            "else",
            "elif ",
            "for ",
            "do",
            "done",
            "while ",
            "case ",
            "esac",
            "function ",
            "return",
            "exit",
            "echo ",
            "printf ",
            "read ",
            "command ",
            "type ",
            "which ",
            "test ",
        ],
    ) {
        return LineClass::Migrate(line.into());
    }
    if trimmed.ends_with("() {") || trimmed == "}" {
        return LineClass::Migrate(line.into());
    }
    if looks_like_simple_assignment(trimmed) {
        return LineClass::Migrate(line.into());
    }

    // Default: try to migrate; brush will refuse at runtime if it can't
    // parse. The user sees the brush diagnostic and edits the line.
    LineClass::Migrate(line.into())
}

fn zsh_skip_reason(trimmed: &str) -> Option<SkipReason> {
    if trimmed.starts_with("setopt ") || trimmed.starts_with("unsetopt ") {
        return Some(SkipReason::ZshSetopt);
    }
    if trimmed.starts_with("autoload ")
        || trimmed == "compinit"
        || trimmed.starts_with("compinit ")
        || trimmed.starts_with("compdef ")
    {
        return Some(SkipReason::ZshAutoload);
    }
    if trimmed.starts_with("bindkey ") {
        return Some(SkipReason::ZshBindkey);
    }
    if starts_with_any(trimmed, &["zplugin ", "zinit ", "antigen ", "zplug "])
        || trimmed.contains("oh-my-zsh")
    {
        return Some(SkipReason::ZshPlugin);
    }
    if trimmed.starts_with("typeset ") {
        return Some(SkipReason::ZshTypeset);
    }
    if trimmed.contains("${(") {
        return Some(SkipReason::ZshArrayExpansion);
    }
    if (trimmed.starts_with("PROMPT=")
        || trimmed.starts_with("PS1=")
        || trimmed.starts_with("RPROMPT="))
        && (trimmed.contains("%F")
            || trimmed.contains("%~")
            || trimmed.contains("%n")
            || trimmed.contains("%K")
            || trimmed.contains("%f"))
    {
        return Some(SkipReason::ZshPrompt);
    }
    None
}

// ───── fish ────────────────────────────────────────────────────────

fn classify_fish_line(line: &str) -> LineClass {
    let trimmed = line.trim();

    if trimmed.is_empty() {
        return LineClass::Blank;
    }
    if trimmed.starts_with('#') {
        return LineClass::Comment(line.into());
    }

    // set -gx / -Ux → export
    if trimmed.starts_with("set -gx ")
        || trimmed.starts_with("set -Ux ")
        || trimmed.starts_with("set -xg ")
        || trimmed.starts_with("set -xU ")
    {
        if let Some(translated) = translate_fish_set_export(trimmed) {
            return LineClass::Translate(line.into(), translated);
        }
        return LineClass::Skip(line.into(), SkipReason::FishSet);
    }

    // Plain `set` — non-exported variable. brush has no equivalent.
    if trimmed.starts_with("set ") {
        return LineClass::Skip(line.into(), SkipReason::FishSet);
    }

    // abbr → alias
    if trimmed.starts_with("abbr ") {
        if let Some(translated) = translate_fish_abbr(trimmed) {
            return LineClass::Translate(line.into(), translated);
        }
        return LineClass::Skip(line.into(), SkipReason::FishAbbr);
    }

    if trimmed.starts_with("function ") || trimmed == "end" {
        return LineClass::Skip(line.into(), SkipReason::FishFunction);
    }

    if trimmed.starts_with("source ") || trimmed.starts_with(". ") {
        return LineClass::Migrate(line.into());
    }

    LineClass::Skip(line.into(), SkipReason::Unknown)
}

/// `set -gx EDITOR helix` → `export EDITOR=helix`
/// `set -gx PATH /usr/local/bin $PATH` → `export PATH=/usr/local/bin:$PATH`
pub fn translate_fish_set_export(line: &str) -> Option<String> {
    // Drop the leading "set -gx" / "set -Ux" / etc. (two tokens).
    let mut parts = line.split_whitespace();
    parts.next()?; // "set"
    parts.next()?; // flags
    let name = parts.next()?;
    let values: Vec<&str> = parts.collect();
    if values.is_empty() {
        return Some(format!("export {name}="));
    }
    // fish lists join with ':' in PATH contexts. For non-PATH vars, the
    // user can hand-edit, but ':' join is a safe default since brush
    // sees `export FOO=a:b` as a literal string regardless.
    let joined = values.join(":");
    Some(format!("export {name}={joined}"))
}

/// `abbr -a gs git status` → `alias gs='git status'`
/// `abbr gs git status`    → `alias gs='git status'`
/// `abbr --add gs git status` → `alias gs='git status'`
pub fn translate_fish_abbr(line: &str) -> Option<String> {
    let rest = line.strip_prefix("abbr")?.trim_start();
    // Drop optional flags ("-a", "--add").
    let rest = rest
        .strip_prefix("-a ")
        .or_else(|| rest.strip_prefix("--add "))
        .unwrap_or(rest);
    let mut it = rest.splitn(2, char::is_whitespace);
    let name = it.next()?.trim();
    let value = it.next()?.trim();
    if name.is_empty() || value.is_empty() {
        return None;
    }
    Some(format!("alias {name}='{value}'"))
}

// ───── helpers ─────────────────────────────────────────────────────

fn starts_with_any(line: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|p| line.starts_with(p))
}

fn looks_like_simple_assignment(line: &str) -> bool {
    // `NAME=value` with `NAME` being a valid identifier and no
    // whitespace before `=`.
    let Some(eq_pos) = line.find('=') else {
        return false;
    };
    if eq_pos == 0 {
        return false;
    }
    let name = &line[..eq_pos];
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// ───── output generation ──────────────────────────────────────────

/// Aggregate counts shown in the summary block.
#[derive(Debug, Default, Clone, Copy)]
pub struct MigrationCounts {
    pub migrated: usize,
    pub translated: usize,
    pub skipped: usize,
    pub comments: usize,
}

impl MigrationCounts {
    pub fn from_lines(lines: &[LineClass]) -> Self {
        let mut c = Self::default();
        for l in lines {
            match l {
                LineClass::Migrate(_) => c.migrated += 1,
                LineClass::Translate(_, _) => c.translated += 1,
                LineClass::Skip(_, _) => c.skipped += 1,
                LineClass::Comment(_) => c.comments += 1,
                LineClass::Blank => {}
            }
        }
        c
    }
}

/// Render the classified lines into a `.orkiarc` file body.
pub fn generate_orkiarc(
    source_path: &str,
    kind: SourceKind,
    lines: &[LineClass],
    date: &str,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# ~/.orkiarc — generated by orkia migrate-rc from {source_path}\n"
    ));
    out.push_str(&format!("# date: {date}\n"));
    out.push_str("#\n");
    out.push_str(&format!(
        "# Review this file. Lines marked TRANSLATED were converted from {} syntax.\n",
        kind.name(),
    ));
    out.push_str("# Skipped lines are listed at the bottom as comments.\n\n");

    for line in lines {
        match line {
            LineClass::Migrate(l) => {
                out.push_str(l);
                out.push('\n');
            }
            LineClass::Translate(_, translated) => {
                out.push_str(translated);
                out.push_str("  # TRANSLATED\n");
            }
            LineClass::Comment(l) => {
                out.push_str(l);
                out.push('\n');
            }
            LineClass::Blank => out.push('\n'),
            LineClass::Skip(_, _) => {}
        }
    }

    let skipped: Vec<_> = lines
        .iter()
        .filter_map(|l| match l {
            LineClass::Skip(orig, reason) => Some((orig, reason)),
            _ => None,
        })
        .collect();

    if !skipped.is_empty() {
        out.push_str("\n# ──────────────────────────────────────────────\n");
        out.push_str(&format!(
            "# {} line{} skipped ({}-specific syntax):\n",
            skipped.len(),
            if skipped.len() == 1 { "" } else { "s" },
            kind.name(),
        ));
        out.push_str("# Uncomment and translate manually if needed.\n");
        out.push_str("#\n");
        for (orig, reason) in &skipped {
            out.push_str(&format!("# SKIP ({reason:?}): {}\n", orig.trim()));
        }
    }

    out
}

/// Parsed `migrate-rc` flags. Free-standing so both the REPL builtin
/// (`orkia> migrate-rc ...`) and the binary subcommand
/// (`orkia migrate-rc ...`) share one parser + one set of semantics.
#[derive(Debug, Clone, Default)]
pub struct MigrateRcOpts {
    pub from: Option<std::path::PathBuf>,
    pub kind: Option<SourceKind>,
    pub dry_run: bool,
    pub append: bool,
}

impl MigrateRcOpts {
    pub fn parse<S: AsRef<str>>(args: &[S]) -> Result<Self, String> {
        let mut out = Self::default();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_ref() {
                "--from" => {
                    let p = it
                        .next()
                        .ok_or_else(|| "missing argument to --from".to_string())?;
                    out.from = Some(std::path::PathBuf::from(p.as_ref()));
                }
                "--dry-run" | "-n" => out.dry_run = true,
                "--append" | "-a" => out.append = true,
                "--zsh" => out.kind = Some(SourceKind::Zsh),
                "--bash" => out.kind = Some(SourceKind::Bash),
                "--fish" => out.kind = Some(SourceKind::Fish),
                other => return Err(format!("unknown flag: {other}")),
            }
        }
        Ok(out)
    }

    /// Resolve `(source_path, kind)` given a HOME directory. The kind is
    /// `None` only for an explicit `--from <file>` whose flavour neither
    /// the flags nor the filename pin down — [`run_migration`] then sniffs
    /// it from the file content.
    pub fn resolve_source(
        &self,
        home: &Path,
    ) -> Result<(std::path::PathBuf, Option<SourceKind>), String> {
        if let Some(p) = &self.from {
            // explicit flag > filename > (deferred) content sniff.
            let kind = self.kind.or_else(|| detect_source_kind(p));
            return Ok((p.clone(), kind));
        }
        if let Some(k) = self.kind {
            let p = match k {
                SourceKind::Zsh => home.join(".zshrc"),
                SourceKind::Bash => home.join(".bashrc"),
                SourceKind::Fish => home.join(".config/fish/config.fish"),
            };
            if !p.exists() {
                return Err(format!("no rc at {}", p.display()));
            }
            return Ok((p, Some(k)));
        }
        auto_detect_source(home)
            .map(|(p, k)| (p, Some(k)))
            .ok_or_else(|| "no existing rc found (.zshrc / .bashrc / fish config)".into())
    }
}

/// Result of [`run_migration`] — what the REPL renders as blocks and
/// what the binary main prints to stderr.
pub struct MigrationReport {
    pub source_path: std::path::PathBuf,
    pub kind: SourceKind,
    pub counts: MigrationCounts,
    pub skipped: Vec<(String, SkipReason)>,
    /// Rendered body of the would-be / actual `~/.orkiarc`.
    pub orkiarc_body: String,
    /// `Some(path)` if `.orkiarc` was written, `None` for `--dry-run`.
    pub written_to: Option<std::path::PathBuf>,
    /// `Some(err)` if the write failed (the caller decides how loud to
    /// be about it).
    pub write_error: Option<String>,
}

/// End-to-end: read the source rc, classify, render the orkiarc body,
/// and (unless `--dry-run`) write it to `dest`. Stable interface shared
/// between the REPL builtin and the CLI subcommand.
pub fn run_migration(
    opts: &MigrateRcOpts,
    home: &Path,
    dest: &Path,
    today: &str,
) -> Result<MigrationReport, String> {
    let (source_path, kind_hint) = opts.resolve_source(home)?;
    let input = std::fs::read_to_string(&source_path)
        .map_err(|e| format!("read {}: {e}", source_path.display()))?;
    // Non-standard `--from` filename with no flag → sniff the content.
    let kind = kind_hint.unwrap_or_else(|| detect_source_kind_from_content(&input));
    let lines = classify_lines(&input, kind);
    let counts = MigrationCounts::from_lines(&lines);
    let skipped: Vec<_> = lines
        .iter()
        .filter_map(|l| match l {
            LineClass::Skip(orig, reason) => Some((orig.clone(), *reason)),
            _ => None,
        })
        .collect();
    let body = generate_orkiarc(&source_path.display().to_string(), kind, &lines, today);

    let mut written_to = None;
    let mut write_error = None;
    if !opts.dry_run {
        match write_orkiarc(dest, &body, opts.append) {
            Ok(()) => written_to = Some(dest.to_path_buf()),
            Err(e) => write_error = Some(format!("write {}: {e}", dest.display())),
        }
    }

    Ok(MigrationReport {
        source_path,
        kind,
        counts,
        skipped,
        orkiarc_body: body,
        written_to,
        write_error,
    })
}

fn write_orkiarc(dest: &Path, body: &str, append: bool) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true);
    if append {
        opts.append(true);
    } else {
        opts.truncate(true);
    }
    let mut f = opts.open(dest)?;
    f.write_all(body.as_bytes())
}

#[cfg(test)]
mod tests;
