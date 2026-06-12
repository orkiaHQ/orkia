// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! [`Policy`] is the data the Orkia Cage consumes: a workspace (filesystem)
//! bound, a set of named [`Capability`] rules, and a fallthrough [`Verdict`].
//! It is pure data — it declares *what* is allowed, never *how* isolation is
//! enforced (that lives in the cage binary).
//!
//! [`PolicyProvider`] is the open-core seam: the OSS shell ships a trivial
//! TOML loader (`orkia_shell::TomlPolicyLoader`), while an enterprise build can
//! drop in an RFC-driven compiler behind the same trait without touching OSS
//! code — the same pattern as [`crate::ForgeBuilder`].
//!
//! The model is **hybrid**: a capability is a named group of command-match
//! rules sharing one verdict. The `name` is a stable id (e.g. `"git.push"`)
//! that future trust scoring keys on; V1 enforcement matches on `matches`.
//!
//! The policy file is **TOML** — consistent with every other Orkia config
//! (`config.toml`, `agent.toml`, `manifest.toml`), reusing the maintained
//! `toml` crate already in the tree. The policy file *is* the security
//! perimeter, so it is not parsed by an unmaintained dependency.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// the workspace root the agent may see and write; everything else is hidden.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceScope {
    /// Directory made visible + writable inside the cage. Relative paths
    /// resolve against the agent's cwd at spawn time.
    pub root: PathBuf,
}

/// The decision attached to a capability (or applied as the policy default).
///
/// In V1 [`Verdict::Ask`] is a recognized, recorded value but resolves as
/// [`Verdict::Deny`] at enforcement time (fail-closed) — interactive
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Allow,
    Ask,
    Deny,
}

/// Declared in policy, versioned, **fail-closed**: an un-annotated capability
/// is [`Sensitivity::Sensitive`] (human-gated), never silently auto-promotable.
///
/// **Inert in V1** — no cage/`orkia-sh` decision-path code reads this. It is the
/// declared input the Trust Atlas seam ([`crate::apply_trust`]) consumes: a benign
/// capability may auto-promote `Ask`→auto on evidence; a sensitive one stays `Ask`
/// until an explicit human unlock is recorded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sensitivity {
    /// May auto-promote `Ask`→auto-`Allow` as benign evidence accumulates (capped).
    Benign,
    /// Human-gated: auto-promotion requires a recorded one-time human unlock. The
    /// fail-closed default for any un-annotated capability.
    #[default]
    Sensitive,
}

/// A named group of command-match rules sharing one verdict.
///
/// `name` is a stable capability id (e.g. `"git.push"`) — it is what Trust
/// Atlas will key on later, so it must not change meaning between versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capability {
    pub name: String,
    /// Glob patterns matched against the command line — see [`Policy::evaluate_match`]
    /// for the (deliberately dumb) semantics.
    ///
    /// **V1 placeholder.** This string-glob is a stand-in for a real
    /// `argv → capability` classifier. A glob like `"git push*"` does not
    /// understand argument structure, so `git -C /repo push` does *not* match
    /// it and falls through to the default verdict (safe: it fails toward
    /// with an argv classifier **keeping the same `name`s**, so trust evidence
    /// keyed on `name` survives the change — no trust migration.
    pub matches: Vec<String>,
    pub verdict: Verdict,
    /// Declared in the policy TOML; absent ⇒ [`Sensitivity::Sensitive`]
    /// (fail-closed). Inert in V1 — read only by the Atlas seam, never the cage.
    #[serde(default)]
    pub sensitivity: Sensitivity,
}

/// Per-agent capability classes — the coarse `chmod`-style switches the `cap`
/// builtin shows and sets. Each is a *master switch* for an enforcement layer:
///
/// - `read`  → the cage mounts the workspace at all (false ⇒ omitted, ENOENT);
/// - `write` → the workspace mount is read-write (false ⇒ read-only, EROFS);
/// - `exec`  → the command shim consults `capabilities[]` (false ⇒ the class is
///   closed and every command is denied before any rule is checked).
///
/// **Exactly three fields.** The `spawn`/`reach` frontier is deliberately *not*
/// represented here: it is vocabulary the `cap` surface displays but refuses to
/// set, so it can never be persisted or drift on. When the frontier enforcement
/// layer lands, it adds its fields here and flips those columns from reserved.
///
/// Default is **all-false** (fail-closed): a policy that omits
/// `[caps]` grants no class. Co-located per-agent policies (`cap @agent`) carry
/// an explicit `[caps]` block.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassCaps {
    #[serde(default)]
    pub read: bool,
    #[serde(default)]
    pub write: bool,
    #[serde(default)]
    pub exec: bool,
}

