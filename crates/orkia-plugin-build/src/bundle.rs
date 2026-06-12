// Copyright 2026 Orkia
// SPDX-License-Identifier: Apache-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Apache License 2.0; see https://www.apache.org/licenses/LICENSE-2.0
// for terms.

//!
//! A real plugin is rarely one file: `src/main.ts` imports `./haversine`,
//! `./types`, maybe a pure-JS package from `node_modules`. V1 only handled a
//! single file. This module resolves the **local** module graph from an entry,
//! converts each module's ES-module syntax to CommonJS, and emits a single
//! self-contained JS with a tiny CommonJS runtime — which then goes to
//! Javy/QuickJS exactly like a single-file plugin.
//!
//! **Local only** (relative imports + pure-JS packages already in
//! `node_modules`); no network. Fetching from the npm registry is Volet B,
//! that is byte-identical to V1 — bundling only engages when there are imports.
//!
//! OXC has no general ESM→CommonJS transform (only TS-specific module syntax),
//! and Rolldown is not a practical library dependency, so the ESM→CJS rewrite
//! here is done by reading the parsed AST for import/export metadata + byte
//! spans and splicing CommonJS text in — no fragile AST reconstruction.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use oxc::allocator::Allocator;
use oxc::ast::ast::{
    BindingPattern, Declaration, ExportDefaultDeclarationKind, ImportDeclarationSpecifier,
    ModuleExportName, Statement,
};
use oxc::parser::Parser;
use oxc::span::{GetSpan, SourceType};

use crate::error::CompileError;
use crate::transpile::transpile_ts;

/// Extensions probed when resolving an extensionless import (TS first — the
/// authoring language — then JS).
const RESOLVE_EXTS: &[&str] = &["ts", "tsx", "mts", "js", "mjs", "cjs"];

/// Compile-time entry point used by `compile_file`: produce the JS to hand to
/// Javy. A single-file plugin (no imports) keeps the exact V1 output; a
/// multi-file plugin is bundled into one CommonJS-wrapped module.
pub fn bundle_entry(entry: &Path, ext: &str) -> Result<String, CompileError> {
    let source = read(entry)?;
    if !has_imports(&source, entry)? {
        // V1 fast path — byte-identical to the single-file compiler.
        return match ext {
            "ts" | "tsx" | "mts" => transpile_ts(&source, entry),
            _ => Ok(source),
        };
    }
    let mut ids = IdMap::default();
    let entry_id = ids.id_for(entry)?;
    let modules = build_graph(entry, &mut ids)?;
    Ok(emit_bundle(&modules, &entry_id))
}

/// Maps a canonical module path to a short, JS-safe registry id (`m0`, `m1`…).
#[derive(Default)]
struct IdMap {
    by_path: HashMap<PathBuf, String>,
    next: usize,
}

impl IdMap {
    fn id_for(&mut self, path: &Path) -> Result<String, CompileError> {
        let canonical = std::fs::canonicalize(path)
            .map_err(|e| CompileError::Bundle(format!("canonicalize {}: {e}", path.display())))?;
        if let Some(id) = self.by_path.get(&canonical) {
            return Ok(id.clone());
        }
        let id = format!("m{}", self.next);
        self.next += 1;
        self.by_path.insert(canonical, id.clone());
        Ok(id)
    }
}

/// One resolved module: its registry id and its CommonJS body.
struct ModuleOut {
    id: String,
    code: String,
}

/// Walk the import graph from `entry`, converting each module to CommonJS and
/// rewriting its import specifiers to registry ids. Iterative; cycles are fine
/// (each module visited once; the runtime cache handles circular `require`).
fn build_graph(entry: &Path, ids: &mut IdMap) -> Result<Vec<ModuleOut>, CompileError> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![entry.to_path_buf()];
    while let Some(path) = stack.pop() {
        let id = ids.id_for(&path)?;
        if !seen.insert(id.clone()) {
            continue;
        }
        let (code, deps) = transform_module(&path, ids)?;
        out.push(ModuleOut { id, code });
        for dep in deps {
            stack.push(dep);
        }
    }
    Ok(out)
}

