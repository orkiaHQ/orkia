// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! The `trust` builtin — human unlock grants + the effective-trust view
//!
//! Trust Atlas auto-promotes some `Ask` decisions within already-granted bounds.
//! A **benign** capability may auto-promote on evidence alone; a **sensitive**
//! one never does — it needs a deliberate, cold, recorded human **unlock**. This
//! builtin is where the human grants and revokes those unlocks, and where they
//! read the effective per-`(project × capability)` state.
//!
//! - `trust [show] [@agent]` — the effective-trust view: per capability, its base
//!   verdict, declared sensitivity, and whether a sensitive cap is unlocked **in
//!   this project**. (Session `Auto` promotions are computed by the enterprise
//!   scorer; OSS shows the base authority + grants, which is all it decides.)
//! - `trust unlock @agent <capability>` — grant a durable unlock for the current
//!   project. **Two-step, cold confirmation:** without `--yes` it only shows what
//!   would be granted and writes nothing; `trust unlock @agent <cap> --yes`
//!   commits. Persisted to the [`UnlockStore`] the cage reads, and recorded as a
//!   `trust.unlock` SEAL event (auditable, revocable).
//! - `trust lock @agent <capability>` — revoke it (`trust.lock` SEAL event);
//!   fail-safe, so no confirmation needed.
//!
//! The grant is **out of band** by design: it never prompts inline during agent
//! execution (that would be consent fatigue on a durable grant). The deliberate
//! act is the explicit `--yes` confirmation — not a blocking `[y/N]`, which would
//! seize the REPL's read loop (CLAUDE.md #1); the two-step delivers the same cold
//! deliberateness. `lock` reverses a grant.

use orkia_shell_types::{
    AgentInfo, BlockContent, CellStyle, Outcome, PendingStore, Policy, ProjectId, Sensitivity,
    TrustKey, UnlockStore, Verdict, resolve_project_id,
};

use super::*;
use crate::job::JobId;

impl Repl {
    /// `trust` · `trust show [@agent]` · `trust pending` · `trust unlock @agent
    /// <cap>` · `trust lock @agent <cap>`.
    pub(crate) fn handle_trust(&self, args: &[String]) -> Outcome {
        match args.first().map(String::as_str) {
            None | Some("show") => self.trust_show(args.get(1).map(String::as_str)),
            Some("pending") => self.trust_pending(),
            Some("unlock") => self.trust_grant(args.get(1..).unwrap_or_default(), true),
            Some("lock") => self.trust_grant(args.get(1..).unwrap_or_default(), false),
            Some(other) => Outcome::Error(format!(
                "trust: unknown subcommand '{other}' (use show, pending, unlock, lock)"
            )),
        }
    }

    /// `trust pending` — the cold-review list: per agent, the **sensitive**
    /// `Ask`-tier capabilities in the current project that have **no** unlock yet.
    /// These are the grant decisions awaiting a human. Those whose evidence has
    /// crossed the auto-promotion threshold (computed by the enterprise scorer and
    /// surfaced into the pending list at spawn) are marked 🔥 **eligible**; the
    /// rest (🔒) are candidates still accumulating evidence.
    fn trust_pending(&self) -> Outcome {
        let Some(project) = self.project_opt() else {
            return Outcome::Error(
                "trust pending: not inside a project (no git root) — grants are project-scoped"
                    .into(),
            );
        };
        let unlocks = UnlockStore::load(&unlocks_path(&self.config.data_dir));
        let pending = PendingStore::load(&pending_path(&self.config.data_dir));
        let mut blocks = vec![BlockContent::SystemInfo(format!(
            "trust pending — project {} (🔥 = evidence threshold met, 🔒 = candidate)",
            project.0
        ))];
        let mut any = false;
        for a in &self.agents {
            let Some(policy) = self.read_trust_policy(&a.name) else {
                continue;
            };
            let waiting: Vec<&str> = policy
                .capabilities
                .iter()
                .filter(|c| c.verdict == Verdict::Ask && c.sensitivity == Sensitivity::Sensitive)
                .filter(|c| {
                    !unlocks.has(&TrustKey {
                        agent: a.name.clone(),
                        project: project.clone(),
                        capability: c.name.clone(),
                    })
                })
                .map(|c| c.name.as_str())
                .collect();
            if waiting.is_empty() {
                continue;
            }
            any = true;
            blocks.push(BlockContent::Notice {
                style: CellStyle::Accent,
                text: format!(" @{}", a.name),
            });
            for cap in waiting {
                let eligible = pending.has(&TrustKey {
                    agent: a.name.clone(),
                    project: project.clone(),
                    capability: cap.to_string(),
                });
                let mark = if eligible { "🔥" } else { "🔒" };
                let note = if eligible {
                    " (evidence threshold met)"
                } else {
                    ""
                };
                blocks.push(BlockContent::Notice {
                    style: if eligible {
                        CellStyle::Good
                    } else {
                        CellStyle::Plain
                    },
                    text: format!(
                        "   {mark} {cap:<18}{note} — grant with `trust unlock @{} {cap}`",
                        a.name
                    ),
                });
            }
        }
        if !any {
            blocks.push(BlockContent::SystemInfo(
                "   (nothing pending — every sensitive cap is either unlocked or undeclared)"
                    .into(),
            ));
        }
        Outcome::BuiltinOutput { blocks }
    }

