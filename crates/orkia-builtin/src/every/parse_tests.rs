// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::*;

fn ok(s: &str) -> String {
    parse(s).expect("parse should succeed").to_line()
}

#[test]
fn every_n_minutes() {
    assert_eq!(ok("every 5 minutes"), "*/5 * * * *");
    assert_eq!(ok("every 1 minute"), "*/1 * * * *");
}

#[test]
fn every_n_hours_and_every_hour() {
    assert_eq!(ok("every 2 hours"), "0 */2 * * *");
    assert_eq!(ok("every hour"), "0 * * * *");
    assert_eq!(ok("hourly"), "0 * * * *");
}

#[test]
fn daily_default_and_with_time() {
    assert_eq!(ok("daily"), "0 9 * * *");
    assert_eq!(ok("daily 2:30pm"), "30 14 * * *");
    assert_eq!(ok("every day midnight"), "0 0 * * *");
}

#[test]
fn weekdays_and_weekends() {
    assert_eq!(ok("weekdays 8am"), "0 8 * * MON-FRI");
    assert_eq!(ok("weekends 10am"), "0 10 * * SAT,SUN");
    assert_eq!(ok("weekdays"), "0 9 * * MON-FRI");
}

#[test]
fn named_days_single_and_list() {
    assert_eq!(ok("monday 9am"), "0 9 * * MON");
    assert_eq!(ok("friday 5pm"), "0 17 * * FRI");
    assert_eq!(ok("mon and wed and fri 9am"), "0 9 * * MON,WED,FRI");
    assert_eq!(ok("tue, thu 14:00"), "0 14 * * TUE,THU");
}

#[test]
fn monthly_ordinals() {
    assert_eq!(ok("1st of month"), "0 9 1 * *");
    assert_eq!(ok("15th of the month 10am"), "0 10 15 * *");
}

#[test]
fn twice_a_day_and_bare_time() {
    assert_eq!(ok("twice a day"), "0 9,18 * * *");
    assert_eq!(ok("9am"), "0 9 * * *");
    assert_eq!(ok("23:45"), "45 23 * * *");
}

#[test]
fn time_token_corner_cases() {
    assert_eq!(parse_time_token("12am"), Some((0, 0)));
    assert_eq!(parse_time_token("12pm"), Some((0, 12)));
    assert_eq!(parse_time_token("1am"), Some((0, 1)));
    assert_eq!(parse_time_token("1pm"), Some((0, 13)));
    assert_eq!(parse_time_token("noon"), Some((0, 12)));
    assert_eq!(parse_time_token("midnight"), Some((0, 0)));
    assert!(parse_time_token("13am").is_none());
    assert!(parse_time_function_safe("notatime"));
}

fn parse_time_function_safe(s: &str) -> bool {
    parse_time_token(s).is_none()
}

#[test]
fn unrecognised_returns_error() {
    assert_eq!(
        parse("purple monkey dishwasher"),
        Err(ParseError::Unrecognised)
    );
    assert_eq!(parse(""), Err(ParseError::Unrecognised));
}

#[test]
fn invalid_values_rejected() {
    assert!(matches!(
        parse("every 0 minutes"),
        Err(ParseError::InvalidValue(_))
    ));
    assert!(matches!(
        parse("every 99 hours"),
        Err(ParseError::InvalidValue(_))
    ));
    assert!(matches!(
        parse("99th of month"),
        Err(ParseError::InvalidValue(_))
    ));
}
