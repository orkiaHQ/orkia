// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! Single source of truth for every Orkia command name: which dispatch
//! family serves it, whether it is registered in the typed
//! [`CommandRegistry`](crate::exec::registry::CommandRegistry), whether its
//! bare first token resolves to `Mode::Builtin`, and whether the name
//! collides with a system binary (and under which fallback rule).
//!
//! Derived consumers (set-equality is test-enforced, both directions):
//! - `classifier::is_builtin` / `resolve_mode` — bare first-token lookup.
//! - `repl::dispatch::dispatch_named` — family routing via the arm consts.
//! - `exec::parse::typed_head` — the POSIX bare-collision guard.
//! - completion — first-word candidates and the stable command set.

use std::sync::LazyLock;

/// Which dispatch family serves a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinKind {
    /// Job control / REPL control — `repl::dispatch::dispatch_shell_control`.
    ShellControl,
    /// Auth + backed services — `repl::dispatch::dispatch_auth_services`.
    AuthService,
    /// Mutates REPL-owned state / privileged effects —
    /// `repl::dispatch::dispatch_effectful`.
    Effectful,
    /// Registry-only typed command — reached via `try_parse_exec`, never
    /// `dispatch_named`.
    Typed,
}

/// How a name that collides with a system binary yields to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackRule {
    /// Bare name belongs to the system; only the `orkia `/`ork ` namespace
    /// reaches the builtin (the `ls` model).
    PrefixOnly,
    /// Bare name reaches the builtin when its args parse as Orkia grammar;
    PosixShapeToBrush {
        /// Whether the bare, argument-less form is the builtin.
        bare_is_builtin: bool,
    },
}

/// declared beside its table entry. Every token must match → builtin;
/// any miss → the whole original line goes to brush. Deliberately an
/// allowlist: when in doubt, yield to the system binary.
#[derive(Debug)]
pub struct ShapeGrammar {
    /// `--long` flags taking no value.
    pub long_flags: &'static [&'static str],
    /// `--long <v>` / `--long=v` flags consuming a value.
    pub value_flags: &'static [&'static str],
    /// Verb words that claim the whole line as Orkia when first
    /// (`audit verify <scope>`, `route show`).
    pub subcommands: &'static [&'static str],
    /// `@agent` targets.
    pub allow_at: bool,
    /// `%n` job targets.
    pub allow_percent: bool,
}

/// One row of the table.
#[derive(Debug, Clone, Copy)]
pub struct BuiltinSpec {
    pub name: &'static str,
    pub kind: BuiltinKind,
    /// `Some` when the name shadows a system binary.
    pub collision: Option<FallbackRule>,
    /// Registered in the typed `CommandRegistry`.
    pub typed: bool,
    /// Bare first token resolves to `Mode::Builtin` in the classifier.
    /// `false` for the pure pipeline commands (`where`, `first`, `sort-by`,
    /// `from`) whose bare form is recognized by `try_parse_exec` instead,
    /// and for the collidables (`ls`, `route`, `log`, `login`) whose bare
    /// form belongs to the system (grammar shapes like `route show` are
    /// captured by the typed parser, not the classifier).
    pub bare_builtin: bool,
    /// collidable means no shape gating: `kill` keeps its internal
    /// PID/system fallback, `leave`/`trust` have an empty POSIX grammar
    /// (the colliding binaries take no shapes worth yielding to).
    pub grammar: Option<&'static ShapeGrammar>,
}

const POSIX_BARE_BUILTIN: Option<FallbackRule> = Some(FallbackRule::PosixShapeToBrush {
    bare_is_builtin: true,
});

const fn shell_control(name: &'static str) -> BuiltinSpec {
    BuiltinSpec {
        name,
        kind: BuiltinKind::ShellControl,
        collision: None,
        typed: false,
        bare_builtin: true,
        grammar: None,
    }
}

const fn auth_service(name: &'static str) -> BuiltinSpec {
    BuiltinSpec {
        name,
        kind: BuiltinKind::AuthService,
        collision: None,
        typed: false,
        bare_builtin: true,
        grammar: None,
    }
}

const fn effectful(name: &'static str) -> BuiltinSpec {
    BuiltinSpec {
        name,
        kind: BuiltinKind::Effectful,
        collision: None,
        typed: false,
        bare_builtin: true,
        grammar: None,
    }
}

