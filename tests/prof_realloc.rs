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

//! Live bytes after reallocations, at the default sample interval, in a
//! binary of its own. A reallocation counts as an allocation of the new
//! size whether it moves or not, so every buffer ends with the same chance
//! to be in the profile.
#![cfg(feature = "symbolize")]

mod common;

use std::hint::black_box;
use stoolap_jemalloc::{Jemalloc, prof};

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc::new().with_profiling();

const INUSE_BYTES: usize = 3;
const BUFFERS: usize = 100_000;

/// Buffers made for 1024 bytes and shrunk to the 1000 they hold, which
/// stays in the same size class, so the reallocation keeps them in place
#[inline(never)]
fn shrunk_buffers() -> Vec<Vec<u8>> {
    let mut kept = Vec::with_capacity(BUFFERS);
    let mut i = 0;
    while i < BUFFERS {
        let mut v: Vec<u8> = Vec::with_capacity(1024);
        v.resize(1000, 1);
        v.shrink_to_fit();
        kept.push(black_box(v));
        i += 1;
    }
    kept
}

#[test]
fn live_bytes_survive_reallocation() {
    assert_eq!(prof::sample_interval(), prof::DEFAULT_INTERVAL);
    let kept = shrunk_buffers();
    let live = common::parse(&prof::dump_pprof().unwrap()).totals("shrunk_buffers")[INUSE_BYTES];
    // The buffers take 1024 bytes each: about 195 samples, so 35% is five
    // standard deviations
    let want = (BUFFERS * 1024) as u64;
    assert!(
        live.abs_diff(want) * 100 <= want * 35,
        "{live} live bytes for {want}"
    );
    drop(kept);
}
