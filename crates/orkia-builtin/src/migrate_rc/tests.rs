// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::*;

// ───── bash: trivial passthrough ────────────────────────────────────

#[test]
fn bash_passes_everything_through() {
    let input = "# comment\nexport FOO=bar\nalias ll='ls -la'\n\nfunction hi() { echo hi; }\n";
    let lines = classify_lines(input, SourceKind::Bash);
    assert!(matches!(lines[0], LineClass::Comment(_)));
    assert!(matches!(lines[1], LineClass::Migrate(_)));
    assert!(matches!(lines[2], LineClass::Migrate(_)));
    assert!(matches!(lines[3], LineClass::Blank));
    assert!(matches!(lines[4], LineClass::Migrate(_)));
}

// ───── zsh: migrate / skip rules ────────────────────────────────────

#[test]
fn zsh_export_alias_source_are_migrated() {
    for line in [
        "export EDITOR=helix",
        "alias gs='git status'",
        "source ~/.cargo/env",
        ". /etc/profile.d/foo.sh",
        "eval \"$(starship init bash)\"",
        "PATH=/usr/local/bin:$PATH",
        "VALID_VAR=hello",
        "MULTI_WORD_=\"hi there\"",
    ] {
        let classified = classify_line(line, SourceKind::Zsh);
        assert!(
            matches!(classified, LineClass::Migrate(_)),
            "expected Migrate for {line:?}, got {classified:?}"
        );
    }
}

#[test]
fn zsh_setopt_autoload_bindkey_are_skipped() {
    let cases: &[(&str, SkipReason)] = &[
        ("setopt autocd", SkipReason::ZshSetopt),
        ("unsetopt beep", SkipReason::ZshSetopt),
        ("autoload -Uz compinit", SkipReason::ZshAutoload),
        ("compinit", SkipReason::ZshAutoload),
        ("compdef _git gco=git-checkout", SkipReason::ZshAutoload),
        ("bindkey '^R' history-search", SkipReason::ZshBindkey),
        ("typeset -A my_map", SkipReason::ZshTypeset),
        ("zplugin light foo/bar", SkipReason::ZshPlugin),
        ("zinit light foo/bar", SkipReason::ZshPlugin),
        ("antigen bundle foo", SkipReason::ZshPlugin),
        (
            "source $HOME/.oh-my-zsh/oh-my-zsh.sh",
            SkipReason::ZshPlugin,
        ),
        ("for k in ${(k)my_map}; do", SkipReason::ZshArrayExpansion),
        ("PROMPT='%F{red}%n@%~%f$ '", SkipReason::ZshPrompt),
    ];
    for (line, expected) in cases {
        match classify_line(line, SourceKind::Zsh) {
            LineClass::Skip(_, got) => assert_eq!(
                got, *expected,
                "{line:?} → wrong skip reason: {got:?} (expected {expected:?})"
            ),
            other => panic!("{line:?} → expected Skip({expected:?}), got {other:?}"),
        }
    }
}

#[test]
fn zsh_plain_prompt_is_migrated() {
    // PROMPT without zsh-specific escapes is fine in bash.
    let classified = classify_line("PROMPT='$ '", SourceKind::Zsh);
    assert!(matches!(classified, LineClass::Migrate(_)));
}

#[test]
fn zsh_blank_and_comment_preserved() {
    assert!(matches!(
        classify_line("", SourceKind::Zsh),
        LineClass::Blank
    ));
    assert!(matches!(
        classify_line("   ", SourceKind::Zsh),
        LineClass::Blank
    ));
    assert!(matches!(
        classify_line("# a comment", SourceKind::Zsh),
        LineClass::Comment(_)
    ));
}

#[test]
fn zsh_assignment_with_space_around_equals_not_assignment() {
    // `FOO = bar` is not a valid bash assignment either; we shouldn't
    // try to migrate it as one. Falls through to the default Migrate
    // path (the user will see brush's error if they kept it).
    let classified = classify_line("FOO = bar", SourceKind::Zsh);
    assert!(matches!(classified, LineClass::Migrate(_)));
}