const fn typed(name: &'static str) -> BuiltinSpec {
    BuiltinSpec {
        name,
        kind: BuiltinKind::Typed,
        collision: None,
        typed: true,
        bare_builtin: true,
        grammar: None,
    }
}

const fn with_collision(spec: BuiltinSpec, rule: Option<FallbackRule>) -> BuiltinSpec {
    BuiltinSpec {
        collision: rule,
        ..spec
    }
}

const fn with_grammar(spec: BuiltinSpec, grammar: &'static ShapeGrammar) -> BuiltinSpec {
    BuiltinSpec {
        grammar: Some(grammar),
        ..spec
    }
}

const fn also_typed(spec: BuiltinSpec) -> BuiltinSpec {
    BuiltinSpec {
        typed: true,
        ..spec
    }
}

/// The bare first token is *not* this builtin — either a pure pipeline
/// command (recognized by `try_parse_exec` instead) or a PrefixOnly
/// collidable whose bare form belongs to the system binary.
const fn bare_excluded(spec: BuiltinSpec) -> BuiltinSpec {
    BuiltinSpec {
        bare_builtin: false,
        ..spec
    }
}

/// `ps`: the merged agents+system view plus its four long flags. Any
/// short flag or bare word (`aux`, `-ef`, `-p N`) is a POSIX shape.
static PS_GRAMMAR: ShapeGrammar = ShapeGrammar {
    long_flags: &["--agents", "--system", "--full", "--json"],
    value_flags: &[],
    subcommands: &[],
    allow_at: false,
    allow_percent: false,
};

/// `whoami`: bare only — any argument belongs to `/usr/bin/whoami`.
static WHOAMI_GRAMMAR: ShapeGrammar = ShapeGrammar {
    long_flags: &[],
    value_flags: &[],
    subcommands: &[],
    allow_at: false,
    allow_percent: false,
};

/// `audit`: the SEAL filter grammar extracted from
/// `seal::audit::parse_args` + the `redact` dispatch arm. A bare word
/// used to scope by project; that form is ambiguous against
/// `/usr/sbin/audit` and now yields to brush — use `--project <p>`,
/// `audit verify <p>`, or the `orkia ` namespace.
static AUDIT_GRAMMAR: ShapeGrammar = ShapeGrammar {
    long_flags: &["--verify", "--deep", "--export"],
    value_flags: &["--job", "--project", "--rfc", "--last"],
    subcommands: &["verify", "redact"],
    allow_at: false,
    allow_percent: false,
};

/// `log`: job-log shapes (`%n`, `@agent`, `--tail N`); everything else
/// (`log show`, `log stream`, bare numerics) belongs to `/usr/bin/log`.
static LOG_GRAMMAR: ShapeGrammar = ShapeGrammar {
    long_flags: &["--json"],
    value_flags: &["--tail"],
    subcommands: &[],
    allow_at: true,
    allow_percent: true,
};

/// `route`: `show` and `@agent` are the Orkia sugar; `-n`, `add`,
/// `get`, … are real `/sbin/route` invocations.
static ROUTE_GRAMMAR: ShapeGrammar = ShapeGrammar {
    long_flags: &[],
    value_flags: &[],
    subcommands: &["show"],
    allow_at: true,
    allow_percent: false,
};

