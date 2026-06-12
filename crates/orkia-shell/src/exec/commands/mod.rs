// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! Six commands chosen to exercise every behavior of the socle: a producer
//! (`ls`), a streaming transformer (`where`), early termination (`first`), a
//! collecting/blocking command (`sort-by`), a boundary converter (`from
//! json`), and the migration of an existing builtin (`ps`).

pub mod attention;
pub mod blocks_adapter;
pub mod briefing;
pub mod first;
pub mod from_json;
pub mod help;
pub mod history;
pub mod jobs;
pub mod journal;
pub mod log;
pub mod ls;
pub mod plan;
pub mod predicate;
pub mod ps;
pub mod route;
pub mod sort_by;
pub mod version;
pub mod where_cmd;
pub mod whoami;
