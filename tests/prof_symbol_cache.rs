// Copyright 2026 Stoolap Contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Memory a dump leaves behind, in a binary of its own, as the symbolizer's
//! cache is the process's
#![cfg(feature = "symbolize")]

mod common;

use std::hint::black_box;
use stoolap_jemalloc::{Jemalloc, prof};

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc::new();

const INUSE_BYTES: usize = 3;

/// The symbolizer parses the debug information of every library it looks
/// up, tens of megabytes for a large one, and must not keep it after the
/// dump. Only its list of the libraries stays, a few kilobytes.
#[test]
fn a_dump_keeps_no_memory() {
    prof::set_sample_interval(1);
    prof::activate();
    // Past the wait drawn at the default interval
    drop(black_box(vec![0u8; 2 << 20]));
    prof::dump_pprof().unwrap();
    let live = common::parse(&prof::dump_pprof().unwrap()).totals("symbolize")[INUSE_BYTES];
    assert!(live < 64 << 10, "{live} bytes live from the first dump");
}