/// Convert one module from ESM to CommonJS. Type-strips TS first, then re-parses
/// the plain JS and splices import/export statements into `require`/`exports`
/// using the AST's byte spans. Returns the CJS body and the resolved deps.
fn transform_module(path: &Path, ids: &mut IdMap) -> Result<(String, Vec<PathBuf>), CompileError> {
    let source = read(path)?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let js = match ext {
        "ts" | "tsx" | "mts" => transpile_ts(&source, path)?,
        _ => source,
    };

    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::ts());
    let parsed = Parser::new(&allocator, &js, source_type).parse();
    if !parsed.errors.is_empty() {
        return Err(CompileError::Bundle(format!(
            "parse {}: {}",
            path.display(),
            join_errors(parsed.errors.iter().map(|e| e.to_string()))
        )));
    }

    let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let mut deps = Vec::new();
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut tmp = 0usize;
    for stmt in parsed.program.body.iter() {
        if let Some(edit) = rewrite_stmt(stmt, &js, &dir, ids, &mut deps, &mut tmp)? {
            edits.push(edit);
        }
    }

    // Splice from the end so earlier offsets stay valid.
    edits.sort_by(|a, b| b.0.cmp(&a.0));
    let mut body = js;
    for (start, end, text) in edits {
        body.replace_range(start..end, &text);
    }
    Ok((format!("exports.__esModule = true;\n{body}"), deps))
}

/// Produce the CommonJS replacement for one top-level statement, if it is a
/// module declaration. Records any resolved dependency in `deps`. Returns
/// `None` for ordinary statements (left untouched).
fn rewrite_stmt(
    stmt: &Statement,
    js: &str,
    dir: &Path,
    ids: &mut IdMap,
    deps: &mut Vec<PathBuf>,
    tmp: &mut usize,
) -> Result<Option<(usize, usize, String)>, CompileError> {
    match stmt {
        Statement::ImportDeclaration(d) => {
            let id = resolve_dep(d.source.value.as_str(), dir, ids, deps)?;
            let text = match &d.specifiers {
                None => format!("require(\"{id}\");"),
                Some(specs) if specs.is_empty() => format!("require(\"{id}\");"),
                Some(specs) => import_bindings(specs, &id, tmp),
            };
            Ok(Some((span(stmt), span_end(stmt), text)))
        }
        Statement::ExportNamedDeclaration(d) => {
            let text = if let Some(decl) = &d.declaration {
                export_declaration(decl, js)
            } else if let Some(src) = &d.source {
                let id = resolve_dep(src.value.as_str(), dir, ids, deps)?;
                reexport_from(&d.specifiers, &id, tmp)
            } else {
                local_reexports(&d.specifiers)
            };
            Ok(Some((span(stmt), span_end(stmt), text)))
        }
        Statement::ExportAllDeclaration(d) => {
            let id = resolve_dep(d.source.value.as_str(), dir, ids, deps)?;
            let t = next_tmp(tmp);
            let text = format!(
                "const {t} = require(\"{id}\"); for (var _k in {t}) {{ if (_k !== \"default\") exports[_k] = {t}[_k]; }}"
            );
            Ok(Some((span(stmt), span_end(stmt), text)))
        }
        Statement::ExportDefaultDeclaration(d) => Ok(Some((
            span(stmt),
            span_end(stmt),
            export_default(&d.declaration, js),
        ))),
        _ => Ok(None),
    }
}

/// Build `const` bindings for a non-empty import: a temp `require`, then one
/// `const` per default / namespace / named specifier.
fn import_bindings(specs: &[ImportDeclarationSpecifier], id: &str, tmp: &mut usize) -> String {
    let t = next_tmp(tmp);
    let mut out = vec![format!("const {t} = require(\"{id}\");")];
    for spec in specs {
        match spec {
            ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                out.push(format!("const {} = _orkiaInterop({t});", s.local.name));
            }
            ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                out.push(format!("const {} = {t};", s.local.name));
            }
            ImportDeclarationSpecifier::ImportSpecifier(s) => {
                out.push(format!(
                    "const {} = {t}.{};",
                    s.local.name,
                    export_name(&s.imported)
                ));
            }
        }
    }
    out.join(" ")
}