/// The full cage policy. Net-new in V1 (no prior policy type existed).
///
/// `network` is intentionally absent in V1: network namespaces are out of
/// scope, so the policy declares no bound the cage cannot enforce. Add it when
///
/// Field order is deliberate: the scalar `default_verdict` precedes the `caps`,
/// `workspace`, and `capabilities` tables/arrays-of-tables so TOML
/// *serialization* stays valid (a bare key may not follow a table at the same
/// level).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// Verdict applied when no capability matches. Defaults to [`Verdict::Ask`]
    /// (fail-closed) when the field is omitted from the file.
    #[serde(default = "default_verdict")]
    pub default_verdict: Verdict,
    /// Coarse per-agent capability classes. Omitted ⇒ all-false (fail-closed).
    #[serde(default)]
    pub caps: ClassCaps,
    pub workspace: WorkspaceScope,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
}

fn default_verdict() -> Verdict {
    Verdict::Ask
}

impl Policy {
    /// Resolve a command line to a typed [`PolicyDecision`], falling back to
    /// [`Policy::default_verdict`] when nothing matches.
    ///
    /// Matching is deliberately simple and stable:
    /// - both the command line and each pattern are whitespace-normalized
    ///   (leading/trailing trimmed, internal runs collapsed to one space);
    /// - a pattern ending in `*` matches when the normalized command line
    ///   *starts with* the normalized prefix (so `"git push*"` matches
    ///   `"git push origin main"` — and, as a known consequence of a literal
    ///   prefix, also `"git pushy"`; author patterns accordingly);
    /// - any other pattern matches only on exact equality;
    /// - capabilities are evaluated in declaration order; the first capability
    ///   with any matching pattern wins.
    ///
    /// The returned [`PolicyDecision`] names which rule fired (`rule`) for audit
    ///
    /// A command line may chain several simple commands with shell operators
    /// (`&&`, `||`, `;`, `|`, `&`). Matching the whole string against one glob
    /// let a denied command hide behind an allowed prefix (`echo ok && git
    /// push`). We instead evaluate **each segment independently** and let the
    /// **most restrictive** decision win (deny > ask > allow) — fail-closed.
    /// Each segment is canonicalized first (see [`canonicalize`])
    /// so a structured `git -C /repo push` matches `git push*` rather than
    /// escaping to the default. The match is on the raw (normalized) command
    pub fn evaluate_match(&self, command_line: &str) -> PolicyDecision<'_> {
        let mut chosen: Option<PolicyDecision<'_>> = None;
        for segment in split_segments(command_line) {
            let canon = canonicalize(&segment);
            if canon.is_empty() {
                continue;
            }
            let m = self.match_one(&canon);
            chosen = Some(match chosen {
                Some(prev) if tier(&prev) >= tier(&m) => prev,
                _ => m,
            });
        }
        chosen.unwrap_or_else(|| decision_from(self.default_verdict, None, None))
    }

    /// Match a single already-canonicalized simple command against the
    /// capabilities in declaration order; fall back to the policy default.
    fn match_one(&self, cmd: &str) -> PolicyDecision<'_> {
        for cap in &self.capabilities {
            if let Some(rule) = cap.matches.iter().find(|p| pattern_matches(p, cmd)) {
                return decision_from(cap.verdict, Some(cap.name.as_str()), Some(rule.as_str()));
            }
        }
        decision_from(self.default_verdict, None, None)
    }
}

/// Build a [`PolicyDecision`] from a base [`Verdict`] + the matched capability
/// and rule. `Allow`/`Deny` are terminal; `Ask` wraps an [`Adjustable`] — the
/// only tier a future trust layer may act on.
fn decision_from<'a>(
    verdict: Verdict,
    capability: Option<&'a str>,
    rule: Option<&'a str>,
) -> PolicyDecision<'a> {
    match verdict {
        Verdict::Allow => PolicyDecision::Allow { capability, rule },
        Verdict::Deny => PolicyDecision::Deny { capability, rule },
        Verdict::Ask => PolicyDecision::Ask(Adjustable { capability, rule }),
    }
}

