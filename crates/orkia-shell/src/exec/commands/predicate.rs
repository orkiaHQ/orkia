// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! Grammar (precedence: `and` binds tighter than `or`):
//!   expr   := and ( "or" and )*
//!   and    := cmp ( "and" cmp )*
//!   cmp    := field OP literal
//!   field  := dotted column path (`status.phase`)
//!   OP     := == | != | > | < | >= | <=
//!
//! Operators may be glued (`size>1mb`) or spaced (`size > 1mb`). The RHS
//! literal is inferred (`1mb`→Filesize, `5sec`→Duration, ints/floats/bools,
//! else string). A quoted multi-word string is preserved as one token by the
//! upstream tokenizer; a quoted value that looks numeric is still inferred as

use std::cmp::Ordering;

use orkia_shell_types::Value;

use crate::exec::eval::infer_literal;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CmpOp {
    Eq,
    Ne,
    Gt,
    Lt,
    Ge,
    Le,
}

#[derive(Debug)]
pub enum Predicate {
    Or(Box<Predicate>, Box<Predicate>),
    And(Box<Predicate>, Box<Predicate>),
    Cmp {
        field: String,
        op: CmpOp,
        rhs: Value,
    },
}

#[derive(Debug, PartialEq)]
enum Lexeme {
    Word(String),
    Op(CmpOp),
    And,
    Or,
}

fn is_op_char(c: char) -> bool {
    matches!(c, '>' | '<' | '=' | '!')
}

fn op_from(symbol: &str) -> Option<CmpOp> {
    match symbol {
        "==" | "=" => Some(CmpOp::Eq),
        "!=" => Some(CmpOp::Ne),
        ">=" => Some(CmpOp::Ge),
        "<=" => Some(CmpOp::Le),
        ">" => Some(CmpOp::Gt),
        "<" => Some(CmpOp::Lt),
        _ => None,
    }
}

/// Lex tokens into lexemes, splitting glued operators from words.
fn lex(tokens: &[String]) -> Result<Vec<Lexeme>, String> {
    let mut out = Vec::new();
    for token in tokens {
        match token.as_str() {
            "and" => out.push(Lexeme::And),
            "or" => out.push(Lexeme::Or),
            _ => lex_token(token, &mut out)?,
        }
    }
    Ok(out)
}

/// Split a single token into alternating word / operator lexemes.
fn lex_token(token: &str, out: &mut Vec<Lexeme>) -> Result<(), String> {
    let mut word = String::new();
    let mut op = String::new();
    for c in token.chars() {
        if is_op_char(c) {
            if !word.is_empty() {
                out.push(Lexeme::Word(std::mem::take(&mut word)));
            }
            op.push(c);
        } else {
            if !op.is_empty() {
                out.push(Lexeme::Op(flush_op(&mut op)?));
            }
            word.push(c);
        }
    }
    if !op.is_empty() {
        out.push(Lexeme::Op(flush_op(&mut op)?));
    }
    if !word.is_empty() {
        out.push(Lexeme::Word(word));
    }
    Ok(())
}

fn flush_op(op: &mut String) -> Result<CmpOp, String> {
    let symbol = std::mem::take(op);
    op_from(&symbol).ok_or_else(|| format!("unknown operator `{symbol}`"))
}

struct Parser<'a> {
    lex: &'a [Lexeme],
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Lexeme> {
        self.lex.get(self.pos)
    }

    fn parse_or(&mut self) -> Result<Predicate, String> {
        let mut left = self.parse_and()?;
        while matches!(self.peek(), Some(Lexeme::Or)) {
            self.pos += 1;
            let right = self.parse_and()?;
            left = Predicate::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Predicate, String> {
        let mut left = self.parse_cmp()?;
        while matches!(self.peek(), Some(Lexeme::And)) {
            self.pos += 1;
            let right = self.parse_cmp()?;
            left = Predicate::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_cmp(&mut self) -> Result<Predicate, String> {
        let field = match self.peek() {
            Some(Lexeme::Word(w)) => w.clone(),
            _ => return Err("expected a field name".to_string()),
        };
        self.pos += 1;
        let op = match self.peek() {
            Some(Lexeme::Op(op)) => *op,
            _ => return Err(format!("expected an operator after `{field}`")),
        };
        self.pos += 1;
        let rhs = match self.peek() {
            Some(Lexeme::Word(w)) => infer_literal(w),
            _ => return Err(format!("expected a value after `{field}`")),
        };
        self.pos += 1;
        Ok(Predicate::Cmp { field, op, rhs })
    }
}

/// Parse a token slice into a [`Predicate`].
pub fn parse(tokens: &[String]) -> Result<Predicate, String> {
    if tokens.is_empty() {
        return Err("empty predicate".to_string());
    }
    let lexemes = lex(tokens)?;
    let mut parser = Parser {
        lex: &lexemes,
        pos: 0,
    };
    let predicate = parser.parse_or()?;
    if parser.pos != lexemes.len() {
        return Err("trailing tokens in predicate".to_string());
    }
    Ok(predicate)
}

/// Evaluate a predicate against a record value. A missing field, or an
/// incomparable pair, evaluates to `false` (fail-closed filtering).
pub fn eval(predicate: &Predicate, record: &Value) -> bool {
    match predicate {
        Predicate::Or(a, b) => eval(a, record) || eval(b, record),
        Predicate::And(a, b) => eval(a, record) && eval(b, record),
        Predicate::Cmp { field, op, rhs } => {
            let Some(lhs) = record.get_path(field) else {
                return false;
            };
            match op {
                CmpOp::Eq => lhs == rhs,
                CmpOp::Ne => lhs != rhs,
                _ => match lhs.compare(rhs) {
                    Some(ord) => ordering_matches(*op, ord),
                    None => false,
                },
            }
        }
    }
}

fn ordering_matches(op: CmpOp, ord: Ordering) -> bool {
    match op {
        CmpOp::Gt => ord == Ordering::Greater,
        CmpOp::Lt => ord == Ordering::Less,
        CmpOp::Ge => ord != Ordering::Less,
        CmpOp::Le => ord != Ordering::Greater,
        _ => false,
    }
}