    /// Grant (`unlock`) or revoke (`lock`) a durable human unlock for the current
    /// project, persist it, and record a SEAL event. Fail-closed: an unknown
    /// agent, an unresolved project, or an undeclared/benign capability aborts
    /// without writing anything.
    fn trust_grant(&self, rest: &[String], grant: bool) -> Outcome {
        let verb = if grant { "unlock" } else { "lock" };
        // A durable unlock requires an explicit `--yes`/`-y` (cold, deliberate
        // confirmation). `lock` (revocation) is fail-safe and needs none.
        let confirmed = rest.iter().any(|a| a == "--yes" || a == "-y");
        let positional: Vec<String> = rest
            .iter()
            .filter(|a| *a != "--yes" && *a != "-y")
            .cloned()
            .collect();
        let (agent, capability) = match parse_target(&positional, verb) {
            Ok(t) => t,
            Err(e) => return Outcome::Error(e),
        };
        if !self.agents.iter().any(|a| a.name == agent) {
            return Outcome::Error(format!("trust: unknown agent '{agent}'"));
        }
        let project = match self.current_project() {
            Ok(p) => p,
            Err(e) => return Outcome::Error(e),
        };
        if let Err(e) = self.validate_capability(&agent, &capability, grant) {
            return Outcome::Error(e);
        }

        // Cold confirmation gate: a grant of durable autonomy must never be
        // an accidental reflex. A literal blocking `[y/N]` would seize the REPL's
        // sacred read loop (CLAUDE.md #1), so we require an explicit confirmation
        // flag — the same deliberate, out-of-band intent, two-step. The first
        // `trust unlock` shows the consequence and writes nothing; `--yes` commits.
        if grant && !confirmed {
            return Outcome::BuiltinOutput {
                blocks: vec![confirm_prompt(&agent, &capability, &project)],
            };
        }

        let key = TrustKey {
            agent: agent.clone(),
            project: project.clone(),
            capability: capability.clone(),
        };
        let path = unlocks_path(&self.config.data_dir);
        let mut store = UnlockStore::load(&path);
        let changed = if grant {
            let had = store.has(&key);
            store.record(&key);
            !had
        } else {
            store.remove(&key)
        };
        if let Err(e) = store.save(&path) {
            return Outcome::Error(format!("trust: writing {}: {e}", path.display()));
        }

        // Record the grant/revocation as an auditable SEAL event.
        self.emit_audit_event(
            JobId(0),
            &agent,
            if grant { "trust.unlock" } else { "trust.lock" },
            serde_json::json!({
                "agent": agent,
                "project": project.0,
                "capability": capability,
                "action": verb,
            }),
        );

        Outcome::BuiltinOutput {
            blocks: vec![grant_block(grant, changed, &agent, &capability, &project)],
        }
    }

    /// The effective-trust view: per capability, base verdict + sensitivity, and
    /// (for the current project) whether a sensitive capability is unlocked.
    fn trust_show(&self, filter: Option<&str>) -> Outcome {
        let want = filter.map(|s| s.trim_start_matches('@'));
        let project = self.project_opt();
        let unlocks = UnlockStore::load(&unlocks_path(&self.config.data_dir));

        let header = match &project {
            Some(p) => format!("trust — project {}", p.0),
            None => "trust — no project (not in a git root; grants are project-scoped)".into(),
        };
        let mut blocks = vec![BlockContent::SystemInfo(header)];

        let agents: Vec<&AgentInfo> = self
            .agents
            .iter()
            .filter(|a| want.is_none_or(|w| a.name == w))
            .collect();
        if agents.is_empty() {
            return Outcome::Error(match want {
                Some(w) => format!("trust: unknown agent '{w}'"),
                None => "trust: no agents defined".into(),
            });
        }
        for a in agents {
            blocks.push(BlockContent::Notice {
                style: CellStyle::Accent,
                text: format!(" @{}", a.name),
            });
            blocks.extend(self.trust_rows(a, project.as_ref(), &unlocks));
        }
        blocks.push(BlockContent::SystemInfo(
            " unlock = a recorded human grant lets a sensitive cap auto-promote; \
             session Auto promotions are computed by the trust scorer"
                .into(),
        ));
        Outcome::BuiltinOutput { blocks }
    }

