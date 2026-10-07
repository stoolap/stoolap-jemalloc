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

//! The profiler with every allocation sampled, in a binary of its own

mod common;

use std::alloc::{GlobalAlloc, Layout};
use std::hint::black_box;
use stoolap_jemalloc::{Jemalloc, prof};

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

/// Allocates through a chain of `left` and `right` calls spelled by the
/// bits of `path`, so that each path makes a stack of its own
#[inline(never)]
fn allocate_on_path(path: u32, depth: u32) -> Vec<u8> {
    if depth == 0 {
        vec![path as u8; 64]
    } else if path & 1 == 0 {
        black_box(left(path >> 1, depth - 1))
    } else {
        black_box(right(path >> 1, depth - 1))
    }
}

// `left` and `right` differ in their bodies: identical functions would be
// folded into one, and all paths would make the same stack

#[inline(never)]
fn left(path: u32, depth: u32) -> Vec<u8> {
    let v = allocate_on_path(path, depth);
    black_box(&v);
    v
}

#[inline(never)]
fn right(path: u32, depth: u32) -> Vec<u8> {
    let v = allocate_on_path(path, depth);
    black_box(v.len());
    v
}

#[test]
fn every_kind_of_sampled_allocation() {
    assert!(!prof::is_active());
    let err = prof::dump_pprof().unwrap_err();
    assert_eq!(err.to_string(), "heap profiling was never activated");

    prof::set_sample_interval(1);
    assert_eq!(prof::sample_interval(), 1);
    prof::activate();
    assert!(prof::is_active());
    // A thread notices activation within 1 MiB of allocation
    drop(black_box(vec![0u8; 2 << 20]));

    // 1024 stacks, more than the table first holds, so it grows
    let boxes: Vec<_> = (0..1024).map(|path| allocate_on_path(path, 10)).collect();

    unsafe {
        // Zeroed, over-aligned and huge allocations, each sampled
        for (size, align) in [(1000, 8), (5000, 8192), (3 << 20, 8), (100, 1 << 21)] {
            let layout = Layout::from_size_align(size, align).unwrap();
            let p = GLOBAL.alloc_zeroed(layout);
            assert_eq!(p as usize % align, 0);
            assert!((0..size).step_by(97).all(|i| *p.add(i) == 0));
            p.write_bytes(5, size);
            // Reallocating a sample moves it, keeping its contents
            let q = GLOBAL.realloc(p, layout, size + 1);
            assert_eq!(q as usize % align, 0);
            assert!((0..size).step_by(97).all(|i| *q.add(i) == 5));
            GLOBAL.dealloc(q, Layout::from_size_align(size + 1, align).unwrap());
        }
    }

    prof::deactivate();
    assert!(!prof::is_active());
    let profile = prof::dump_pprof().unwrap();
    #[cfg(feature = "symbolize")]
    assert!(String::from_utf8_lossy(&profile).contains("allocate_on_path"));
    // A sample per stack, and the 1024 paths make 1024 stacks
    let samples = common::fields(&profile)
        .iter()
        .filter(|(field, _)| *field == 2)
        .count();
    assert!(samples >= 1024, "{samples} samples");
    drop(boxes);

    // Write errors come back as I/O errors
    let err = prof::write_pprof("/nonexistent-dir/heap.pb").unwrap_err();
    assert!(matches!(err, prof::DumpError::Io(_)));
    assert!(err.to_string().starts_with("writing the heap profile"));
}
