// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file for terms.

//! Actor tests over fakes for the five seams (`KernelRpc`, `DetachedSpawner`,
//! `FinalResponseSource`, `DaemonJobs`, plus the resolver). The actor runs on
//! its real OS thread; tests drive fan-in by firing the subscribed
//! final-response callback and poll the on-disk issues store for the outcome.
//!
//! * [`support`] — shared fakes + helpers.
//! * [`fresh`] — a clean run from `start_run`.
//! * [`resume`] — reconstruction from `resume_run` (`SPEC` §4.3 / step 5.D).

mod convergence;
mod fleet;
mod fresh;
mod resume;
mod support;
