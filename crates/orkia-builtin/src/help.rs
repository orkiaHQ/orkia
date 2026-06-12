// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use orkia_shell_types::BlockContent;

pub fn help() -> Vec<BlockContent> {
    vec![
        BlockContent::SystemInfo(" ORKIA SHELL — the process manager for the agent era".into()),
        BlockContent::SystemInfo(" AUGMENTED COMMANDS (Unix + agents)".into()),
        BlockContent::Text("   ps                  processes and agent jobs".into()),
        BlockContent::Text("   jobs                bash-style list of background jobs".into()),
        BlockContent::Text(
            "   kill <target>       send a signal to a job (accepts %N) or process".into(),
        ),
        BlockContent::Text("   stop <target>       graceful stop of a job".into()),
        BlockContent::Text(
            "   fg <target>         foreground a job (accepts %N, %+, %-, %prefix)".into(),
        ),
        BlockContent::Text("   bg <target>         background a job (accepts %N)".into()),
        BlockContent::Text("   wait [target]       block until job(s) complete".into()),
        BlockContent::Text("   history             command and agent history".into()),
        BlockContent::SystemInfo(" AGENTIC COMMANDS".into()),
        BlockContent::Text("   run <cmd>           spawn a background job".into()),
        BlockContent::Text("   attach <target>     foreground an agent job (@name, %N)".into()),
        BlockContent::Text("   detach              detach from foreground (also Ctrl-Z)".into()),
        BlockContent::Text("   tell <target> <msg> send a message to a running agent".into()),
        BlockContent::Text("   approve / deny      handle agent approvals".into()),
        BlockContent::Text("   attention           inspect prompt-detector state".into()),
        BlockContent::Text("   orkia route         agent routing table".into()),
        BlockContent::Text("   agent               manage agent definitions".into()),
        BlockContent::Text("   connect / disconnect     backend connection".into()),
        BlockContent::SystemInfo(" OBSERVABILITY".into()),
        BlockContent::Text(
            "   journal             unified event journal (hooks/tells/lifecycle)".into(),
        ),
        BlockContent::Text("   operator            monitor local/cross-session drift".into()),
        BlockContent::Text(
            "   operator ask <q>    grounded projection over reasoning/journal evidence".into(),
        ),
        BlockContent::Text(
            "   operator open <ref> resolve projection citations (--json supported)".into(),
        ),
        BlockContent::Text("   audit               verify the Shell Audit Log chain".into()),
        BlockContent::SystemInfo(" WORKFLOW".into()),
        BlockContent::Text("   rfc                 manage RFCs and delegate to agents".into()),
        BlockContent::Text("   issue               manage issues".into()),
        BlockContent::Text("   project             manage projects".into()),
        BlockContent::Text("   brief / briefing    briefs and session reports".into()),
        BlockContent::SystemInfo(" TEAM".into()),
        BlockContent::Text("   team ls             list teams in current workspace".into()),
        BlockContent::Text("   team show <id>      team details and member list".into()),
        BlockContent::Text("   team create <id>    create a new team (admin)".into()),
        BlockContent::Text("   team rm <id>        delete a team (owner)".into()),
        BlockContent::Text("   team cd <id>        set current team scope".into()),
        BlockContent::Text("   team pwd            show current team".into()),
        BlockContent::Text("   team refresh        re-bootstrap the local team cache".into()),
        BlockContent::Text("   invite create <email>   invite a user (admin)".into()),
        BlockContent::Text("   invite ls           list pending invites (admin)".into()),
        BlockContent::Text("   invite revoke <n>   revoke a pending invite".into()),
        BlockContent::Text(
            "   invite accept <n>   accept an invite (auto-switches workspace)".into(),
        ),
        BlockContent::Text("   members ls          list members (workspace or --team)".into()),
        BlockContent::Text("   members add <id>    add a member (--role R [--team T])".into()),
        BlockContent::Text("   members rm <id>     remove a member ([--team T])".into()),
        BlockContent::Text("   members role <id>   change a member's role".into()),
        BlockContent::Text(
            "   share project <p> <ws>   share a project to another workspace".into(),
        ),
        BlockContent::Text("   share issue <i> <ws>     share an issue".into()),
        BlockContent::Text("   share unshare ...        reverse a share".into()),
        BlockContent::Text("   share ls            list shared projects/issues".into()),
        BlockContent::Text("   leave               leave the current workspace".into()),
        BlockContent::SystemInfo(" SHELL".into()),
        BlockContent::Text("   tui                 switch to TUI mode".into()),
        BlockContent::Text("   config              shell configuration".into()),
        BlockContent::Text("   setup               first-time setup".into()),
        BlockContent::Text("   version             shell version".into()),
        BlockContent::Text("   help                this help".into()),
        BlockContent::SystemInfo(" DELEGATION".into()),
        BlockContent::Text("   @agent message      delegate directly to a named agent".into()),
        BlockContent::Text("   @a cmd | @b cmd     pipeline between agents".into()),
        BlockContent::Text("   natural language    auto-classified and routed".into()),
        BlockContent::SystemInfo(" PASSTHROUGH".into()),
        BlockContent::Text("   !command            force system shell execution".into()),
        BlockContent::Text("   any $PATH command   auto-detected and passed through".into()),
        BlockContent::SystemInfo(
            " The 'orkia' prefix forces the builtin; bare collidable names (ps, log, login, route) extend their system twins.".into(),
        ),
    ]
}

pub fn version() -> Vec<BlockContent> {
    vec![BlockContent::SystemInfo(format!(
        "orkia v{}",
        env!("CARGO_PKG_VERSION")
    ))]
}