// ───── fish: translation ────────────────────────────────────────────

#[test]
fn fish_set_gx_to_export() {
    let cases = &[
        ("set -gx EDITOR helix", "export EDITOR=helix"),
        ("set -Ux FOO bar", "export FOO=bar"),
        (
            "set -gx PATH /usr/local/bin $PATH",
            "export PATH=/usr/local/bin:$PATH",
        ),
        ("set -gx EMPTY", "export EMPTY="),
    ];
    for (input, want) in cases {
        match classify_line(input, SourceKind::Fish) {
            LineClass::Translate(orig, translated) => {
                assert_eq!(orig.trim(), *input);
                assert_eq!(translated, *want);
            }
            other => panic!("{input:?} → expected Translate({want:?}), got {other:?}"),
        }
    }
}

#[test]
fn fish_abbr_to_alias() {
    let cases = &[
        ("abbr -a gs git status", "alias gs='git status'"),
        ("abbr gs git status", "alias gs='git status'"),
        ("abbr --add gs git status", "alias gs='git status'"),
    ];
    for (input, want) in cases {
        match classify_line(input, SourceKind::Fish) {
            LineClass::Translate(_, translated) => assert_eq!(translated, *want),
            other => panic!("{input:?} → expected Translate({want:?}), got {other:?}"),
        }
    }
}

#[test]
fn fish_function_and_set_local_are_skipped() {
    let cases: &[(&str, SkipReason)] = &[
        ("function greet -d 'say hi'", SkipReason::FishFunction),
        ("end", SkipReason::FishFunction),
        ("set my_local_var hello", SkipReason::FishSet),
    ];
    for (line, expected) in cases {
        match classify_line(line, SourceKind::Fish) {
            LineClass::Skip(_, got) => assert_eq!(got, *expected, "{line:?}"),
            other => panic!("{line:?} → expected Skip({expected:?}), got {other:?}"),
        }
    }
}

#[test]
fn fish_source_is_migrated() {
    let classified = classify_line("source ~/.config/fish/fishrc.private", SourceKind::Fish);
    assert!(matches!(classified, LineClass::Migrate(_)));
}

// ───── output generation ───────────────────────────────────────────

#[test]
fn output_contains_header_translated_marker_and_skip_trailer() {
    let lines = vec![
        LineClass::Comment("# hi".into()),
        LineClass::Migrate("export FOO=bar".into()),
        LineClass::Translate("set -gx BAZ qux".into(), "export BAZ=qux".into()),
        LineClass::Skip("setopt autocd".into(), SkipReason::ZshSetopt),
        LineClass::Blank,
    ];
    let out = generate_orkiarc("/home/u/.zshrc", SourceKind::Zsh, &lines, "2026-05-20");
    assert!(out.contains("# ~/.orkiarc — generated by orkia migrate-rc from /home/u/.zshrc"));
    assert!(out.contains("# date: 2026-05-20"));
    assert!(out.contains("export FOO=bar\n"));
    assert!(
        out.contains("export BAZ=qux  # TRANSLATED\n"),
        "translated line should carry the TRANSLATED marker, got: {out}"
    );
    assert!(out.contains("# 1 line skipped"));
    assert!(out.contains("# SKIP (ZshSetopt): setopt autocd"));
}

#[test]
fn migration_counts_match() {
    let lines = vec![
        LineClass::Comment("# c".into()),
        LineClass::Migrate("export A=1".into()),
        LineClass::Migrate("export B=2".into()),
        LineClass::Translate("set -gx C 3".into(), "export C=3".into()),
        LineClass::Skip("setopt foo".into(), SkipReason::ZshSetopt),
        LineClass::Skip("bindkey x y".into(), SkipReason::ZshBindkey),
        LineClass::Blank,
    ];
    let c = MigrationCounts::from_lines(&lines);
    assert_eq!(c.migrated, 2);
    assert_eq!(c.translated, 1);
    assert_eq!(c.skipped, 2);
    assert_eq!(c.comments, 1);
}