    /// One line per declared capability for `agent`: base verdict, sensitivity,
    /// and unlock state in `project` (if any).
    fn trust_rows(
        &self,
        agent: &AgentInfo,
        project: Option<&ProjectId>,
        unlocks: &UnlockStore,
    ) -> Vec<BlockContent> {
        let Some(policy) = self.read_trust_policy(&agent.name) else {
            return vec![BlockContent::SystemInfo(
                "   (no policy on disk — fail-closed: nothing auto-promotes)".into(),
            )];
        };
        if policy.capabilities.is_empty() {
            return vec![BlockContent::SystemInfo(
                "   (no capabilities declared)".into(),
            )];
        }
        policy
            .capabilities
            .iter()
            .map(|c| {
                let unlocked = project.is_some_and(|p| {
                    unlocks.has(&TrustKey {
                        agent: agent.name.clone(),
                        project: p.clone(),
                        capability: c.name.clone(),
                    })
                });
                trust_cap_line(&c.name, c.verdict, c.sensitivity, unlocked)
            })
            .collect()
    }

    /// The stable project for a grant: the git root of the shell's cwd. `Err` when
    /// unresolved — trust is scoped to a stable project, never a bare cwd.
    fn current_project(&self) -> Result<ProjectId, String> {
        self.project_opt().ok_or_else(|| {
            "trust: not inside a project (no git root) — unlocks are scoped to a stable project"
                .into()
        })
    }

    fn project_opt(&self) -> Option<ProjectId> {
        let cwd = self.agent_cwd().or_else(|| std::env::current_dir().ok())?;
        resolve_project_id(&cwd)
    }

    /// Read an agent's own policy for validation/display. `None` when absent —
    /// callers treat that as "cannot validate" (display) or skip the check.
    fn read_trust_policy(&self, agent_name: &str) -> Option<Policy> {
        let path = crate::agent_dir::agent_policy_path(&self.config.data_dir, agent_name);
        let raw = std::fs::read_to_string(path).ok()?;
        toml::from_str(&raw).ok()
    }

    /// Reject a typo'd capability, and reject unlocking a benign one (benign needs
    /// no grant). Skips the check when the policy is not on disk (cannot validate).
    fn validate_capability(
        &self,
        agent: &str,
        capability: &str,
        grant: bool,
    ) -> Result<(), String> {
        let Some(policy) = self.read_trust_policy(agent) else {
            return Ok(());
        };
        let Some(cap) = policy.capabilities.iter().find(|c| c.name == capability) else {
            return Err(format!(
                "trust: capability '{capability}' is not declared in @{agent}'s policy"
            ));
        };
        if grant && cap.sensitivity == Sensitivity::Benign {
            return Err(format!(
                "trust: '{capability}' is benign — it needs no unlock (benign caps auto-promote \
                 within granted bounds); unlock is only for sensitive caps"
            ));
        }
        Ok(())
    }
}

/// `@agent <capability>` from the subcommand tail.
fn parse_target(rest: &[String], verb: &str) -> Result<(String, String), String> {
    let usage = || format!("trust {verb}: usage `trust {verb} @agent <capability>`");
    let agent = rest
        .first()
        .map(|s| s.trim_start_matches('@').to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("trust {verb}: missing @agent — {}", usage()))?;
    let capability = rest
        .get(1)
        .cloned()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("trust {verb}: missing capability — {}", usage()))?;
    Ok((agent, capability))
}

/// Where human-approval unlocks live. Must match the cage's `unlock_store_path()`
/// (`$HOME/.orkia/trust/unlocks.json`) so a grant here is seen at agent spawn —
/// `data_dir` is `~/.orkia` in the standard install.
fn unlocks_path(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("trust").join("unlocks.json")
}

/// Where the eligibility pending list lives — written by the cage at spawn (must
/// match the cage's `pending_store_path()`, `$HOME/.orkia/trust/pending.json`).
fn pending_path(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("trust").join("pending.json")
}

/// The confirmation line for a grant/revocation.
fn grant_block(
    grant: bool,
    changed: bool,
    agent: &str,
    capability: &str,
    project: &ProjectId,
) -> BlockContent {
    let text = match (grant, changed) {
        (true, true) => format!(
            "trust: unlocked {capability} for @{agent} in {} — persists across sessions \
             (revoke with `trust lock @{agent} {capability}`)",
            project.0
        ),
        (true, false) => format!(
            "trust: {capability} for @{agent} in {} was already unlocked",
            project.0
        ),
        (false, true) => format!(
            "trust: locked {capability} for @{agent} in {} — grant revoked",
            project.0
        ),
        (false, false) => format!(
            "trust: {capability} for @{agent} in {} was not unlocked",
            project.0
        ),
    };
    BlockContent::Notice {
        style: if changed {
            CellStyle::Good
        } else {
            CellStyle::Dim
        },
        text,
    }
}

