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

//! Small workloads that Miri can run in reasonable time, to check the
//! allocator against Rust's aliasing and data race rules:
//!
//! ```sh
//! MIRIFLAGS=-Zmiri-ignore-leaks cargo +nightly miri test --test miri
//! ```
//!
//! They run as ordinary tests too.

use std::alloc::{GlobalAlloc, Layout};
use std::thread;
use stoolap_jemalloc::Jemalloc;

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

#[test]
fn thread_cache_round_trip() {
    let layout = Layout::from_size_align(8, 8).unwrap();
    unsafe {
        let p = GLOBAL.alloc(layout);
        p.write(1);
        GLOBAL.dealloc(p, layout);
        let q = GLOBAL.alloc(layout);
        q.write(2);
        GLOBAL.dealloc(q, layout);
    }
}

/// Threads working on different size classes share chunks, while each
/// holds only its own class's lock
#[test]
fn threads_on_different_classes() {
    let handles: Vec<_> = [16usize, 48, 200, 3000]
        .into_iter()
        .map(|size| {
            thread::spawn(move || {
                let layout = Layout::from_size_align(size, 8).unwrap();
                for _ in 0..3 {
                    // More than a cache holds, so the threads refill and flush
                    let ptrs: Vec<*mut u8> =
                        (0..300).map(|_| unsafe { GLOBAL.alloc(layout) }).collect();
                    for &p in &ptrs {
                        unsafe { p.write(size as u8) };
                    }
                    for p in ptrs {
                        unsafe {
                            assert_eq!(*p, size as u8);
                            GLOBAL.dealloc(p, layout);
                        }
                    }
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

/// Large runs and huge mappings from several threads at once
#[test]
fn threads_on_large_and_huge() {
    let handles: Vec<_> = [40_000usize, 300_000, 2 << 20]
        .into_iter()
        .map(|size| {
            thread::spawn(move || {
                let layout = Layout::from_size_align(size, 8).unwrap();
                for _ in 0..4 {
                    unsafe {
                        let p = GLOBAL.alloc(layout);
                        p.write(7);
                        let q = GLOBAL.realloc(p, layout, size * 2);
                        assert_eq!(*q, 7);
                        GLOBAL.dealloc(q, Layout::from_size_align(size * 2, 8).unwrap());
                    }
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

/// Allocates and frees from its destructor, after the thread's cache is gone
struct LateUser;

impl Drop for LateUser {
    fn drop(&mut self) {
        let small: Vec<Box<u64>> = (0..20).map(Box::new).collect();
        let large = vec![1u8; 40_000];
        assert_eq!(*small[19] + u64::from(large[39_999]), 20);
    }
}

thread_local! {
    static LATE: LateUser = const { LateUser };
}

#[test]
fn frees_while_a_thread_exits() {
    thread::spawn(|| {
        LATE.with(|_| {});
        drop(Box::new(5u32));
    })
    .join()
    .unwrap();
}