/// Every Orkia command name. `top` is deliberately absent (it shadowed
/// `/usr/bin/top` with an error); `stream` is present (it had a live
/// dispatch arm but no classifier registration — unreachable).
pub const BUILTINS: &[BuiltinSpec] = &[
    // ── shell-control family ──
    shell_control("fg"),
    shell_control("bg"),
    shell_control("stop"),
    with_collision(shell_control("kill"), POSIX_BARE_BUILTIN),
    shell_control("run"),
    shell_control("attach"),
    shell_control("wait"),
    shell_control("disown"),
    shell_control("tui"),
    // ── auth/service family ──
    auth_service("app"),
    // reaches the auth builtin via the namespace prefix.
    bare_excluded(with_collision(
        auth_service("login"),
        Some(FallbackRule::PrefixOnly),
    )),
    auth_service("logout"),
    auth_service("kernel"),
    auth_service("reasoning"),
    auth_service("contribute"),
    auth_service("team"),
    auth_service("invite"),
    auth_service("members"),
    auth_service("share"),
    with_collision(auth_service("leave"), POSIX_BARE_BUILTIN),
    auth_service("stream"),
    // ── effectful family ──
    effectful("plugin"),
    also_typed(effectful("help")),
    with_grammar(
        with_collision(also_typed(effectful("ps")), POSIX_BARE_BUILTIN),
        &PS_GRAMMAR,
    ),
    effectful("detach"),
    effectful("approve"),
    effectful("deny"),
    with_grammar(
        with_collision(effectful("audit"), POSIX_BARE_BUILTIN),
        &AUDIT_GRAMMAR,
    ),
    effectful("rfc"),
    effectful("operator"),
    effectful("project"),
    effectful("issue"),
    effectful("agent"),
    effectful("cap"),
    with_collision(effectful("trust"), POSIX_BARE_BUILTIN),
    effectful("config"),
    effectful("connect"),
    effectful("disconnect"),
    effectful("migrate-rc"),
    effectful("setup"),
    effectful("tell"),
    effectful("every"),
    // ── typed-registry-only ──
    bare_excluded(with_collision(typed("ls"), Some(FallbackRule::PrefixOnly))),
    bare_excluded(typed("where")),
    bare_excluded(typed("first")),
    bare_excluded(typed("sort-by")),
    bare_excluded(typed("from")),
    typed("version"),
    // so the grammar shapes (`route show`, `log %1`, …) reach the typed
    // builtin while everything else still falls to brush.
    bare_excluded(with_grammar(
        with_collision(
            typed("route"),
            Some(FallbackRule::PosixShapeToBrush {
                bare_is_builtin: false,
            }),
        ),
        &ROUTE_GRAMMAR,
    )),
    typed("briefing"),
    bare_excluded(with_grammar(
        with_collision(
            typed("log"),
            Some(FallbackRule::PosixShapeToBrush {
                bare_is_builtin: false,
            }),
        ),
        &LOG_GRAMMAR,
    )),
    with_grammar(
        with_collision(typed("whoami"), POSIX_BARE_BUILTIN),
        &WHOAMI_GRAMMAR,
    ),
    typed("plan"),
    typed("history"),
    typed("journal"),
    typed("jobs"),
    typed("attention"),
];

/// CLI-only verbs: subcommands of the orkia binary with no REPL
/// dispatch arm (`orkia logs 1`, `orkia update --check`). The binary
/// owns them — the classifier sends `orkia <verb> …` to brush, which
/// execs the real binary, so the namespace claim still resolves
/// in-house instead of erroring. Set-equality with the CLI parser's
/// subcommand list is test-enforced in `bins/orkia/src/args.rs`.
pub const CLI_ONLY: &[&str] = &[
    "bridge",
    "daemon",
    "inspect",
    "logs",
    "mcp-bridge",
    "mcp-pipe",
    "pty-daemon",
    "pty-daemon-stop",
    "update",
];

/// Whether `name` is a CLI-only verb (see [`CLI_ONLY`]).
pub fn is_cli_only(name: &str) -> bool {
    CLI_ONLY.contains(&name)
}

/// The table row for `name`, if any.
pub fn spec_for(name: &str) -> Option<&'static BuiltinSpec> {
    BUILTINS.iter().find(|s| s.name == name)
}

/// Whether a bare first token resolves to `Mode::Builtin`.
pub fn is_builtin(name: &str) -> bool {
    spec_for(name).is_some_and(|s| s.bare_builtin)
}

