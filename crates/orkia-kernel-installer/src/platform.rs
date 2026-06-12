// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Platform identifier strings used in the manifest URL.

/// Platforms the installer recognises. The string form matches the
/// `?platform=` query parameter the manifest endpoint expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    DarwinArm64,
    DarwinX86_64,
    LinuxX86_64,
    LinuxArm64,
}

impl Platform {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DarwinArm64 => "darwin-arm64",
            Self::DarwinX86_64 => "darwin-x86_64",
            Self::LinuxX86_64 => "linux-x86_64",
            Self::LinuxArm64 => "linux-arm64",
        }
    }
}

/// Detect the platform the current binary is running on. Returns
/// `None` for unsupported combinations — the shell surfaces a clear
/// error rather than guessing.
pub fn current_platform() -> Option<Platform> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        Some(Platform::DarwinArm64)
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        Some(Platform::DarwinX86_64)
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        Some(Platform::LinuxX86_64)
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        Some(Platform::LinuxArm64)
    }
    #[cfg(not(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
    )))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_are_url_safe() {
        for p in [
            Platform::DarwinArm64,
            Platform::DarwinX86_64,
            Platform::LinuxX86_64,
            Platform::LinuxArm64,
        ] {
            let s = p.as_str();
            assert!(
                s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            );
        }
    }

    #[test]
    fn detection_returns_something_on_supported_targets() {
        // Either Some(...) or None — both are valid; the test exists
        // to catch a regression that breaks the cfg matrix.
        let _ = current_platform();
    }
}
