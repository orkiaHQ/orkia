// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! Binds a stage's raw tokens to its `Signature`, coercing typed literals
//! (`1mb`, `5sec`, ints, floats, bools) into `Value`s. Typed (registry)
//! commands bind here; the REPL-side builtins still hand-parse their args
//! (e.g. `PsFlags::parse` for the legacy bare `ps`). Binding errors
//! (`MissingArg`/`UnknownFlag`/`BadArgs`) surface here — before any
//! command runs.

use indexmap::IndexMap;
use orkia_shell_types::exec::command::EvaluatedCall;
use orkia_shell_types::exec::literal::{parse_duration, parse_filesize};
use orkia_shell_types::{ExecError, FlagSpec, Signature, Type, Value};

/// Bind `raw_args` to `sig`, producing a typed [`EvaluatedCall`].
pub fn evaluate(
    sig: &Signature,
    head: &str,
    raw_args: &[String],
) -> Result<EvaluatedCall, ExecError> {
    let mut positional: Vec<Value> = Vec::new();
    let mut named: IndexMap<String, Option<Value>> = IndexMap::new();
    let mut positional_index = 0usize;

    let mut iter = raw_args.iter();
    while let Some(token) = iter.next() {
        if let Some(flag) = flag_lookup(sig, token) {
            bind_flag(head, flag, &mut iter, &mut named)?;
        } else if sig.rest.is_none() && is_flag_shaped(token) {
            // is an error — never a silently-coerced positional. Excess
            // positionals stay lenient; unknown flags do not. Commands that
            // declare a `rest` slot (`journal`, `where`, `sort-by`) own
            // their raw-token grammar and absorb such tokens there.
            return Err(ExecError::UnknownFlag {
                command: head.to_string(),
                flag: token.clone(),
            });
        } else {
            let ty = positional_type(sig, positional_index);
            positional.push(coerce(head, token, ty)?);
            positional_index += 1;
        }
    }

    if positional.len() < sig.required.len() {
        let missing = &sig.required[positional.len()];
        return Err(ExecError::MissingArg {
            command: head.to_string(),
            name: missing.name.clone(),
        });
    }

    Ok(EvaluatedCall {
        head: head.to_string(),
        positional,
        named,
    })
}

/// Whether a token reads as a flag: `--long`, or `-x`/`-xyz` that is not a
/// negative number literal (`-3`, `-2.5` stay positional).
fn is_flag_shaped(token: &str) -> bool {
    if let Some(rest) = token.strip_prefix("--") {
        return !rest.is_empty();
    }
    match token.strip_prefix('-') {
        Some(rest) if !rest.is_empty() => rest.parse::<f64>().is_err(),
        _ => false,
    }
}

/// token is not flag-shaped. A `-3`-style negative number is not a flag.
fn flag_lookup<'a>(sig: &'a Signature, token: &str) -> Option<&'a FlagSpec> {
    if let Some(long) = token.strip_prefix("--") {
        return sig.flags.iter().find(|f| f.long == long);
    }
    if let Some(short) = token.strip_prefix('-')
        && short.chars().count() == 1
        && let Some(c) = short.chars().next()
        && !c.is_ascii_digit()
    {
        return sig.flags.iter().find(|f| f.short == Some(c));
    }
    None
}

/// Bind a recognized flag, consuming its value token when the flag takes one.
fn bind_flag<'a, I: Iterator<Item = &'a String>>(
    head: &str,
    flag: &FlagSpec,
    iter: &mut I,
    named: &mut IndexMap<String, Option<Value>>,
) -> Result<(), ExecError> {
    match &flag.takes_arg {
        Some(ty) => {
            let value = iter.next().ok_or_else(|| ExecError::BadArgs {
                command: head.to_string(),
                message: format!("flag `--{}` requires a value", flag.long),
            })?;
            named.insert(flag.long.clone(), Some(coerce(head, value, ty)?));
        }
        None => {
            named.insert(flag.long.clone(), None);
        }
    }
    Ok(())
}

/// The declared type of the positional at `index`: required, then optional,
/// then the `rest` type (or `String` when no `rest` is declared).
fn positional_type(sig: &Signature, index: usize) -> &Type {
    if let Some(arg) = sig.required.get(index) {
        return &arg.ty;
    }
    if let Some(arg) = sig.optional.get(index - sig.required.len()) {
        return &arg.ty;
    }
    sig.rest.as_ref().map(|a| &a.ty).unwrap_or(&Type::String)
}

/// Coerce a raw token into a `Value` of the requested type.
fn coerce(head: &str, token: &str, ty: &Type) -> Result<Value, ExecError> {
    let bad = |what: &str| ExecError::BadArgs {
        command: head.to_string(),
        message: format!("`{token}` is not a valid {what}"),
    };
    match ty {
        Type::String => Ok(Value::String(token.to_string())),
        Type::Int => token
            .parse::<i64>()
            .map(Value::Int)
            .map_err(|_| bad("integer")),
        Type::Float => token
            .parse::<f64>()
            .map(Value::Float)
            .map_err(|_| bad("float")),
        Type::Bool => parse_bool(token)
            .map(Value::Bool)
            .ok_or_else(|| bad("bool")),
        Type::Filesize => parse_filesize(token)
            .map(Value::Filesize)
            .ok_or_else(|| bad("filesize")),
        Type::Duration => parse_duration(token)
            .map(Value::Duration)
            .ok_or_else(|| bad("duration")),
        _ => Ok(infer_literal(token)),
    }
}

/// Infer the most specific `Value` for an untyped (`Any`) token.
pub fn infer_literal(token: &str) -> Value {
    if let Some(b) = parse_bool(token) {
        return Value::Bool(b);
    }
    if let Some(size) = parse_filesize(token) {
        return Value::Filesize(size);
    }
    if let Some(dur) = parse_duration(token) {
        return Value::Duration(dur);
    }
    if let Ok(i) = token.parse::<i64>() {
        return Value::Int(i);
    }
    if let Ok(f) = token.parse::<f64>() {
        return Value::Float(f);
    }
    Value::String(token.to_string())
}

fn parse_bool(token: &str) -> Option<bool> {
    match token {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_shell_types::exec::command::Command;

    #[test]
    fn unknown_flag_is_an_error_not_a_positional() {
        let sig = crate::exec::commands::ps::Ps.signature();
        match evaluate(&sig, "ps", &["--bogus".to_string()]) {
            Err(ExecError::UnknownFlag { flag, .. }) => assert_eq!(flag, "--bogus"),
            Err(other) => panic!("expected UnknownFlag, got {other:?}"),
            Ok(_) => panic!("expected UnknownFlag, got Ok"),
        }
    }

    #[test]
    fn unknown_short_flag_is_an_error() {
        let sig = crate::exec::commands::ps::Ps.signature();
        match evaluate(&sig, "ps", &["-z".to_string()]) {
            Err(ExecError::UnknownFlag { .. }) => {}
            Err(other) => panic!("expected UnknownFlag, got {other:?}"),
            Ok(_) => panic!("expected UnknownFlag, got Ok"),
        }
    }

    #[test]
    fn negative_numbers_stay_positional() {
        assert!(!is_flag_shaped("-3"));
        assert!(!is_flag_shaped("-2.5"));
        assert!(is_flag_shaped("--bogus"));
        assert!(is_flag_shaped("-ef"));
        assert!(!is_flag_shaped("-"));
    }
}
