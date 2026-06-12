// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `agent.toml [hooks] provider` round-trips through the loader onto
//! `AgentDefinition::hooks_provider`, and `[runtime] type` resolves into
//! `AgentRuntimeKind` (absent ⇒ vendor; invalid sections are skipped by
//! the loader, fail-closed).

use orkia_shell::agent_dir;
use orkia_shell_types::{AgentRuntimeKind, ProviderId};
use tempfile::tempdir;

fn write_agent_toml(dir: &std::path::Path, body: &str) {
    std::fs::create_dir_all(dir).expect("mkdir");
    std::fs::write(dir.join("agent.toml"), body).expect("write");
}

#[test]
fn hooks_provider_is_loaded_when_present() {
    let tmp = tempdir().expect("tempdir");
    write_agent_toml(
        tmp.path(),
        r#"
[agent]
name = "faye"

[hooks]
provider = "claude"
"#,
    );
    let def = agent_dir::load_definition(tmp.path()).expect("load");
    assert_eq!(def.hooks_provider.as_deref(), Some("claude"));
}

#[test]
fn hooks_provider_absent_defaults_to_none() {
    let tmp = tempdir().expect("tempdir");
    write_agent_toml(
        tmp.path(),
        r#"
[agent]
name = "faye"
"#,
    );
    let def = agent_dir::load_definition(tmp.path()).expect("load");
    assert!(def.hooks_provider.is_none());
}

#[test]
fn hooks_section_without_provider_is_none() {
    let tmp = tempdir().expect("tempdir");
    write_agent_toml(
        tmp.path(),
        r#"
[agent]
name = "faye"

[hooks]
"#,
    );
    let def = agent_dir::load_definition(tmp.path()).expect("load");
    assert!(def.hooks_provider.is_none());
}

#[test]
fn runtime_type_absent_resolves_to_vendor_claude_default() {
    let tmp = tempdir().expect("tempdir");
    write_agent_toml(
        tmp.path(),
        r#"
[agent]
name = "faye"
"#,
    );
    let def = agent_dir::load_definition(tmp.path()).expect("load");
    assert_eq!(
        def.runtime,
        AgentRuntimeKind::Vendor {
            command: "claude".into(),
            args: vec![],
            provider: ProviderId::Claude,
        }
    );
    assert_eq!(def.command, "claude");
}

#[test]
fn runtime_type_vendor_explicit_matches_absent() {
    let tmp = tempdir().expect("tempdir");
    write_agent_toml(
        tmp.path(),
        r#"
[agent]
name = "rex"

[runtime]
type = "vendor"
command = "codex"
args = ["--full-auto"]
"#,
    );
    let def = agent_dir::load_definition(tmp.path()).expect("load");
    assert_eq!(
        def.runtime,
        AgentRuntimeKind::Vendor {
            command: "codex".into(),
            args: vec!["--full-auto".into()],
            provider: ProviderId::Codex,
        }
    );
}

#[test]
fn runtime_command_kimi_derives_kimi_provider() {
    let tmp = tempdir().expect("tempdir");
    write_agent_toml(
        tmp.path(),
        r#"
[agent]
name = "kimi"

[runtime]
command = "kimi"
"#,
    );
    let def = agent_dir::load_definition(tmp.path()).expect("load");
    let AgentRuntimeKind::Vendor { provider, .. } = def.runtime else {
        panic!("expected vendor runtime");
    };
    assert_eq!(provider, ProviderId::Kimi);
}

#[test]
fn hooks_provider_wins_over_command_basename() {
    let tmp = tempdir().expect("tempdir");
    write_agent_toml(
        tmp.path(),
        r#"
[agent]
name = "wrapped"

[runtime]
command = "/opt/wrappers/my-claude-wrapper"

[hooks]
provider = "claude"
"#,
    );
    let def = agent_dir::load_definition(tmp.path()).expect("load");
    let AgentRuntimeKind::Vendor { provider, .. } = def.runtime else {
        panic!("expected vendor runtime");
    };
    assert_eq!(provider, ProviderId::Claude);
}

#[test]
fn runtime_type_native_requires_model_and_loads() {
    let tmp = tempdir().expect("tempdir");
    write_agent_toml(
        tmp.path(),
        r#"
[agent]
name = "kimi"

[runtime]
type = "native"
model = "kimi:k2"
"#,
    );
    let def = agent_dir::load_definition(tmp.path()).expect("load");
    assert_eq!(
        def.runtime,
        AgentRuntimeKind::Native {
            model: "kimi:k2".into()
        }
    );
    // Native agents carry no vendor command.
    assert!(def.command.is_empty());
    assert!(def.args.is_empty());
}

