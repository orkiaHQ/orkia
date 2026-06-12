// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! S0 foundation flows (F001–F005): boot, login, RFC→SEAL, shell→agent
//! pipe, agent job control.

mod f001;
mod f002;
mod f003;
mod f004;
mod f005;

pub(crate) use f001::flow_f001;
pub(crate) use f002::flow_f002;
pub(crate) use f003::flow_f003;
pub(crate) use f004::flow_f004;
pub(crate) use f005::flow_f005;