/// Severity ordering for combining segment decisions — deny is most restrictive.
fn tier(d: &PolicyDecision) -> u8 {
    match d {
        PolicyDecision::Allow { .. } => 0,
        PolicyDecision::Ask(_) => 1,
        PolicyDecision::Deny { .. } => 2,
    }
}

/// The typed result of evaluating a command against a [`Policy`].
///
/// `Allow`/`Deny` are **terminal** — they carry no field a trust layer could
/// touch. `Ask` is the **only** adjustable tier: it wraps an [`Adjustable`]
/// whose [`Adjustable::resolve`] is the sole path from a trust decision to a
/// terminal [`Verdict`]. This makes "turn a base `Deny` into `Allow`"
/// unrepresentable — the trust-anchor invariant, enforced by the
/// type system rather than by convention.
///
/// Named `PolicyDecision` (not `Decision`) because [`crate::Decision`] already
/// exists (the REPL classifier type).
///
/// The structural invariant is proven by doctest. A trust layer cannot resolve
/// a terminal decision — there is no `resolve` on `PolicyDecision`:
///
/// ```compile_fail
/// use orkia_shell_types::{PolicyDecision, AskOutcome};
/// let d = PolicyDecision::Deny { capability: None, rule: None };
/// let _ = d.resolve(AskOutcome::Auto); // ERROR: no method `resolve` on PolicyDecision
/// ```
///
/// …and there is no `Allow`/`Deny` → [`Adjustable`] conversion:
///
/// ```compile_fail
/// use orkia_shell_types::{PolicyDecision, Adjustable};
/// let d = PolicyDecision::Allow { capability: None, rule: None };
/// let _a: Adjustable = d.into(); // ERROR: no PolicyDecision -> Adjustable conversion
/// ```
///
/// …while the legitimate `Ask → Allow` path *does* compile (the type stays
/// usable, just safe):
///
/// ```
/// use orkia_shell_types::{PolicyDecision, Adjustable, AskOutcome, Verdict};
/// let d = PolicyDecision::Ask(Adjustable { capability: Some("git.commit"), rule: None });
/// let v = match d {
///     PolicyDecision::Ask(a) => a.resolve(AskOutcome::Auto),
///     _ => unreachable!(),
/// };
/// assert_eq!(v, Verdict::Allow);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyDecision<'a> {
    /// Terminal allow — the command runs (subject to audit).
    Allow {
        capability: Option<&'a str>,
        rule: Option<&'a str>,
    },
    /// Terminal deny — the command is refused.
    Deny {
        capability: Option<&'a str>,
        rule: Option<&'a str>,
    },
    /// The adjustable tier — the only decision a future trust layer may widen.
    Ask(Adjustable<'a>),
}

impl<'a> PolicyDecision<'a> {
    /// The matched capability name, or `None` on a default fall-through.
    pub fn capability(&self) -> Option<&'a str> {
        match self {
            PolicyDecision::Allow { capability, .. } | PolicyDecision::Deny { capability, .. } => {
                *capability
            }
            PolicyDecision::Ask(a) => a.capability,
        }
    }

    /// The exact pattern that fired, or `None` on a default fall-through.
    pub fn rule(&self) -> Option<&'a str> {
        match self {
            PolicyDecision::Allow { rule, .. } | PolicyDecision::Deny { rule, .. } => *rule,
            PolicyDecision::Ask(a) => a.rule,
        }
    }
}

/// The `Ask` tier — the sole surface a future trust layer may resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Adjustable<'a> {
    pub capability: Option<&'a str>,
    pub rule: Option<&'a str>,
}

impl Adjustable<'_> {
    /// Resolve an `Ask` into a terminal [`Verdict`]. Yields `Allow` (auto) or
    /// `Deny` (keep ask-as-deny). It can **never** produce `Allow` from a base
    /// `Deny`, because a `Deny` is never an [`Adjustable`] — the trust-anchor
    /// invariant made structural.
    pub fn resolve(self, outcome: AskOutcome) -> Verdict {
        match outcome {
            AskOutcome::Auto => Verdict::Allow,
            AskOutcome::Ask => Verdict::Deny,
        }
    }
}

/// The only output a future trust layer may produce for an `Ask`. Deliberately
/// **not** [`Verdict`] — so a trust layer cannot name `Allow`/`Deny` directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskOutcome {
    /// Promote the ask to an automatic allow.
    Auto,
    /// Leave it an ask (V1 enforces ask as deny).
    Ask,
}

