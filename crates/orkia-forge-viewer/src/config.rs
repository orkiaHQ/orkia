// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Clone)]
pub struct ViewerConfig {
    pub app_dir: PathBuf,
    pub app_id: String,
    pub socket_path: PathBuf,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("--app-dir missing")]
    MissingAppDir,
    #[error("--app-id missing")]
    MissingAppId,
    #[error("--socket missing")]
    MissingSocket,
    #[error("unknown flag: {0}")]
    UnknownFlag(String),
}

impl ViewerConfig {
    /// Parse the CLI args the shell passes to the viewer. Expected form:
    ///   `orkia-forge-viewer --app-dir <path> --app-id <id> --socket <path>`
    pub fn from_args<I: IntoIterator<Item = String>>(args: I) -> Result<Self, ConfigError> {
        let mut iter = args.into_iter().peekable();
        let mut app_dir = None;
        let mut app_id = None;
        let mut socket = None;
        while let Some(a) = iter.next() {
            match a.as_str() {
                "--app-dir" => app_dir = iter.next().map(PathBuf::from),
                "--app-id" => app_id = iter.next(),
                "--socket" => socket = iter.next().map(PathBuf::from),
                flag if flag.starts_with("--") => {
                    return Err(ConfigError::UnknownFlag(flag.into()));
                }
                // Skip positional (e.g. the program name on argv[0] passthrough).
                _ => {}
            }
        }
        Ok(ViewerConfig {
            app_dir: app_dir.ok_or(ConfigError::MissingAppDir)?,
            app_id: app_id.ok_or(ConfigError::MissingAppId)?,
            socket_path: socket.ok_or(ConfigError::MissingSocket)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_full_arg_set() {
        let c = ViewerConfig::from_args(args(&[
            "--app-dir",
            "/tmp/a",
            "--app-id",
            "orkia.forge.x",
            "--socket",
            "/run/orkia.sock",
        ]))
        .unwrap();
        assert_eq!(c.app_id, "orkia.forge.x");
    }

    #[test]
    fn rejects_unknown_flag() {
        let err = ViewerConfig::from_args(args(&["--app-dir", "/x", "--zzz", "1"])).unwrap_err();
        assert!(matches!(err, ConfigError::UnknownFlag(_)));
    }

    #[test]
    fn missing_app_dir_errors() {
        let err = ViewerConfig::from_args(args(&["--app-id", "x", "--socket", "/x"])).unwrap_err();
        assert!(matches!(err, ConfigError::MissingAppDir));
    }
}