/// POSIX bare-collision guard for the typed parser: a bare (un-namespaced)
/// stage head must not parse as a typed stage when the name is collidable
/// and its bare form belongs elsewhere — `ls` (PrefixOnly → system) and
/// `ps` (the REPL builtin keeps the legacy merged agents+system view).
/// `route`/`log`/`whoami` are not blocked outright: their `route_for`
pub fn bare_typed_blocked(name: &str) -> bool {
    spec_for(name).is_some_and(|s| {
        s.collision.is_some()
            && (matches!(s.collision, Some(FallbackRule::PrefixOnly))
                || s.kind != BuiltinKind::Typed)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeRoute {
    /// The args parse as Orkia grammar — dispatch the builtin.
    Builtin,
    /// POSIX-shaped — the whole original line belongs to brush.
    Brush,
}

/// The shape verdict for a **bare** invocation of `name` with `args`.
/// `None` when the name carries no shape grammar (not collidable, or its
/// fallback is internal like `kill`'s PID arm) — no gating, dispatch as
/// today. Namespaced lines (`orkia <name> …`) must never consult this:
/// the namespace is an explicit builtin claim.
pub fn route_for(name: &str, args: &[&str]) -> Option<ShapeRoute> {
    let spec = spec_for(name)?;
    let Some(FallbackRule::PosixShapeToBrush { bare_is_builtin }) = spec.collision else {
        return None;
    };
    let grammar = spec.grammar?;
    if args.is_empty() {
        return Some(if bare_is_builtin {
            ShapeRoute::Builtin
        } else {
            ShapeRoute::Brush
        });
    }
    // A leading verb word (`audit verify …`, `route show`) claims the
    // whole line: the rest is the verb's own argument grammar.
    if grammar.subcommands.contains(&args[0]) {
        return Some(ShapeRoute::Builtin);
    }
    let mut iter = args.iter();
    while let Some(tok) = iter.next() {
        if grammar.long_flags.contains(tok) {
            continue;
        }
        if grammar.value_flags.contains(tok) {
            // The value token is the flag's, whatever its shape. A
            // missing value is the builtin's own (loud) error to raise.
            iter.next();
            continue;
        }
        if let Some((flag, _)) = tok.split_once('=')
            && grammar.value_flags.contains(&flag)
        {
            continue;
        }
        if grammar.allow_at && tok.strip_prefix('@').is_some_and(|n| !n.is_empty()) {
            continue;
        }
        if grammar.allow_percent
            && tok
                .strip_prefix('%')
                .is_some_and(|n| n.parse::<u32>().is_ok())
        {
            continue;
        }
        return Some(ShapeRoute::Brush);
    }
    Some(ShapeRoute::Builtin)
}

static ALL_NAMES: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    let mut names: Vec<&'static str> = BUILTINS.iter().map(|s| s.name).collect();
    names.sort_unstable();
    names
});

/// All table names, sorted — the first-word completion candidates.
pub fn completion_names() -> &'static [&'static str] {
    &ALL_NAMES
}

/// Table names of one dispatch family (test seam for the arm consts).
pub fn names_of_kind(kind: BuiltinKind) -> Vec<&'static str> {
    BUILTINS
        .iter()
        .filter(|s| s.kind == kind)
        .map(|s| s.name)
        .collect()
}

/// Table names registered in the typed `CommandRegistry`.
pub fn typed_names() -> Vec<&'static str> {
    BUILTINS
        .iter()
        .filter(|s| s.typed)
        .map(|s| s.name)
        .collect()
}

/// candidates when any are close; always points at `orkia help`.
pub fn unknown_builtin_message(name: &str) -> String {
    let nearest = suggestions_for(name);
    if nearest.is_empty() {
        format!("unknown builtin: {name} (run 'orkia help' for the command list)")
    } else {
        format!(
            "unknown builtin: {name} (did you mean: {}?)",
            nearest.join(", ")
        )
    }
}

/// Nearest table names for an unknown input — prefix matches rank
/// first, then edit distance ≤ 2. At most three.
fn suggestions_for(input: &str) -> Vec<&'static str> {
    if input.is_empty() {
        return Vec::new();
    }
    let mut ranked: Vec<(usize, &'static str)> = BUILTINS
        .iter()
        .map(|s| s.name)
        .chain(CLI_ONLY.iter().copied())
        .filter_map(|n| {
            if n.starts_with(input) {
                return Some((0, n));
            }
            let d = edit_distance(input, n);
            (d <= 2).then_some((d, n))
        })
        .collect();
    ranked.sort_unstable();
    ranked.into_iter().map(|(_, n)| n).take(3).collect()
}

