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

//! Decay driven by the program, in a binary of its own: with no background
//! thread and no allocation, only calls to `decay` return memory

use std::hint::black_box;
use std::thread;
use std::time::{Duration, Instant};
use stoolap_jemalloc::{Jemalloc, background};

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc::new();

#[test]
fn decay_returns_memory_while_the_program_allocates_nothing() {
    // 100 MiB of large runs fill many chunks; freed, they go to the spare
    // pool, from which only decay unmaps them
    let blocks: Vec<Vec<u8>> = (0..200)
        .map(|i| black_box(vec![i as u8; 512 << 10]))
        .collect();
    let full = stoolap_jemalloc::stats().chunks;
    assert!(full >= 100 << 20);
    drop(blocks);

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        stoolap_jemalloc::decay();
        let chunks = stoolap_jemalloc::stats().chunks;
        if chunks < full / 4 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{chunks} of {full} chunk bytes still mapped"
        );
        thread::sleep(Duration::from_millis(250));
    }
    assert!(!background::is_running(), "no thread did it");
}
