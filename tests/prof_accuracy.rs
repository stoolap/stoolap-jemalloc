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

//! The profiler's estimates against known allocation counts, in a binary
//! of its own. Counts are read from the stacks of marker functions, since
//! dumps allocate too.
#![cfg(feature = "symbolize")]

mod common;

use std::alloc::{GlobalAlloc, Layout};
use std::hint::black_box;
use std::thread;
use stoolap_jemalloc::{Jemalloc, prof};

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

const ALLOC_OBJECTS: usize = 0;
const INUSE_OBJECTS: usize = 2;
const INUSE_BYTES: usize = 3;

fn totals(function: &str) -> [u64; 4] {
    common::parse(&prof::dump_pprof().unwrap()).totals(function)
}

const ALLOC_BYTES: usize = 1;

/// Within `percent` of `want`
fn near(got: u64, want: u64, percent: u64) -> bool {
    got.abs_diff(want) * 100 <= want * percent
}

// The markers allocate in plain loops: shared iterator code would be
// folded into one function that both markers' stacks then name

#[inline(never)]
fn page_aligned_bytes(n: usize) -> Vec<*mut u8> {
    let layout = Layout::from_size_align(1, 4096).unwrap();
    let mut ptrs = Vec::with_capacity(n);
    for _ in 0..n {
        ptrs.push(black_box(unsafe { GLOBAL.alloc(layout) }));
    }
    ptrs
}

#[inline(never)]
fn four_kib_blocks(n: usize) -> Vec<*mut u8> {
    let layout = Layout::from_size_align(4096, 8).unwrap();
    let mut ptrs = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        ptrs.push(black_box(unsafe { GLOBAL.alloc(layout) }));
        i += 1;
    }
    ptrs
}

#[inline(never)]
fn one_block_after_a_raise() -> Vec<u8> {
    black_box(vec![1u8; 4096])
}

#[inline(never)]
fn blocks_after_a_cut(n: usize) -> Vec<Vec<u8>> {
    let mut blocks = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        blocks.push(black_box(vec![1u8; 4096]));
        i += 1;
    }
    blocks
}

#[inline(never)]
fn short_thread_block() {
    drop(black_box(vec![1u8; 64 << 10]));
}

trait First {
    fn allocate(&self) -> Vec<u8>;
}

impl First for common::traits::Twice {
    // A body of its own: identical functions get folded into one
    #[inline(never)]
    fn allocate(&self) -> Vec<u8> {
        black_box(vec![11u8; 6000])
    }
}

#[inline(never)]
fn first_block_of_a_thread() -> Vec<u8> {
    black_box(vec![7u8; 16 << 20])
}

#[test]
fn estimates_match_the_allocations() {
    prof::set_sample_interval(4096);
    prof::activate();
    // Let this thread notice activation
    drop(black_box(vec![0u8; 2 << 20]));

    // An allocation aligned to a page takes a page: the chance to be
    // sampled and the weight of a sample both come from that page
    let ptrs = page_aligned_bytes(10_000);
    let objects = totals("page_aligned_bytes")[ALLOC_OBJECTS];
    assert!(near(objects, 10_000, 5), "{objects} objects for 10000");
    for p in ptrs {
        unsafe { GLOBAL.dealloc(p, Layout::from_size_align(1, 4096).unwrap()) };
    }

    // Fractional weights add up before rounding: 100000 allocations, each
    // sampled with probability 1 - 1/e, are not counted as 2 per sample
    let ptrs = four_kib_blocks(100_000);
    let live = totals("four_kib_blocks");
    assert!(near(live[ALLOC_OBJECTS], 100_000, 3), "{live:?} for 100000");
    assert!(near(live[INUSE_OBJECTS], 100_000, 3), "{live:?} for 100000");
    for p in ptrs {
        unsafe { GLOBAL.dealloc(p, Layout::from_size_align(4096, 8).unwrap()) };
    }
    assert_eq!(totals("four_kib_blocks")[INUSE_OBJECTS], 0);

    // A new thread's first allocation counts while profiling is active
    prof::set_sample_interval(1);
    let blocks: Vec<Vec<u8>> = (0..10)
        .map(|_| thread::spawn(first_block_of_a_thread).join().unwrap())
        .collect();
    let live = totals("first_block_of_a_thread")[INUSE_BYTES];
    assert!(live >= 160 << 20, "{live} live bytes for 160 MiB");
    drop(blocks);

    // Functions are told apart by their full names and files, though two
    // traits' methods on one type simplify to the same name
    {
        use common::traits::{Second, Twice};
        // This thread last drew a wait before the interval became 1
        drop(black_box(vec![0u8; 1 << 20]));
        let kept = (First::allocate(&Twice), Second::allocate(&Twice));
        let profile = common::parse(&prof::dump_pprof().unwrap());
        let mut files: Vec<&str> = profile
            .functions
            .values()
            .filter(|f| f.name.ends_with("Twice::allocate"))
            .map(|f| f.file.as_str())
            .collect();
        files.sort_unstable();
        assert_eq!(files.len(), 2, "{files:?}");
        assert!(files[0].ends_with("common/traits.rs") && files[1].ends_with("prof_accuracy.rs"));
        drop(kept);
    }

    // A wait drawn before the interval was raised weighs its sample with
    // the interval it was drawn with: one block counts as about one
    prof::set_sample_interval(4096);
    drop(black_box(vec![0u8; 1 << 20]));
    prof::set_sample_interval(1 << 30);
    let block = one_block_after_a_raise();
    let counted = totals("one_block_after_a_raise");
    assert!(
        counted[ALLOC_OBJECTS] <= 2 && counted[ALLOC_BYTES] <= 3 * 4096,
        "{counted:?}"
    );
    drop(block);

    // After the interval is cut, a long wait drawn before does not hide
    // what comes next; threads notice the change within 64 KiB
    prof::set_sample_interval(usize::MAX / 4);
    drop(black_box(vec![0u8; 1 << 20]));
    prof::set_sample_interval(4096);
    let blocks = blocks_after_a_cut(10_000);
    let objects = totals("blocks_after_a_cut")[ALLOC_OBJECTS];
    assert!(near(objects, 10_000, 5), "{objects} objects for 10000");
    drop(blocks);

    // Short-lived threads each draw their own waits, also when they reuse
    // the cache memory of threads that exited: 2000 × 64 KiB
    prof::set_sample_interval(1 << 20);
    for _ in 0..2000 {
        thread::spawn(short_thread_block).join().unwrap();
    }
    let bytes = totals("short_thread_block")[ALLOC_BYTES];
    assert!(
        near(bytes, 2000 << 16, 35),
        "{bytes} bytes for 2000 × 64 KiB"
    );
}