/// Collapse whitespace: trim ends and reduce internal runs to a single space.
fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn pattern_matches(pattern: &str, normalized_cmd: &str) -> bool {
    match pattern.trim().strip_suffix('*') {
        Some(prefix) => normalized_cmd.starts_with(&normalize(prefix)),
        None => normalized_cmd == normalize(pattern),
    }
}

/// Split a command line into simple-command segments on the shell control
/// operators `&&`, `||`, `;`, `|`, `&`, and newline — **outside** quotes and
/// backslash escapes, so `echo "a && b"` stays one segment. Over-splitting is
/// safe for a deny boundary (more segments = more chances to hit a deny);
/// under-splitting is the hazard, so unparsed constructs (redirections, etc.)
/// merely produce extra harmless segments rather than ever merging a denied
/// command into an allowed one.
fn split_segments(line: &str) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    let mut segs = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_single {
            cur.push(c);
            if c == '\'' {
                in_single = false;
            }
            i += 1;
        } else if in_double {
            if c == '\\' && i + 1 < chars.len() {
                cur.push(c);
                cur.push(chars[i + 1]);
                i += 2;
            } else {
                cur.push(c);
                if c == '"' {
                    in_double = false;
                }
                i += 1;
            }
        } else {
            match c {
                '\'' => {
                    in_single = true;
                    cur.push(c);
                    i += 1;
                }
                '"' => {
                    in_double = true;
                    cur.push(c);
                    i += 1;
                }
                '\\' if i + 1 < chars.len() => {
                    cur.push(c);
                    cur.push(chars[i + 1]);
                    i += 2;
                }
                '&' if i + 1 < chars.len() && chars[i + 1] == '&' => {
                    segs.push(std::mem::take(&mut cur));
                    i += 2;
                }
                '|' if i + 1 < chars.len() && chars[i + 1] == '|' => {
                    segs.push(std::mem::take(&mut cur));
                    i += 2;
                }
                ';' | '|' | '&' | '\n' => {
                    segs.push(std::mem::take(&mut cur));
                    i += 1;
                }
                _ => {
                    cur.push(c);
                    i += 1;
                }
            }
        }
    }
    segs.push(cur);
    segs.into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Canonicalize one segment for matching. For a `git` invocation, strip the
/// global options that may precede the subcommand (`-C <path>`, `-c <k=v>`,
/// `--git-dir <p>`, …) so a structured call like `git -C /repo push` matches
/// `git push*` instead of escaping to the default (a fail-*open* when the
/// default is `allow`). Every other command keeps the plain
/// whitespace-normalized form — unchanged V1 behavior.
///
/// Scope (documented): only `git`'s global grammar is modeled (it is the
/// canonical structured-argv case). Env-prefixed forms (`GIT_DIR=… git push`)
/// are still seen as a non-git program and fall through to the default.
fn canonicalize(segment: &str) -> String {
    let tokens = shell_split(segment);
    let is_git = tokens
        .first()
        .map(|t| t.rsplit('/').next() == Some("git"))
        .unwrap_or(false);
    if !is_git {
        return normalize(segment);
    }
    let start = git_subcommand_index(&tokens);
    let mut out = String::from("git");
    for t in &tokens[start.min(tokens.len())..] {
        out.push(' ');
        out.push_str(t);
    }
    normalize(&out)
}

/// Index of the git subcommand in `tokens` (`tokens[0]` is the `git` program).
/// Skips leading global options, consuming the value of the value-taking ones
/// (`-C`, `-c`, `--git-dir`, `--work-tree`, `--namespace`, `--super-prefix`);
/// `--opt=value` and bare flags consume only themselves.
fn git_subcommand_index(tokens: &[String]) -> usize {
    const VALUE_TAKING: &[&str] = &[
        "-C",
        "-c",
        "--git-dir",
        "--work-tree",
        "--namespace",
        "--super-prefix",
    ];
    let mut i = 1;
    while i < tokens.len() {
        if !tokens[i].starts_with('-') {
            break; // the subcommand
        }
        i += if VALUE_TAKING.contains(&tokens[i].as_str()) {
            2
        } else {
            1
        };
    }
    i
}