/// The cold confirmation shown when `trust unlock` is run without `--yes`. It
/// states exactly what is being granted, that it persists, and how to confirm or
/// revoke — and the command wrote **nothing** when this is shown.
fn confirm_prompt(agent: &str, capability: &str, project: &ProjectId) -> BlockContent {
    BlockContent::Notice {
        style: CellStyle::Accent,
        text: format!(
            "trust: grant auto-promotion of sensitive capability {capability} for @{agent} in \
             {}? It persists across sessions and lets it auto-promote without asking. Nothing was \
             written. Confirm: `trust unlock @{agent} {capability} --yes` (revoke later: `trust \
             lock @{agent} {capability}`).",
            project.0
        ),
    }
}

/// One capability row in the effective-trust view.
fn trust_cap_line(
    name: &str,
    verdict: Verdict,
    sensitivity: Sensitivity,
    unlocked: bool,
) -> BlockContent {
    let v = match verdict {
        Verdict::Allow => "allow",
        Verdict::Ask => "ask",
        Verdict::Deny => "deny",
    };
    let s = match sensitivity {
        Sensitivity::Benign => "benign",
        Sensitivity::Sensitive => "sensitive",
    };
    // The unlock marker only means something for a sensitive Ask cap.
    let promotable = verdict == Verdict::Ask && sensitivity == Sensitivity::Sensitive;
    let mark = if !promotable {
        "" // benign/allow/deny: unlock is irrelevant
    } else if unlocked {
        "  🔓 unlocked"
    } else {
        "  🔒 needs unlock"
    };
    BlockContent::Notice {
        style: if verdict == Verdict::Deny {
            CellStyle::Dim
        } else {
            CellStyle::Plain
        },
        text: format!("   {name:<18} {v:<6} {s:<10}{mark}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_target_strips_at_and_requires_both() {
        let ok = parse_target(&["@faye".into(), "git.push".into()], "unlock").unwrap();
        assert_eq!(ok, ("faye".to_string(), "git.push".to_string()));
        // Bare name (no @) is also accepted.
        let ok2 = parse_target(&["faye".into(), "git.push".into()], "lock").unwrap();
        assert_eq!(ok2.0, "faye");
        assert!(parse_target(&["@faye".into()], "unlock").is_err());
        assert!(parse_target(&[], "unlock").is_err());
        assert!(parse_target(&["".into(), "git.push".into()], "unlock").is_err());
    }

    #[test]
    fn unlocks_path_is_under_trust_dir() {
        let p = unlocks_path(std::path::Path::new("/home/x/.orkia"));
        assert!(p.ends_with("trust/unlocks.json"));
    }

    #[test]
    fn grant_block_text_reflects_state() {
        let proj = ProjectId("P".into());
        let granted = grant_block(true, true, "faye", "git.push", &proj);
        let already = grant_block(true, false, "faye", "git.push", &proj);
        let revoked = grant_block(false, true, "faye", "git.push", &proj);
        for (b, needle) in [
            (granted, "unlocked"),
            (already, "already unlocked"),
            (revoked, "revoked"),
        ] {
            match b {
                BlockContent::Notice { text, .. } => assert!(text.contains(needle), "{text}"),
                _ => panic!("expected Notice"),
            }
        }
    }

    #[test]
    fn confirm_prompt_states_consequence_and_is_inert() {
        let p = confirm_prompt("faye", "git.push", &ProjectId("P".into()));
        let text = match p {
            BlockContent::Notice { text, .. } => text,
            _ => panic!("expected Notice"),
        };
        assert!(text.contains("--yes"), "{text}");
        assert!(text.contains("Nothing was written"), "{text}");
        assert!(text.contains("persists across sessions"), "{text}");
    }

    #[test]
    fn cap_line_marks_only_sensitive_ask() {
        // sensitive + ask → shows lock state
        let needs = trust_cap_line("git.push", Verdict::Ask, Sensitivity::Sensitive, false);
        let has = trust_cap_line("git.push", Verdict::Ask, Sensitivity::Sensitive, true);
        // benign ask → no lock marker (auto-promotes without a grant)
        let benign = trust_cap_line("git.commit", Verdict::Ask, Sensitivity::Benign, false);
        let text = |b: BlockContent| match b {
            BlockContent::Notice { text, .. } => text,
            _ => panic!("expected Notice"),
        };
        assert!(text(needs).contains("needs unlock"));
        assert!(text(has).contains("unlocked"));
        assert!(!text(benign).contains("unlock"));
    }
}
