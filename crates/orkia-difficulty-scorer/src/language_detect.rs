// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

pub fn detect_english(prompt: &str) -> bool {
    // `prompt` is untrusted user input; a byte-index slice at 500 panics if it
    // lands inside a multibyte char (CJK/emoji). Walk back to a char boundary.
    let limit = prompt.len().min(500);
    let end = (0..=limit)
        .rev()
        .find(|&i| prompt.is_char_boundary(i))
        .unwrap_or(0);
    let sample = &prompt[..end];
    let words: Vec<&str> = sample.split_whitespace().take(50).collect();

    const ENGLISH_COMMON: &[&str] = &[
        "the", "is", "are", "was", "were", "have", "has", "do", "does", "will", "would", "can",
        "could", "should", "a", "an", "and", "or", "but", "in", "on", "at", "to", "for", "of",
        "with", "that", "this", "it", "not", "from", "be", "as", "by", "i", "you", "we", "they",
        "he", "she", "my", "your",
    ];

    let english_count = words
        .iter()
        .filter(|w| {
            let lower = w.to_lowercase();
            ENGLISH_COMMON.contains(&lower.as_str())
        })
        .count();

    english_count as f32 / words.len().max(1) as f32 > 0.25
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_text_detected() {
        assert!(detect_english(
            "Write a Python function to sort a list of integers"
        ));
    }

    #[test]
    fn chinese_text_not_english() {
        assert!(!detect_english("写一个Python函数来排序列表"));
    }

    #[test]
    fn german_text_not_english() {
        assert!(!detect_english(
            "Erstelle eine Funktion die Daten verarbeitet und Ergebnis zurückgibt"
        ));
    }

    #[test]
    fn code_only_not_english() {
        assert!(!detect_english(
            "fn main() { let x = vec![1,2,3]; println!(\"{:?}\", x); }"
        ));
    }

    #[test]
    fn mixed_english_code_detected() {
        assert!(detect_english(
            "Write a function that does the following: parse the input and return a result"
        ));
    }
}
