// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! Filesystem-based, consistent with `agent.toml` / `project.toml`. Drives the
//! EXEC-CORE `Signature` (IO types + args) and the capability grant set. A
//! single-file plugin with no manifest defaults to a total sandbox with an
//! `any → any` signature — see [`PluginManifest::sandbox_default`].

use indexmap::IndexMap;
use orkia_shell_types::exec::Scope;
use orkia_shell_types::{CapabilityScope, CapabilitySet, FlagSpec, Signature, Type};
use serde::Deserialize;

use crate::error::PluginError;

#[derive(Debug, Clone, Deserialize)]
pub struct PluginManifest {
    pub plugin: PluginSection,
    #[serde(default)]
    pub command: CommandSection,
    /// Requested capabilities; empty/absent = total sandbox. Each entry
    /// is presented to the user for approval and journaled in SEAL.
    #[serde(default)]
    pub capabilities: IndexMap<String, toml::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginSection {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub entry: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommandSection {
    #[serde(default = "any_type")]
    pub input_type: String,
    #[serde(default = "any_type")]
    pub output_type: String,
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub args: IndexMap<String, ArgSpec>,
}

impl Default for CommandSection {
    fn default() -> Self {
        Self {
            input_type: any_type(),
            output_type: any_type(),
            streaming: false,
            args: IndexMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArgSpec {
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub required: bool,
}

fn any_type() -> String {
    "any".to_string()
}

impl PluginManifest {
    /// Parse a `plugin.toml` from its text.
    pub fn parse(text: &str) -> Result<Self, PluginError> {
        toml::from_str(text).map_err(|e| PluginError::Manifest(e.to_string()))
    }

    /// The default manifest for a single-file plugin with no `plugin.toml`:
    /// total sandbox, `any → any`, no declared args.
    pub fn sandbox_default(name: &str) -> Self {
        Self {
            plugin: PluginSection {
                name: name.to_string(),
                version: "0.0.0".to_string(),
                entry: None,
                description: None,
            },
            command: CommandSection::default(),
            capabilities: IndexMap::new(),
        }
    }

    /// Whether the plugin requested any capability. `false` ⇒ total sandbox.
    pub fn requests_capabilities(&self) -> bool {
        !self.capabilities.is_empty()
    }

    /// Translate the manifest's `[capabilities]` table into the unified
    /// absent / unrecognized keys grant nothing. Recognized keys:
    /// `fs_read`/`fs_write` (paths), `net` (hosts), `env` (var names) — each a
    /// string `"any"`, a list of strings, or omitted; `clock`/`random` (bool).
    pub fn granted_capabilities(&self) -> CapabilitySet {
        let get = |key: &str| self.capabilities.get(key);
        CapabilitySet {
            fs_read: scope_of(get("fs_read"), ScopeKind::Path),
            fs_write: scope_of(get("fs_write"), ScopeKind::Path),
            net: scope_of(get("net"), ScopeKind::Host),
            env: scope_of(get("env"), ScopeKind::Var),
            clock: get("clock").and_then(toml::Value::as_bool).unwrap_or(false),
            random: get("random")
                .and_then(toml::Value::as_bool)
                .unwrap_or(false),
        }
    }

    /// Build the EXEC-CORE [`Signature`] this plugin presents to the registry.
    /// Declared args become value flags (`--name <value>`), matching how
    /// plugins are invoked (`where_geo --within-km 5`). The value is
    /// coerced to the declared type by the kernel arg-evaluator and reaches
    /// the guest under `call.named`. `required` is advisory in V1 — a
    /// missing flag simply isn't present in `call.named`; the plugin handles it.
    pub fn to_signature(&self) -> Result<Signature, PluginError> {
        let input = parse_type(&self.command.input_type)?;
        let output = parse_type(&self.command.output_type)?;
        let mut builder = Signature::builder(&self.plugin.name).io(input, output);
        for (name, arg) in &self.command.args {
            let ty = parse_type(&arg.ty)?;
            builder = builder.flag(FlagSpec {
                long: name.clone(),
                short: None,
                takes_arg: Some(ty),
                desc: String::new(),
            });
        }
        Ok(builder.build())
    }
}

/// Parse a manifest type string (`any`, `float`, `list<record>`, …) into a
/// [`Type`]. Case-insensitive; whitespace-trimmed. Unknown → error (a typo'd
/// type must not silently become `any`).
pub fn parse_type(spec: &str) -> Result<Type, PluginError> {
    let s = spec.trim();
    let lower = s.to_ascii_lowercase();
    if let Some(inner) = lower
        .strip_prefix("list<")
        .and_then(|r| r.strip_suffix('>'))
    {
        return Ok(Type::List(Box::new(parse_type(inner)?)));
    }
    let ty = match lower.as_str() {
        "any" => Type::Any,
        "nothing" | "null" => Type::Nothing,
        "bool" => Type::Bool,
        "int" => Type::Int,
        "float" => Type::Float,
        "filesize" => Type::Filesize,
        "duration" => Type::Duration,
        "date" => Type::Date,
        "string" => Type::String,
        "binary" => Type::Binary,
        "record" => Type::Record(Vec::new()),
        "table" => Type::Table,
        "bytestream" => Type::ByteStream,
        other => {
            return Err(PluginError::Manifest(format!("unknown type `{other}`")));
        }
    };
    Ok(ty)
}

/// Which kind of [`Scope`] entry a capability key's strings denote.
enum ScopeKind {
    Path,
    Host,
    Var,
}

fn make_scope(kind: &ScopeKind, s: String) -> Scope {
    match kind {
        ScopeKind::Path => Scope::Path(s.into()),
        ScopeKind::Host => Scope::Host(s),
        ScopeKind::Var => Scope::Var(s),
    }
}

/// Interpret a `[capabilities]` value as a [`CapabilityScope`]. `"any"` ⇒ `Any`;
/// a string or list of strings ⇒ `Scoped`; anything else (incl. absent) ⇒
/// `None` (fail-closed).
fn scope_of(value: Option<&toml::Value>, kind: ScopeKind) -> CapabilityScope {
    match value {
        Some(toml::Value::String(s)) if s == "any" => CapabilityScope::Any,
        Some(toml::Value::String(s)) => CapabilityScope::Scoped(vec![make_scope(&kind, s.clone())]),
        Some(toml::Value::Array(items)) => {
            let scopes: Vec<Scope> = items
                .iter()
                .filter_map(|v| v.as_str().map(|s| make_scope(&kind, s.to_string())))
                .collect();
            if scopes.is_empty() {
                CapabilityScope::None
            } else {
                CapabilityScope::Scoped(scopes)
            }
        }
        _ => CapabilityScope::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_manifest() {
        let text = r#"
            [plugin]
            name = "where_geo"
            version = "0.1.0"
            entry = "src/main.ts"
            description = "geo filter"

            [command]
            input_type = "list<record>"
            output_type = "list<record>"
            streaming = false

            [command.args]
            within_km = { type = "float", required = true }
            unit = { type = "string" }
        "#;
        let m = PluginManifest::parse(text).expect("parse");
        assert_eq!(m.plugin.name, "where_geo");
        assert_eq!(m.plugin.entry.as_deref(), Some("src/main.ts"));
        assert!(!m.requests_capabilities());

        let sig = m.to_signature().expect("signature");
        assert_eq!(sig.name, "where_geo");
        assert_eq!(
            sig.io_types,
            vec![(
                Type::List(Box::new(Type::Record(Vec::new()))),
                Type::List(Box::new(Type::Record(Vec::new())))
            )]
        );
        // Declared args become value flags (looked up by name — order is not
        // significant and the toml map does not preserve document order).
        assert_eq!(sig.flags.len(), 2);
        let within = sig
            .flags
            .iter()
            .find(|f| f.long == "within_km")
            .expect("within_km flag");
        assert_eq!(within.takes_arg, Some(Type::Float));
        let unit = sig
            .flags
            .iter()
            .find(|f| f.long == "unit")
            .expect("unit flag");
        assert_eq!(unit.takes_arg, Some(Type::String));
    }

    #[test]
    fn sandbox_default_is_any_to_any() {
        let m = PluginManifest::sandbox_default("x");
        let sig = m.to_signature().expect("sig");
        assert_eq!(sig.io_types, vec![(Type::Any, Type::Any)]);
        assert!(!m.requests_capabilities());
    }

    #[test]
    fn capabilities_present_flips_flag() {
        let text = r#"
            [plugin]
            name = "p"
            version = "1.0.0"
            [capabilities]
            clock = "deterministic"
        "#;
        let m = PluginManifest::parse(text).expect("parse");
        assert!(m.requests_capabilities());
    }

    #[test]
    fn granted_capabilities_parse_scopes_and_flags() {
        let text = r#"
            [plugin]
            name = "p"
            version = "1.0.0"
            [capabilities]
            fs_read = ["./data", "./more"]
            net = "any"
            clock = true
        "#;
        let m = PluginManifest::parse(text).expect("parse");
        let caps = m.granted_capabilities();
        assert!(caps.allows_fs_read(std::path::Path::new("./data/x")));
        assert!(!caps.allows_fs_read(std::path::Path::new("/etc")));
        assert!(caps.allows_net("anyhost.example"));
        assert!(caps.clock);
        assert!(!caps.random);
        // Unrequested capabilities stay fail-closed.
        assert!(!caps.allows_fs_write(std::path::Path::new("./data/x")));
        assert!(!caps.allows_env("HOME"));
    }

    #[test]
    fn no_capabilities_is_total_sandbox() {
        let caps = PluginManifest::sandbox_default("x").granted_capabilities();
        assert!(caps.is_total_sandbox());
    }

    #[test]
    fn unknown_type_is_rejected() {
        assert!(parse_type("frobnicate").is_err());
        assert!(parse_type("list<frob>").is_err());
    }

    #[test]
    fn type_parser_cases() {
        assert_eq!(parse_type("any").unwrap(), Type::Any);
        assert_eq!(parse_type("Float").unwrap(), Type::Float);
        assert_eq!(parse_type("filesize").unwrap(), Type::Filesize);
        assert_eq!(
            parse_type("list<string>").unwrap(),
            Type::List(Box::new(Type::String))
        );
    }
}