/// Minimal POSIX-ish word splitter recovering argv for canonicalization: splits
/// on unquoted whitespace, honoring single quotes (literal), double quotes
/// (`\` escapes the next char), and backslash escapes outside quotes. Quote
/// characters are removed; it does **no** variable/glob expansion.
fn shell_split(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut has = false;
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_single {
            if c == '\'' {
                in_single = false;
            } else {
                cur.push(c);
            }
            i += 1;
        } else if in_double {
            if c == '\\' && i + 1 < chars.len() {
                cur.push(chars[i + 1]);
                i += 2;
            } else if c == '"' {
                in_double = false;
                i += 1;
            } else {
                cur.push(c);
                i += 1;
            }
        } else {
            match c {
                '\'' => {
                    in_single = true;
                    has = true;
                    i += 1;
                }
                '"' => {
                    in_double = true;
                    has = true;
                    i += 1;
                }
                '\\' if i + 1 < chars.len() => {
                    cur.push(chars[i + 1]);
                    has = true;
                    i += 2;
                }
                c if c.is_whitespace() => {
                    if has {
                        tokens.push(std::mem::take(&mut cur));
                        has = false;
                    }
                    i += 1;
                }
                _ => {
                    cur.push(c);
                    has = true;
                    i += 1;
                }
            }
        }
    }
    if has {
        tokens.push(cur);
    }
    tokens
}

/// Failure modes for producing or loading a [`Policy`].
#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("policy file not found: {0}")]
    NotFound(PathBuf),
    #[error("policy I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("policy parse error: {0}")]
    Parse(String),
    #[error("policy provider unavailable: {reason}")]
    Unavailable { reason: String },
}

/// Context handed to a [`PolicyProvider`] when resolving a policy.
///
/// The OSS impl (`TomlPolicyLoader`) ignores this — the TOML file *is* the
/// resolved policy. The fields exist for the enterprise RFC-driven compiler,
/// which derives a policy from the agent and working directory in scope.
///
/// Marked `#[non_exhaustive]` so future fields (e.g. an `rfc_id` once the
/// enterprise `RfcPolicyCompiler` exists) can be added without breaking
/// external construction sites — construct via [`PolicyContext::new`], never a
/// struct literal.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PolicyContext {
    pub agent: String,
    pub cwd: PathBuf,
}

impl PolicyContext {
    pub fn new(agent: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            agent: agent.into(),
            cwd: cwd.into(),
        }
    }
}

/// Produces the [`Policy`] the cage will run under.
///
/// OSS ships `TomlPolicyLoader`; an enterprise build swaps in an RFC-driven
/// compiler behind this trait with zero OSS edits — the same open-core seam as
/// [`crate::ForgeBuilder`]. Resolving a file is sync and fast, so the trait is
/// sync; revisit only if a concrete impl genuinely needs async.
pub trait PolicyProvider: Send + Sync {
    fn resolve(&self, ctx: &PolicyContext) -> Result<Policy, PolicyError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Project a decision back to the legacy `(capability, verdict)` pair so the
    /// behavior-preserving assertions below read exactly as before the reshape.
    fn cv(d: PolicyDecision<'_>) -> (Option<&str>, Verdict) {
        let verdict = match d {
            PolicyDecision::Allow { .. } => Verdict::Allow,
            PolicyDecision::Deny { .. } => Verdict::Deny,
            PolicyDecision::Ask(_) => Verdict::Ask,
        };
        (d.capability(), verdict)
    }

    fn sample() -> Policy {
        Policy {
            default_verdict: Verdict::Ask,
            caps: ClassCaps::default(),
            workspace: WorkspaceScope {
                root: PathBuf::from("."),
            },
            capabilities: vec![
                Capability {
                    name: "git.commit".into(),
                    matches: vec!["git commit*".into()],
                    verdict: Verdict::Allow,
                    sensitivity: Sensitivity::Sensitive,
                },
                Capability {
                    name: "git.push".into(),
                    matches: vec!["git push*".into()],
                    verdict: Verdict::Deny,
                    sensitivity: Sensitivity::Sensitive,
                },
                Capability {
                    name: "pkg.install".into(),
                    matches: vec![
                        "npm install*".into(),
                        "pnpm install*".into(),
                        "yarn add*".into(),
                    ],
                    verdict: Verdict::Ask,
                    sensitivity: Sensitivity::Sensitive,
                },
            ],
        }
    }