/// Plain Levenshtein distance, two-row DP. Inputs are command-name
/// sized, so the O(a×b) cost is trivial.
fn edit_distance(a: &str, b: &str) -> usize {
    let b_chars: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b_chars.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1; b_chars.len() + 1];
        for (j, &cb) in b_chars.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b_chars.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn set<'a>(names: &[&'a str]) -> BTreeSet<&'a str> {
        names.iter().copied().collect()
    }

    #[test]
    fn no_duplicate_names() {
        let mut seen = BTreeSet::new();
        for spec in BUILTINS {
            assert!(
                seen.insert(spec.name),
                "duplicate table entry: {}",
                spec.name
            );
        }
    }

    #[test]
    fn cli_only_names_are_not_builtins() {
        // A name can't be both: a table entry would shadow the CLI verb
        // (the classifier checks CLI_ONLY before the builtin lookup).
        for name in CLI_ONLY {
            assert!(
                spec_for(name).is_none(),
                "{name} is both CLI_ONLY and a table builtin"
            );
        }
    }

    #[test]
    fn top_is_not_in_the_table() {
        // bare `top` falls through to brush and reaches the system binary.
        assert!(spec_for("top").is_none());
    }

    #[test]
    fn stream_is_reachable() {
        // `stream` had a live dispatch arm but no classifier registration.
        let spec = spec_for("stream").expect("stream must be in the table");
        assert_eq!(spec.kind, BuiltinKind::AuthService);
        assert!(spec.bare_builtin);
    }

    // ── exhaustiveness: table ↔ dispatch arm consts, both directions ──

    #[test]
    fn shell_control_arms_match_table() {
        assert_eq!(
            set(&names_of_kind(BuiltinKind::ShellControl)),
            set(crate::repl::SHELL_CONTROL_ARMS),
        );
    }

    #[test]
    fn auth_service_arms_match_table() {
        assert_eq!(
            set(&names_of_kind(BuiltinKind::AuthService)),
            set(crate::repl::AUTH_SERVICE_ARMS),
        );
    }

    #[test]
    fn effectful_arms_match_table() {
        assert_eq!(
            set(&names_of_kind(BuiltinKind::Effectful)),
            set(crate::repl::EFFECTFUL_ARMS),
        );
    }

    // ── exhaustiveness: table ↔ typed registry, both directions ──

    #[test]
    fn typed_names_match_registry() {
        let registry = crate::exec::registry::CommandRegistry::with_pilots();
        let registry_names: BTreeSet<String> = registry.names().into_iter().collect();
        let table_names: BTreeSet<String> = typed_names().into_iter().map(str::to_string).collect();
        assert_eq!(table_names, registry_names);
    }

    #[test]
    fn completion_candidates_are_the_table() {
        assert_eq!(set(completion_names()), set(&ALL_NAMES));
        // The phantom `agents` completion candidate is gone.
        assert!(!completion_names().contains(&"agents"));
    }

    #[test]
    fn unknown_builtin_message_suggests_nearest_names() {
        // at help. Either way the error carries the unknown name.
        let near = unknown_builtin_message("setpu");
        assert!(near.contains("unknown builtin: setpu"), "{near}");
        assert!(near.contains("setup"), "{near}");
        let far = unknown_builtin_message("zzqqxxyy");
        assert!(far.contains("unknown builtin: zzqqxxyy"), "{far}");
        assert!(far.contains("orkia help"), "{far}");
    }

    #[test]
    fn posix_collision_guard_blocks_bare_system_owned_names() {
        // Replaces parse.rs's POSIX_COLLISIONS: among registry names, the
        // PrefixOnly collidable `ls` and bare `ps` (legacy REPL view) are
        // kept off the typed parser unconditionally. `route`/`log`/`whoami`
        // invocation.
        let blocked: Vec<&str> = typed_names()
            .into_iter()
            .filter(|n| bare_typed_blocked(n))
            .collect();
        assert_eq!(set(&blocked), set(&["ls", "ps"]));
    }

    #[test]
    fn a_prefixed_names_are_gone() {
        for old in ["aattach", "aroute", "aconnect", "adisconnect"] {
            assert!(spec_for(old).is_none(), "{old} must not be in the table");
        }
    }

    #[test]
    fn bare_login_log_route_leave_the_classifier() {
        // the builtin.
        for name in ["login", "log", "route"] {
            let spec = spec_for(name).expect("in table");
            assert!(!spec.bare_builtin, "bare {name} must not be a builtin");
        }
        assert_eq!(
            spec_for("login").expect("in table").collision,
            Some(FallbackRule::PrefixOnly),
            "login has no bare grammar shapes"
        );
        // the bare default itself is unchanged (bare_is_builtin: false).
        for name in ["log", "route"] {
            let spec = spec_for(name).expect("in table");
            assert_eq!(
                spec.collision,
                Some(FallbackRule::PosixShapeToBrush {
                    bare_is_builtin: false
                }),
                "{name} must be shape-gated with a brush bare default"
            );
            assert!(spec.grammar.is_some(), "{name} must declare a grammar");
        }
        // `logout` is untouched: no system binary collides.
        let logout = spec_for("logout").expect("in table");
        assert!(logout.bare_builtin);
        assert!(logout.collision.is_none());
    }

    fn shape(name: &str, args: &[&str]) -> Option<ShapeRoute> {
        route_for(name, args)
    }

    #[test]
    fn ps_shapes() {
        assert_eq!(shape("ps", &[]), Some(ShapeRoute::Builtin));
        assert_eq!(shape("ps", &["--json"]), Some(ShapeRoute::Builtin));
        assert_eq!(
            shape("ps", &["--agents", "--full"]),
            Some(ShapeRoute::Builtin)
        );
        assert_eq!(shape("ps", &["aux"]), Some(ShapeRoute::Brush));
        assert_eq!(shape("ps", &["-ef"]), Some(ShapeRoute::Brush));
        assert_eq!(shape("ps", &["-a"]), Some(ShapeRoute::Brush));
        assert_eq!(shape("ps", &["-p", "1"]), Some(ShapeRoute::Brush));
    }

    #[test]
    fn whoami_shapes() {
        assert_eq!(shape("whoami", &[]), Some(ShapeRoute::Builtin));
        assert_eq!(shape("whoami", &["-u"]), Some(ShapeRoute::Brush));
        assert_eq!(shape("whoami", &["anything"]), Some(ShapeRoute::Brush));
    }

    #[test]
    fn audit_shapes() {
        assert_eq!(shape("audit", &[]), Some(ShapeRoute::Builtin));
        assert_eq!(shape("audit", &["--verify"]), Some(ShapeRoute::Builtin));
        assert_eq!(
            shape("audit", &["--job", "3", "--deep"]),
            Some(ShapeRoute::Builtin)
        );
        assert_eq!(
            shape("audit", &["--project=myproj"]),
            Some(ShapeRoute::Builtin)
        );
        // Verb forms claim the whole line, scope word included.
        assert_eq!(
            shape("audit", &["verify", "myproj"]),
            Some(ShapeRoute::Builtin)
        );
        assert_eq!(
            shape("audit", &["redact", "ev-1", "--reason", "x"]),
            Some(ShapeRoute::Builtin)
        );
        // Short flags are /usr/sbin/audit's; a bare word is ambiguous
        // (legacy project scope vs audit's file arg) — yield.
        assert_eq!(shape("audit", &["-e"]), Some(ShapeRoute::Brush));
        assert_eq!(shape("audit", &["myproject"]), Some(ShapeRoute::Brush));
    }

    #[test]
    fn log_shapes() {
        assert_eq!(shape("log", &[]), Some(ShapeRoute::Brush));
        assert_eq!(shape("log", &["%1"]), Some(ShapeRoute::Builtin));
        assert_eq!(shape("log", &["@faye"]), Some(ShapeRoute::Builtin));
        assert_eq!(
            shape("log", &["%1", "--tail", "10"]),
            Some(ShapeRoute::Builtin)
        );
        assert_eq!(shape("log", &["show"]), Some(ShapeRoute::Brush));
        assert_eq!(
            shape("log", &["stream", "--level", "debug"]),
            Some(ShapeRoute::Brush)
        );
        // A bare numeric is /usr/bin/log's to reject — `%n` or the
        // namespace reach the job log.
        assert_eq!(shape("log", &["99"]), Some(ShapeRoute::Brush));
    }

    #[test]
    fn route_shapes() {
        assert_eq!(shape("route", &[]), Some(ShapeRoute::Brush));
        assert_eq!(shape("route", &["show"]), Some(ShapeRoute::Builtin));
        assert_eq!(shape("route", &["@faye"]), Some(ShapeRoute::Builtin));
        assert_eq!(
            shape("route", &["-n", "get", "default"]),
            Some(ShapeRoute::Brush)
        );
        assert_eq!(
            shape("route", &["add", "10.0.0.0/8", "gw"]),
            Some(ShapeRoute::Brush)
        );
    }

    #[test]
    fn ungated_names_have_no_shape_verdict() {
        // `kill` keeps its internal PID/system fallback; `leave`/`trust`
        // declare an empty POSIX grammar (no behavior change); `ls` is
        // PrefixOnly; `tell` doesn't collide at all.
        for (name, args) in [
            ("kill", &["12345"][..]),
            ("kill", &["%1"][..]),
            ("leave", &[][..]),
            ("trust", &["@faye"][..]),
            ("ls", &["-la"][..]),
            ("tell", &["@a", "hi"][..]),
        ] {
            assert_eq!(shape(name, args), None, "{name} must be ungated");
        }
    }
}
