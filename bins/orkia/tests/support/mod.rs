// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use std::process::Command;

fn orkia_bin() -> &'static str {
    env!("CARGO_BIN_EXE_orkia")
}

pub struct DaemonGuard {
    home: std::path::PathBuf,
}

impl DaemonGuard {
    pub fn new(home: &std::path::Path) -> Self {
        Self {
            home: home.to_path_buf(),
        }
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = Command::new(orkia_bin())
            .env("HOME", &self.home)
            .arg("pty-daemon-stop")
            .output();
    }
}

pub fn write_echo_agent_config(home: &std::path::Path, orkia_dir: &std::path::Path) {
    let script = home.join("echo-agent.sh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
printf 'agent:echo\n'
trap 'exit 0' TERM INT
while IFS= read -r line; do
  printf 'echo:%s\n' "$line"
done
while true; do
  sleep 1
done
"#,
    )
    .expect("write fake agent");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
            .expect("chmod fake agent");
    }
    std::fs::write(orkia_dir.join("config.toml"), "").expect("write config");
    write_agent_definition(orkia_dir, "echo", "test", &script.display().to_string());
}

pub fn write_agent_definition(
    orkia_dir: &std::path::Path,
    name: &str,
    archetype: &str,
    command: &str,
) {
    let agent_dir = orkia_dir.join("agents").join(name);
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");
    std::fs::write(
        agent_dir.join("agent.toml"),
        format!(
            r#"
[agent]
name = "{name}"
archetype = "{archetype}"

[runtime]
command = "{command}"
args = []
"#,
        ),
    )
    .expect("write agent toml");
}

pub fn wait_for_path(path: &std::path::Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if path.exists() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