    const SAMPLE_TOML: &str = r#"
default_verdict = "ask"

[workspace]
root = "."

[[capabilities]]
name = "git.commit"
matches = ["git commit*"]
verdict = "allow"

[[capabilities]]
name = "git.push"
matches = ["git push*"]
verdict = "deny"

[[capabilities]]
name = "pkg.install"
matches = ["npm install*", "pnpm install*", "yarn add*"]
verdict = "ask"
"#;

    #[test]
    fn toml_round_trip() {
        let parsed: Policy = toml::from_str(SAMPLE_TOML).expect("parse sample toml");
        assert_eq!(parsed, sample());
        // re-serialize and re-parse: serialization is valid and stable.
        let reserialized = toml::to_string(&parsed).expect("serialize");
        let reparsed: Policy = toml::from_str(&reserialized).expect("reparse");
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn caps_default_all_false_when_omitted() {
        // A policy that omits `[caps]` grants no class (fail-closed).
        let parsed: Policy = toml::from_str(SAMPLE_TOML).expect("parse");
        assert_eq!(parsed.caps, ClassCaps::default());
        assert!(!parsed.caps.read && !parsed.caps.write && !parsed.caps.exec);
    }

    #[test]
    fn caps_round_trip_and_field_order() {
        let toml = r#"
default_verdict = "deny"

[caps]
read = true
write = true
exec = false

[workspace]
root = "."
"#;
        let parsed: Policy = toml::from_str(toml).expect("parse caps");
        assert!(parsed.caps.read && parsed.caps.write && !parsed.caps.exec);
        // Re-serialization keeps the scalar before the tables (valid TOML): the
        // `[caps]` table must serialize after `default_verdict` and reparse equal.
        let reserialized = toml::to_string(&parsed).expect("serialize");
        let reparsed: Policy = toml::from_str(&reserialized).expect("reparse");
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn default_verdict_when_omitted() {
        let toml = "[workspace]\nroot = \".\"\n";
        let parsed: Policy = toml::from_str(toml).expect("parse");
        assert!(parsed.capabilities.is_empty());
        assert_eq!(parsed.default_verdict, Verdict::Ask);
    }

    #[test]
    fn evaluate_deny_match() {
        let p = sample();
        assert_eq!(
            cv(p.evaluate_match("git push origin main")),
            (Some("git.push"), Verdict::Deny)
        );
    }

    #[test]
    fn evaluate_allow_match() {
        let p = sample();
        assert_eq!(
            cv(p.evaluate_match("git commit -m x")),
            (Some("git.commit"), Verdict::Allow)
        );
    }

    #[test]
    fn evaluate_multi_pattern_capability() {
        let p = sample();
        assert_eq!(
            cv(p.evaluate_match("pnpm install")),
            (Some("pkg.install"), Verdict::Ask)
        );
        assert_eq!(
            cv(p.evaluate_match("yarn add left-pad")),
            (Some("pkg.install"), Verdict::Ask)
        );
    }

    #[test]
    fn evaluate_default_fallthrough() {
        let p = sample();
        assert_eq!(cv(p.evaluate_match("rm -rf /")), (None, Verdict::Ask));
    }

    #[test]
    fn evaluate_normalizes_whitespace() {
        let p = sample();
        assert_eq!(
            cv(p.evaluate_match("  git    push   origin ")),
            (Some("git.push"), Verdict::Deny)
        );
    }

    #[test]
    fn deny_and_allow_are_terminal_variants() {
        // The reshape's point: the three tiers are distinct *variants*, not a
        // flat verdict — Allow/Deny terminal, Ask wrapping an Adjustable.
        let p = sample();
        assert!(matches!(
            p.evaluate_match("git push origin"),
            PolicyDecision::Deny {
                capability: Some("git.push"),
                ..
            }
        ));
        assert!(matches!(
            p.evaluate_match("git commit -m x"),
            PolicyDecision::Allow {
                capability: Some("git.commit"),
                ..
            }
        ));
        assert!(matches!(
            p.evaluate_match("rm -rf /"),
            PolicyDecision::Ask(_)
        ));
    }

    #[test]
    fn ask_tier_is_adjustable_and_resolves() {
        // The Ask tier is the ONLY one carrying an Adjustable; its resolve is the
        // sole path to a terminal verdict (Auto→Allow, Ask→Deny).
        let p = sample();
        match p.evaluate_match("pnpm install") {
            PolicyDecision::Ask(a) => {
                assert_eq!(a.capability, Some("pkg.install"));
                assert_eq!(a.resolve(AskOutcome::Auto), Verdict::Allow);
                assert_eq!(a.resolve(AskOutcome::Ask), Verdict::Deny);
            }
            other => panic!("expected Ask, got {other:?}"),
        }
    }

    #[test]
    fn toml_parsed_evaluates_unchanged() {
        // On-disk schema is unchanged: a parsed policy evaluates identically.
        let parsed: Policy = toml::from_str(SAMPLE_TOML).expect("parse");
        assert_eq!(
            cv(parsed.evaluate_match("git push origin main")),
            (Some("git.push"), Verdict::Deny)
        );
        assert_eq!(
            cv(parsed.evaluate_match("git commit -m x")),
            (Some("git.commit"), Verdict::Allow)
        );
        assert_eq!(
            cv(parsed.evaluate_match("unmatched cmd")),
            (None, Verdict::Ask)
        );
    }

    #[test]
    fn evaluate_first_capability_wins() {
        let p = Policy {
            default_verdict: Verdict::Ask,
            caps: ClassCaps::default(),
            workspace: WorkspaceScope {
                root: PathBuf::from("."),
            },
            capabilities: vec![
                Capability {
                    name: "a".into(),
                    matches: vec!["git*".into()],
                    verdict: Verdict::Allow,
                    sensitivity: Sensitivity::Sensitive,
                },
                Capability {
                    name: "b".into(),
                    matches: vec!["git push*".into()],
                    verdict: Verdict::Deny,
                    sensitivity: Sensitivity::Sensitive,
                },
            ],
        };
        // "a" is declared first and also matches "git push" → wins.
        assert_eq!(
            cv(p.evaluate_match("git push")),
            (Some("a"), Verdict::Allow)
        );
    }

    #[test]
    fn evaluate_exact_pattern_no_star() {
        let p = Policy {
            default_verdict: Verdict::Deny,
            caps: ClassCaps::default(),
            workspace: WorkspaceScope {
                root: PathBuf::from("."),
            },
            capabilities: vec![Capability {
                name: "status".into(),
                matches: vec!["git status".into()],
                verdict: Verdict::Allow,
                sensitivity: Sensitivity::Sensitive,
            }],
        };
        assert_eq!(
            cv(p.evaluate_match("git status")),
            (Some("status"), Verdict::Allow)
        );
        // exact pattern does not match a longer command.
        assert_eq!(
            cv(p.evaluate_match("git status --short")),
            (None, Verdict::Deny)
        );
    }

    #[test]
    fn git_global_opts_are_normalized_before_match() {
        // Structured git calls that interpose global options before the
        // subcommand must still resolve to the same capability — otherwise a
        // denied `git push` escapes by writing `git -C /repo push`.
        let p = sample();
        assert_eq!(
            cv(p.evaluate_match("git -C /repo push")),
            (Some("git.push"), Verdict::Deny)
        );
        assert_eq!(
            cv(p.evaluate_match("git -c user.name=x push origin")),
            (Some("git.push"), Verdict::Deny)
        );
        assert_eq!(
            cv(p.evaluate_match("git --git-dir=/r/.git --work-tree=/r push")),
            (Some("git.push"), Verdict::Deny)
        );
        // absolute git path is canonicalized to the bare program too.
        assert_eq!(
            cv(p.evaluate_match("/usr/bin/git -C /repo push")),
            (Some("git.push"), Verdict::Deny)
        );
    }

    #[test]
    fn git_normalizer_no_false_positive_on_quoted_arg() {
        // The word "push" inside a commit message is an argument, not the
        // subcommand — it must stay an allowed `git commit`, never a deny.
        let p = sample();
        assert_eq!(
            cv(p.evaluate_match(r#"git commit -m "please push it""#)),
            (Some("git.commit"), Verdict::Allow)
        );
    }

    #[test]
    fn compound_command_most_restrictive_wins() {
        let p = sample();
        // a denied segment hidden behind an allowed prefix is still denied.
        assert_eq!(
            cv(p.evaluate_match("echo ok && git push origin main")),
            (Some("git.push"), Verdict::Deny)
        );
        // every chaining operator splits.
        assert_eq!(cv(p.evaluate_match("echo ok ; git push")).1, Verdict::Deny);
        assert_eq!(cv(p.evaluate_match("echo ok | git push")).1, Verdict::Deny);
        assert_eq!(cv(p.evaluate_match("echo ok || git push")).1, Verdict::Deny);
        assert_eq!(
            cv(p.evaluate_match("git push & echo done")).1,
            Verdict::Deny
        );
        assert_eq!(cv(p.evaluate_match("echo a\ngit push")).1, Verdict::Deny);
        // deny beats ask too, regardless of order.
        assert_eq!(
            cv(p.evaluate_match("git push && pnpm install")),
            (Some("git.push"), Verdict::Deny)
        );
    }

    #[test]
    fn compound_operators_inside_quotes_do_not_split() {
        // `&&` inside a quoted string is data, not an operator — the whole thing
        // is one (unmatched) segment, so it does not become a hidden `git push`.
        let p = sample();
        assert_eq!(
            cv(p.evaluate_match(r#"echo "a && git push""#)),
            (None, Verdict::Ask)
        );
        assert_eq!(
            cv(p.evaluate_match("echo 'a && git push'")),
            (None, Verdict::Ask)
        );
    }

    #[test]
    fn sensitivity_defaults_to_sensitive_when_absent() {
        // Existing policies have no `sensitivity` key → fail-closed Sensitive,
        // so an un-annotated capability is never silently auto-promotable.
        let parsed: Policy = toml::from_str(SAMPLE_TOML).expect("parse");
        assert!(!parsed.capabilities.is_empty());
        for cap in &parsed.capabilities {
            assert_eq!(
                cap.sensitivity,
                Sensitivity::Sensitive,
                "absent sensitivity must default to Sensitive for {}",
                cap.name
            );
        }
    }

    #[test]
    fn sensitivity_benign_parses_and_round_trips() {
        let toml = r#"
[workspace]
root = "."

[[capabilities]]
name = "git.commit"
matches = ["git commit*"]
verdict = "ask"
sensitivity = "benign"

[[capabilities]]
name = "git.push"
matches = ["git push*"]
verdict = "ask"
sensitivity = "sensitive"
"#;
        let parsed: Policy = toml::from_str(toml).expect("parse");
        assert_eq!(parsed.capabilities[0].sensitivity, Sensitivity::Benign);
        assert_eq!(parsed.capabilities[1].sensitivity, Sensitivity::Sensitive);
        // serialize + reparse is stable (the field round-trips).
        let reserialized = toml::to_string(&parsed).expect("serialize");
        let reparsed: Policy = toml::from_str(&reserialized).expect("reparse");
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn sensitivity_invalid_value_is_a_parse_error_not_a_silent_fallback() {
        // A typo'd / unknown sensitivity (`"garbage"`, `"sensitif"`, `"high"`)
        // must FAIL the parse — never silently map to a variant. The
        // `#[serde(default)]` on the field fires only for an ABSENT key, not for
        // a present-but-invalid value; a two-variant lowercase enum rejects any
        // other string. (Catches the dangerous case where a mistyped value would
        // map to Benign; here it can't reach any variant at all.)
        for bad in ["garbage", "sensitif", "high", "Benign", "BENIGN", ""] {
            let toml = format!(
                "[workspace]\nroot = \".\"\n\n[[capabilities]]\nname = \"x\"\n\
                 matches = [\"y*\"]\nverdict = \"ask\"\nsensitivity = \"{bad}\"\n"
            );
            assert!(
                toml::from_str::<Policy>(&toml).is_err(),
                "sensitivity = {bad:?} must be a parse error, not a silent fallback"
            );
        }
    }

    #[test]
    fn sensitivity_is_inert_in_the_decision_path() {
        // The cage/sh decision path (evaluate_match) must ignore sensitivity in
        // V1: a Deny capability marked benign still denies; an Allow marked
        // sensitive still allows. Sensitivity changes nothing until Atlas reads it.
        let p = Policy {
            default_verdict: Verdict::Ask,
            caps: ClassCaps::default(),
            workspace: WorkspaceScope {
                root: PathBuf::from("."),
            },
            capabilities: vec![Capability {
                name: "git.push".into(),
                matches: vec!["git push*".into()],
                verdict: Verdict::Deny,
                sensitivity: Sensitivity::Benign,
            }],
        };
        assert_eq!(
            cv(p.evaluate_match("git push origin")),
            (Some("git.push"), Verdict::Deny)
        );
    }
}