// ───── source detection ────────────────────────────────────────────

#[test]
fn detect_kind_from_well_known_names() {
    use std::path::PathBuf;
    assert_eq!(
        detect_source_kind(&PathBuf::from("/home/u/.zshrc")),
        Some(SourceKind::Zsh)
    );
    assert_eq!(
        detect_source_kind(&PathBuf::from("/home/u/.bashrc")),
        Some(SourceKind::Bash)
    );
    assert_eq!(
        detect_source_kind(&PathBuf::from("/home/u/.bash_profile")),
        Some(SourceKind::Bash)
    );
    assert_eq!(
        detect_source_kind(&PathBuf::from("/home/u/.config/fish/config.fish")),
        Some(SourceKind::Fish)
    );
    assert_eq!(detect_source_kind(&PathBuf::from("/etc/hosts")), None,);
}

#[test]
fn content_sniff_defaults_to_bash_for_generic_rc() {
    // The finding-#3 case: a non-standard filename with only generic,
    // bash/zsh-identical syntax. Sniff → Bash (safe default).
    let rc = "alias gs=\"git status\"\nexport EDITOR=hx\n";
    assert_eq!(detect_source_kind_from_content(rc), SourceKind::Bash);
}

#[test]
fn content_sniff_detects_zsh_markers() {
    let rc = "alias gs=\"git status\"\nsetopt AUTO_CD\nautoload -Uz compinit\n";
    assert_eq!(detect_source_kind_from_content(rc), SourceKind::Zsh);
}

#[test]
fn content_sniff_detects_fish_markers() {
    let rc = "set -gx EDITOR hx\nabbr gs 'git status'\n";
    assert_eq!(detect_source_kind_from_content(rc), SourceKind::Fish);
}

#[test]
fn content_sniff_ignores_markers_in_comments() {
    // A bash file merely *mentioning* setopt in a comment is still Bash.
    let rc = "# I used to call setopt here\nexport FOO=bar\n";
    assert_eq!(detect_source_kind_from_content(rc), SourceKind::Bash);
}

// ───── round-trip: classify → generate → result is sourceable shape ─

#[test]
fn round_trip_zshrc_with_mixed_content() {
    let input = "# zsh config\n\
                 export EDITOR=helix\n\
                 alias ll='ls -la'\n\
                 setopt autocd\n\
                 source ~/.cargo/env\n\
                 autoload -Uz compinit\n\
                 PROMPT='%F{red}%n%f$ '\n\
                 PATH=$HOME/bin:$PATH\n";
    let lines = classify_lines(input, SourceKind::Zsh);
    let counts = MigrationCounts::from_lines(&lines);
    assert_eq!(counts.migrated, 4); // export, alias, source, PATH
    assert_eq!(counts.skipped, 3); // setopt, autoload, PROMPT
    assert_eq!(counts.comments, 1);

    let out = generate_orkiarc("/home/u/.zshrc", SourceKind::Zsh, &lines, "2026-05-20");
    assert!(out.contains("export EDITOR=helix"));
    assert!(out.contains("alias ll='ls -la'"));
    assert!(out.contains("source ~/.cargo/env"));
    assert!(out.contains("PATH=$HOME/bin:$PATH"));
    // Skipped lines must NOT appear as live commands.
    assert!(!out.contains("\nsetopt autocd\n"));
    assert!(!out.contains("\nautoload "));
    // They DO appear in the comment trailer.
    assert!(out.contains("# SKIP (ZshSetopt): setopt autocd"));
    assert!(out.contains("# SKIP (ZshAutoload): autoload -Uz compinit"));
    assert!(out.contains("# SKIP (ZshPrompt): PROMPT="));
}
