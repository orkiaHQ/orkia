// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use chrono::{DateTime, Utc};

#[derive(Debug, Clone)]
pub struct AskArgs {
    pub question: String,
    pub agent: Option<String>,
    pub evidence_agent: Option<String>,
    pub domain: Option<String>,
    pub cwd: Option<String>,
    pub last: usize,
    pub job: Option<u32>,
    pub rfc: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub evidence_only: bool,
    pub timeout_ms: u64,
    pub json: bool,
}

pub fn parse_ask_args(args: &[String]) -> Result<AskArgs, String> {
    let mut question = Vec::new();
    let mut agent = None;
    let mut evidence_agent = None;
    let mut domain = None;
    let mut cwd = None;
    let mut last = 8;
    let mut job = None;
    let mut rfc = None;
    let mut since = None;
    let mut evidence_only = false;
    let mut timeout_ms = 1_500;
    let mut json = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--agent" => agent = Some(parse_named_arg(&mut iter, "--agent", "@name")?),
            "--evidence-agent" => {
                evidence_agent = Some(parse_named_arg(&mut iter, "--evidence-agent", "@name")?);
            }
            "--domain" => {
                let raw = parse_required_arg(&mut iter, "--domain", "a value")?;
                domain = Some(raw.to_ascii_lowercase());
            }
            "--cwd" => cwd = Some(parse_required_arg(&mut iter, "--cwd", "a path")?),
            "--last" => {
                let raw = parse_required_arg(&mut iter, "--last", "a number")?;
                last = raw
                    .parse::<usize>()
                    .map_err(|_| format!("operator ask: invalid --last `{raw}`"))?
                    .clamp(1, 20);
            }
            "--job" => {
                let raw = parse_required_arg(&mut iter, "--job", "an id")?;
                job = Some(
                    raw.parse::<u32>()
                        .map_err(|_| format!("operator ask: invalid --job `{raw}`"))?,
                );
            }
            "--rfc" => rfc = Some(parse_required_arg(&mut iter, "--rfc", "an id")?),
            "--since" => {
                let raw = parse_required_arg(&mut iter, "--since", "RFC3339 or NN[smhd]")?;
                since = Some(parse_since(&raw)?);
            }
            "--evidence" => evidence_only = true,
            "--timeout-ms" => {
                let raw = parse_required_arg(&mut iter, "--timeout-ms", "a number")?;
                timeout_ms = raw
                    .parse::<u64>()
                    .map_err(|_| format!("operator ask: invalid --timeout-ms `{raw}`"))?
                    .clamp(100, 30_000);
            }
            "--json" => json = true,
            other => question.push(other.to_string()),
        }
    }
    let question = question.join(" ");
    if question.trim().is_empty() {
        return Err(
            "usage: operator ask <question> [--agent @name] [--evidence-agent @name] [--domain NAME] [--cwd PATH] [--last N] [--job ID] [--rfc ID] [--since NN[smhd]|RFC3339] [--evidence] [--timeout-ms N] [--json]"
                .into(),
        );
    }
    Ok(AskArgs {
        question,
        agent,
        evidence_agent,
        domain,
        cwd,
        last,
        job,
        rfc,
        since,
        evidence_only,
        timeout_ms,
        json,
    })
}

fn parse_named_arg<'a>(
    iter: &mut impl Iterator<Item = &'a String>,
    flag: &str,
    expected: &str,
) -> Result<String, String> {
    let raw = parse_required_arg(iter, flag, expected)?;
    let name = raw.trim_start_matches('@');
    if name.is_empty() {
        return Err(format!("operator ask: {flag} requires {expected}"));
    }
    Ok(name.to_string())
}

fn parse_required_arg<'a>(
    iter: &mut impl Iterator<Item = &'a String>,
    flag: &str,
    expected: &str,
) -> Result<String, String> {
    let Some(raw) = iter.next() else {
        return Err(format!("operator ask: {flag} requires {expected}"));
    };
    if raw.trim().is_empty() {
        return Err(format!("operator ask: {flag} requires {expected}"));
    }
    Ok(raw.to_string())
}

fn parse_since(raw: &str) -> Result<DateTime<Utc>, String> {
    if let Ok(absolute) = DateTime::parse_from_rfc3339(raw) {
        return Ok(absolute.with_timezone(&Utc));
    }
    let unit_char = raw
        .chars()
        .last()
        .ok_or_else(|| "operator ask: --since requires RFC3339 or NN[smhd]".to_string())?;
    let (num_part, unit) = raw.split_at(raw.len() - unit_char.len_utf8());
    let n: i64 = num_part
        .parse()
        .map_err(|_| format!("operator ask: invalid --since `{raw}`"))?;
    let seconds = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3_600,
        "d" => n * 86_400,
        _ => return Err(format!("operator ask: invalid --since `{raw}`")),
    };
    Ok(Utc::now() - chrono::Duration::seconds(seconds))
}
