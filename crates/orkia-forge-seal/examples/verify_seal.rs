// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: verify_seal <seal-dir>");
        std::process::exit(2)
    });
    match orkia_forge_seal::verify_chain(std::path::Path::new(&path)) {
        Ok(r) => println!("OK events={} last_hash={}", r.events, r.last_hash),
        Err(e) => {
            eprintln!("ERR {e}");
            std::process::exit(1)
        }
    }
}
