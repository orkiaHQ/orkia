// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use chrono::{DateTime, Utc};
use std::collections::HashSet;

pub(super) fn recency_score(timestamp: DateTime<Utc>) -> i32 {
    let age = Utc::now().signed_duration_since(timestamp);
    if age.num_hours() < 1 {
        16
    } else if age.num_hours() < 24 {
        12
    } else if age.num_days() < 7 {
        8
    } else if age.num_days() < 30 {
        4
    } else {
        0
    }
}

pub(super) fn agent_score(expected: Option<&str>, actual: Option<&str>) -> i32 {
    match (expected.map(clean_agent), actual.map(clean_agent)) {
        (Some(expected), Some(actual)) if expected == actual => 12,
        _ => 0,
    }
}

pub(super) fn cwd_score(expected: Option<&str>, actual: Option<&str>) -> i32 {
    let (Some(expected), Some(actual)) = (expected, actual) else {
        return 0;
    };
    if expected == actual {
        return 10;
    }
    if expected.starts_with(actual) || actual.starts_with(expected) {
        return 6;
    }
    0
}

pub(super) fn domain_score(expected: Option<&str>, actual: Option<&str>, question: &str) -> i32 {
    if let (Some(expected), Some(actual)) = (expected, actual)
        && expected.eq_ignore_ascii_case(actual)
    {
        return 14;
    }
    let Some(actual) = actual else {
        return 0;
    };
    if tokens(question).iter().any(|token| token == actual) {
        6
    } else {
        0
    }
}

pub(super) fn term_score(question: &str, text: &str) -> i32 {
    let haystack = text.to_lowercase();
    tokens(question)
        .iter()
        .filter(|term| haystack.contains(term.as_str()))
        .count() as i32
        * 3
}

pub(super) fn semantic_score(question: &str, text: &str) -> i32 {
    let q = normalized_tokens(question);
    let t = normalized_tokens(text);
    if q.is_empty() || t.is_empty() {
        return 0;
    }
    phrase_score(&q, &t) + coverage_score(&q, &t) + bigram_score(&q, &t)
}

pub(super) fn tokens(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|term| term.len() >= 3)
        .map(str::to_ascii_lowercase)
        .collect()
}

fn normalized_tokens(text: &str) -> Vec<String> {
    tokens(text)
        .into_iter()
        .map(|token| {
            token
                .trim_end_matches("ing")
                .trim_end_matches("ed")
                .trim_end_matches('s')
                .to_string()
        })
        .collect()
}

fn phrase_score(question: &[String], text: &[String]) -> i32 {
    if question.len() < 2 || text.len() < question.len() {
        return 0;
    }
    if text
        .windows(question.len())
        .any(|window| window == question)
    {
        16
    } else {
        0
    }
}

fn coverage_score(question: &[String], text: &[String]) -> i32 {
    let text_terms = text.iter().collect::<HashSet<_>>();
    let overlap = question
        .iter()
        .filter(|token| text_terms.contains(token))
        .count();
    let coverage = overlap as f32 / question.len() as f32;
    (coverage * 12.0).round() as i32
}

fn bigram_score(question: &[String], text: &[String]) -> i32 {
    let q = bigrams(question);
    let t = bigrams(text);
    if q.is_empty() || t.is_empty() {
        return 0;
    }
    let overlap = q.iter().filter(|item| t.contains(*item)).count();
    let coverage = overlap as f32 / q.len() as f32;
    (coverage * 10.0).round() as i32
}

fn bigrams(tokens: &[String]) -> HashSet<String> {
    tokens
        .windows(2)
        .map(|pair| format!("{} {}", pair[0], pair[1]))
        .collect()
}

fn clean_agent(agent: &str) -> String {
    agent.trim_start_matches('@').to_ascii_lowercase()
}