/// `export <decl>` → the bare declaration plus `exports.x = x` for each name.
fn export_declaration(decl: &Declaration, js: &str) -> String {
    let text = slice(js, decl.span());
    let mut out = text.to_string();
    for name in declared_names(decl) {
        out.push_str(&format!(" exports.{name} = {name};"));
    }
    out
}

/// `export { a, b as c } from "x"` → require + per-specifier re-assignment.
fn reexport_from(specs: &[oxc::ast::ast::ExportSpecifier], id: &str, tmp: &mut usize) -> String {
    let t = next_tmp(tmp);
    let mut out = vec![format!("const {t} = require(\"{id}\");")];
    for s in specs {
        out.push(format!(
            "exports.{} = {t}.{};",
            export_name(&s.exported),
            export_name(&s.local)
        ));
    }
    out.join(" ")
}

/// `export { a, b as c }` (local) → `exports.a = a; exports.c = b;`.
fn local_reexports(specs: &[oxc::ast::ast::ExportSpecifier]) -> String {
    specs
        .iter()
        .map(|s| {
            format!(
                "exports.{} = {};",
                export_name(&s.exported),
                export_name(&s.local)
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `export default <x>` → assign to `exports.default` (named function/class is
/// kept as a hoisted declaration first, so recursion/self-reference still work).
fn export_default(decl: &ExportDefaultDeclarationKind, js: &str) -> String {
    match decl {
        ExportDefaultDeclarationKind::FunctionDeclaration(f) => match &f.id {
            Some(id) => format!("{} exports.default = {};", slice(js, f.span), id.name),
            None => format!("exports.default = {};", slice(js, f.span)),
        },
        ExportDefaultDeclarationKind::ClassDeclaration(c) => match &c.id {
            Some(id) => format!("{} exports.default = {};", slice(js, c.span), id.name),
            None => format!("exports.default = {};", slice(js, c.span)),
        },
        other => format!("exports.default = {};", slice(js, other.span())),
    }
}

fn declared_names(decl: &Declaration) -> Vec<String> {
    match decl {
        Declaration::VariableDeclaration(v) => v
            .declarations
            .iter()
            .filter_map(|d| match &d.id {
                BindingPattern::BindingIdentifier(b) => Some(b.name.to_string()),
                _ => None,
            })
            .collect(),
        Declaration::FunctionDeclaration(f) => f.id.iter().map(|i| i.name.to_string()).collect(),
        Declaration::ClassDeclaration(c) => c.id.iter().map(|i| i.name.to_string()).collect(),
        _ => Vec::new(),
    }
}

fn export_name(name: &ModuleExportName) -> String {
    match name {
        ModuleExportName::IdentifierName(i) => i.name.to_string(),
        ModuleExportName::IdentifierReference(i) => i.name.to_string(),
        ModuleExportName::StringLiteral(s) => s.value.to_string(),
    }
}

fn resolve_dep(
    specifier: &str,
    dir: &Path,
    ids: &mut IdMap,
    deps: &mut Vec<PathBuf>,
) -> Result<String, CompileError> {
    let resolved = resolve(specifier, dir)?;
    let id = ids.id_for(&resolved)?;
    deps.push(resolved);
    Ok(id)
}

fn next_tmp(tmp: &mut usize) -> String {
    let t = format!("_orkiaImp{tmp}");
    *tmp += 1;
    t
}

fn span(stmt: &Statement) -> usize {
    stmt.span().start as usize
}
fn span_end(stmt: &Statement) -> usize {
    stmt.span().end as usize
}
fn slice(js: &str, sp: oxc::span::Span) -> &str {
    // Checked slice: spans normally come from parsing this same `js` (valid),
    // but any span/offset inconsistency (rewrite reuse, multibyte boundary,
    // parser bug) would otherwise panic the plugin compiler (BUG-088).
    js.get(sp.start as usize..sp.end as usize).unwrap_or("")
}

/// Resolve an import specifier to a local file path. Relative (`./`, `../`) and
/// bare (`lodash-es`) specifiers only; bare ones are looked up in `node_modules`
/// walking up from `dir`. No network (Volet B).
fn resolve(specifier: &str, dir: &Path) -> Result<PathBuf, CompileError> {
    if specifier.starts_with("./") || specifier.starts_with("../") || specifier.starts_with('/') {
        return resolve_file(&dir.join(specifier))
            .ok_or_else(|| CompileError::Bundle(format!("cannot resolve `{specifier}`")));
    }
    resolve_bare(specifier, dir)
        .ok_or_else(|| CompileError::Bundle(format!("cannot resolve package `{specifier}`")))
}

/// Resolve a path to a concrete source file: the path itself, then with each
/// candidate extension, then as a directory with an `index.*`.
fn resolve_file(base: &Path) -> Option<PathBuf> {
    reject_native(base)?;
    if base.is_file() {
        return Some(base.to_path_buf());
    }
    for ext in RESOLVE_EXTS {
        let cand = base.with_extension(ext);
        if cand.is_file() {
            return Some(cand);
        }
    }
    for ext in RESOLVE_EXTS {
        let cand = base.join(format!("index.{ext}"));
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// Resolve a bare specifier from the nearest `node_modules`, honoring
/// `package.json` `module`/`main` (else `index.*`). Pure-JS only.
fn resolve_bare(specifier: &str, dir: &Path) -> Option<PathBuf> {
    for ancestor in dir.ancestors() {
        let pkg_dir = ancestor.join("node_modules").join(specifier);
        if !pkg_dir.is_dir() {
            if let Some(f) = resolve_file(&pkg_dir) {
                return Some(f);
            }
            continue;
        }
        if let Some(main) = package_entry(&pkg_dir)
            && let Some(f) = resolve_file(&pkg_dir.join(main))
        {
            return Some(f);
        }
        if let Some(f) = resolve_file(&pkg_dir.join("index")) {
            return Some(f);
        }
    }
    None
}

/// Read a package's preferred entry from `package.json` (`module` then `main`).
fn package_entry(pkg_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(pkg_dir.join("package.json")).ok()?;
    let obj: serde_json::Value = serde_json::from_str(&text).ok()?;
    field(&obj, "module").or_else(|| field(&obj, "main"))
}

/// Extract a top-level string field from a parsed `package.json` object.
/// Returns `None` for missing keys, non-string values, or empty strings.
fn field(obj: &serde_json::Value, key: &str) -> Option<String> {
    let v = obj.get(key)?.as_str()?;
    (!v.is_empty()).then(|| v.to_string())
}

/// A native addon (`.node`) is never bundleable — plugins are pure compute;
fn reject_native(path: &Path) -> Option<()> {
    (path.extension().and_then(|e| e.to_str()) != Some("node")).then_some(())
}

/// Emit the final single-file bundle: a CommonJS runtime, every module as a
/// factory, then a `require` of the entry. Self-contained — no `import`/`export`
/// remains, so Javy/QuickJS runs it as a plain script.
fn emit_bundle(modules: &[ModuleOut], entry_id: &str) -> String {
    let mut out = String::from(RUNTIME);
    for module in modules {
        out.push_str(&format!(
            "__orkiaModules[\"{}\"] = function (module, exports, require) {{\n{}\n}};\n",
            module.id, module.code
        ));
    }
    out.push_str(&format!("__orkiaRequire(\"{entry_id}\");\n"));
    out
}

/// The CommonJS module runtime prepended to every multi-file bundle.
const RUNTIME: &str = r#""use strict";
var __orkiaModules = {};
var __orkiaCache = {};
function __orkiaInterop(m) { return m && m.__esModule ? m.default : m; }
function __orkiaRequire(id) {
  var cached = __orkiaCache[id];
  if (cached) return cached.exports;
  var module = { exports: {} };
  __orkiaCache[id] = module;
  __orkiaModules[id](module, module.exports, __orkiaRequire);
  return module.exports;
}
"#;

/// Does this source contain any module imports (or re-export-from)? Drives the
/// single-file fast path. Parse-only.
fn has_imports(source: &str, path: &Path) -> Result<bool, CompileError> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::ts());
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if !parsed.errors.is_empty() {
        return Err(CompileError::Bundle(format!(
            "parse {}: {}",
            path.display(),
            join_errors(parsed.errors.iter().map(|e| e.to_string()))
        )));
    }
    Ok(parsed.program.body.iter().any(|stmt| {
        matches!(
            stmt,
            Statement::ImportDeclaration(_) | Statement::ExportAllDeclaration(_)
        ) || matches!(stmt, Statement::ExportNamedDeclaration(d) if d.source.is_some())
    }))
}

fn read(path: &Path) -> Result<String, CompileError> {
    std::fs::read_to_string(path)
        .map_err(|e| CompileError::Io(format!("read {}: {e}", path.display())))
}

fn join_errors(errors: impl Iterator<Item = String>) -> String {
    errors.collect::<Vec<_>>().join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, contents: &str) -> PathBuf {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn single_file_no_imports_takes_v1_fast_path() {
        let dir = tempfile::tempdir().unwrap();
        let entry = write(
            dir.path(),
            "p.ts",
            "const x: number = 41; (globalThis as any).x = x + 1;",
        );
        let js = bundle_entry(&entry, "ts").unwrap();
        assert!(!js.contains("__orkiaModules"), "no bundle wrapper: {js}");
        assert!(!js.contains(": number"), "transpiled: {js}");
        assert!(js.contains("globalThis"), "code preserved: {js}");
    }

    #[test]
    fn bundles_relative_imports_into_one_self_contained_module() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "src/geo.ts",
            "export function area(r: number): number { return 3 * r * r; }\n",
        );
        let entry = write(
            dir.path(),
            "src/main.ts",
            "import { area } from './geo';\n(globalThis as any).result = area(2);\n",
        );
        let js = bundle_entry(&entry, "ts").unwrap();
        assert!(js.contains("__orkiaRequire("), "has runtime require: {js}");
        assert!(
            js.contains("require(\"m1\")"),
            "entry requires the dep (m1): {js}"
        );
        assert!(js.contains("exports.area = area"), "dep exports area: {js}");
        assert!(
            !js.contains("import {") && !js.contains("from './geo'"),
            "no ESM import survives: {js}"
        );
        assert!(!js.contains("@babel/runtime"), "no external helpers: {js}");
    }

    #[test]
    fn bundles_pure_js_package_from_node_modules() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "node_modules/padder/package.json",
            "{ \"name\": \"padder\", \"main\": \"index.js\" }",
        );
        write(
            dir.path(),
            "node_modules/padder/index.js",
            "export function pad(s) { return '>' + s; }\n",
        );
        let entry = write(
            dir.path(),
            "main.ts",
            "import { pad } from 'padder';\n(globalThis as any).out = pad('x');\n",
        );
        let js = bundle_entry(&entry, "ts").unwrap();
        assert!(js.contains("__orkiaRequire("), "bundled: {js}");
        assert!(js.contains("'>' + s"), "package inlined: {js}");
        assert!(
            !js.contains("from 'padder'"),
            "bare import resolved away: {js}"
        );
    }

    #[test]
    fn unresolvable_import_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let entry = write(dir.path(), "main.ts", "import x from './missing';\nx;\n");
        let err = bundle_entry(&entry, "ts").unwrap_err();
        assert!(
            matches!(err, CompileError::Bundle(_)),
            "missing module → Bundle error, got {err:?}"
        );
    }

    #[test]
    fn native_addon_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "native.node", "binary");
        let entry = write(
            dir.path(),
            "main.ts",
            "import n from './native.node';\nn;\n",
        );
        assert!(
            bundle_entry(&entry, "ts")
                .unwrap_err()
                .to_string()
                .contains("resolve")
        );
    }
}