#[test]
fn hydrate_routes_native_agents_out_of_the_command_map() {
    let tmp = tempdir().expect("tempdir");
    let agents = tmp.path().join("agents");
    write_agent_toml(&agents.join("faye"), "[agent]\nname = \"faye\"\n");
    write_agent_toml(
        &agents.join("kimi"),
        "[agent]\nname = \"kimi\"\n\n[runtime]\ntype = \"native\"\nmodel = \"kimi:k2\"\n",
    );
    let mut config = orkia_shell::ShellConfig {
        data_dir: tmp.path().to_path_buf(),
        ..Default::default()
    };
    config.hydrate_agents_from_dir();
    assert!(config.resolve_agent("faye").is_some());
    assert!(config.resolve_agent("kimi").is_none());
    assert!(config.native_agents.contains("kimi"));
    assert!(
        config
            .agent_unresolved_reason("kimi")
            .contains("type = \"native\"")
    );
    assert!(
        config
            .agent_unresolved_reason("ghost")
            .contains("no command configured")
    );
}

#[test]
fn kimi_cli_agent_gets_a_bare_plan_and_zero_global_mutation() {
    // P1.6: a `command = "kimi"` agent works end-to-end with NO special
    // code — context env only, no Claude wiring, no hook config written,
    // no provider trust config. Each capability flips to true only after
    // a green real-agent demos scenario with the actual kimi CLI.
    let tmp = tempdir().expect("tempdir");
    write_agent_toml(
        tmp.path(),
        r#"
[agent]
name = "kimi"

[runtime]
command = "kimi"
"#,
    );
    let def = agent_dir::load_definition(tmp.path()).expect("load");
    let AgentRuntimeKind::Vendor { provider, .. } = def.runtime else {
        panic!("expected vendor runtime");
    };
    assert_eq!(provider, ProviderId::Kimi);

    // Capability table: everything off, validation required.
    let caps = provider.capabilities();
    assert!(!caps.hooks_capture);
    assert!(!caps.mcp_primitives);
    assert!(!caps.cooperative_deny);
    assert!(!caps.final_response);
    assert!(caps.requires_real_agent_validation);

    // Spawn plan is bare: the context bundle env only, even when every
    // input was prepared, and mediation never arms.
    let plan = orkia_shell::providers::build_spawn_plan(orkia_shell::providers::SpawnPlanInputs {
        provider,
        assembled_system_prompt: Some("system prompt"),
        context_path: Some(std::path::Path::new("/run/context.md")),
        mcp_config_path: Some(std::path::Path::new("/run/mcp-config.json")),
        mcp_servers: None,
        mediate_requested: true,
    });
    assert_eq!(
        plan.env,
        vec![(
            "ORKIA_AGENT_CONTEXT".to_string(),
            "/run/context.md".to_string()
        )]
    );
    assert!(plan.args.is_empty());
    assert!(!plan.mediate);

    // No hook config is written anywhere (no known kimi hook format).
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).expect("mkdir project");
    let installed = orkia_shell::hooks::install_hooks(&project, provider, false).expect("noop");
    assert_eq!(installed, None);
    assert_eq!(std::fs::read_dir(&project).expect("read_dir").count(), 0);

    // Trust falls back to the generic auto-answer integration.
    let trust = orkia_shell::trust::provider_for(provider, tmp.path().to_path_buf());
    assert_eq!(trust.name(), "kimi");
    assert!(!trust.is_trusted(&project));
}

#[test]
fn invalid_runtime_sections_are_skipped_fail_closed() {
    // Each invalid [runtime] must make the loader skip the agent
    // entirely — never half-load it as a vendor-claude agent.
    let cases = [
        // native without model
        "[agent]\nname = \"a\"\n\n[runtime]\ntype = \"native\"\n",
        // native with a vendor command
        "[agent]\nname = \"a\"\n\n[runtime]\ntype = \"native\"\nmodel = \"kimi:k2\"\ncommand = \"claude\"\n",
        // native with vendor args
        "[agent]\nname = \"a\"\n\n[runtime]\ntype = \"native\"\nmodel = \"kimi:k2\"\nargs = [\"-x\"]\n",
        // unknown runtime type
        "[agent]\nname = \"a\"\n\n[runtime]\ntype = \"quantum\"\n",
        // model on a vendor runtime
        "[agent]\nname = \"a\"\n\n[runtime]\nmodel = \"kimi:k2\"\n",
    ];
    for body in cases {
        let tmp = tempdir().expect("tempdir");
        write_agent_toml(tmp.path(), body);
        assert!(
            agent_dir::load_definition(tmp.path()).is_none(),
            "should skip: {body}"
        );
    }
}
