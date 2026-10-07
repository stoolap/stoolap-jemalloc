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

//! The allocator as the global allocator of this test binary

use std::alloc::{GlobalAlloc, Layout};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use stoolap_jemalloc::Jemalloc;

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc::new();

fn fill(p: *mut u8, len: usize, seed: u8) {
    for i in 0..len {
        unsafe { *p.add(i) = seed.wrapping_add(i as u8) };
    }
}

fn check(p: *const u8, len: usize, seed: u8) {
    for i in 0..len {
        assert_eq!(
            unsafe { *p.add(i) },
            seed.wrapping_add(i as u8),
            "byte {i} of {len}"
        );
    }
}

fn sizes() -> Vec<usize> {
    let mut v: Vec<usize> = (0..=256).collect();
    let mut s = 257;
    while s <= 8 << 20 {
        v.extend([s - 1, s, s + 1, s * 3 / 2]);
        s *= 2;
    }
    v
}

#[test]
fn sizes_and_alignments() {
    let mut align = 1;
    while align <= 1 << 23 {
        for &size in &sizes() {
            if size > 4 << 20 && align > 4096 {
                continue;
            }
            let layout = Layout::from_size_align(size, align).unwrap();
            unsafe {
                let p = GLOBAL.alloc(layout);
                assert!(!p.is_null());
                assert_eq!(p as usize % align, 0, "size {size} align {align}");
                fill(p, size, size as u8);
                let z = GLOBAL.alloc_zeroed(layout);
                assert_eq!(z as usize % align, 0);
                assert!(
                    (0..size).all(|i| *z.add(i) == 0),
                    "zeroed size {size} align {align}"
                );
                check(p, size, size as u8);
                GLOBAL.dealloc(p, layout);
                GLOBAL.dealloc(z, layout);
            }
        }
        align *= 4;
    }
}

/// Alignments above the chunk size put the header in the chunk below
#[test]
fn alignment_beyond_chunk() {
    for align in [8 << 20, 16 << 20] {
        for size in [1, 4096, 3 << 20, 9 << 20] {
            let layout = Layout::from_size_align(size, align).unwrap();
            unsafe {
                let p = GLOBAL.alloc(layout);
                assert!(!p.is_null());
                assert_eq!(p as usize % align, 0);
                fill(p, size, 3);
                check(p, size, 3);
                GLOBAL.dealloc(p, layout);
            }
        }
    }
}

#[test]
fn realloc_keeps_contents() {
    for &align in &[8, 64, 8192] {
        let mut size = 1;
        let layout = Layout::from_size_align(size, align).unwrap();
        let mut p = unsafe { GLOBAL.alloc(layout) };
        fill(p, size, 7);
        // Grow, then shrink, through every kind of allocation
        let steps: Vec<usize> = (0..26)
            .map(|i| 1usize << i)
            .chain((0..26).rev().map(|i| (1usize << i) + 3))
            .collect();
        for &new in &steps {
            p = unsafe { GLOBAL.realloc(p, Layout::from_size_align(size, align).unwrap(), new) };
            assert!(!p.is_null());
            assert_eq!(p as usize % align, 0);
            check(p, size.min(new), 7);
            fill(p, new, 7);
            size = new;
        }
        unsafe { GLOBAL.dealloc(p, Layout::from_size_align(size, align).unwrap()) };
    }
}

#[test]
fn collections() {
    let mut map = HashMap::new();
    let mut tree = BTreeMap::new();
    for i in 0..200_000u64 {
        map.insert(i, format!("value {i}"));
        tree.insert(i.to_string(), vec![i; (i % 50) as usize]);
    }
    for i in (0..200_000u64).step_by(3) {
        map.remove(&i);
        tree.remove(&i.to_string());
    }
    for i in 0..200_000u64 {
        assert_eq!(map.contains_key(&i), i % 3 != 0);
        if let Some(v) = tree.get(&i.to_string()) {
            assert!(v.iter().all(|&x| x == i));
        }
    }
}

/// Objects freed on other threads than the ones that made them
#[test]
fn cross_thread_frees() {
    let (tx, rx) = mpsc::sync_channel::<Vec<Box<[u8]>>>(64);
    let producers: Vec<_> = (0..4)
        .map(|t| {
            let tx = tx.clone();
            thread::spawn(move || {
                for round in 0..300 {
                    let batch: Vec<Box<[u8]>> = (0..100)
                        .map(|i| {
                            let size = (i * 37 + round * 13 + t) % 40_000 + 1;
                            vec![(size % 251) as u8; size].into_boxed_slice()
                        })
                        .collect();
                    tx.send(batch).unwrap();
                }
            })
        })
        .collect();
    drop(tx);
    let consumers = thread::spawn(move || {
        let mut n = 0;
        for batch in rx {
            for b in batch {
                assert!(b.iter().all(|&x| x == (b.len() % 251) as u8));
                n += 1;
            }
        }
        n
    });
    for p in producers {
        p.join().unwrap();
    }
    assert_eq!(consumers.join().unwrap(), 4 * 300 * 100);
}

/// Many short-lived threads, so caches get created, flushed and reused
#[test]
fn thread_churn() {
    for _ in 0..20 {
        let handles: Vec<_> = (0..16)
            .map(|t| {
                thread::spawn(move || {
                    let mut v: Vec<Vec<u64>> = Vec::new();
                    for i in 0..2000u64 {
                        v.push(vec![i ^ t; (i % 300) as usize]);
                        if i % 3 == 0 {
                            v.swap_remove((i as usize * 7) % v.len());
                        }
                    }
                    v.iter().map(std::vec::Vec::len).sum::<usize>()
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }
}

#[test]
fn stress_random() {
    let handles: Vec<_> = (0..8)
        .map(|t| {
            thread::spawn(move || {
                let mut rng = 0x1234_5678_9abc_def1u64 ^ t;
                let mut next = move || {
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    rng
                };
                let mut live: Vec<(*mut u8, Layout, u8)> = Vec::new();
                for _ in 0..200_000 {
                    let r = next();
                    if live.len() < 2000 && r % 3 != 0 {
                        let size = match r % 100 {
                            0 => (next() % (3 << 20)) as usize,
                            1..=5 => (next() % 100_000) as usize,
                            _ => (next() % 512) as usize,
                        };
                        let align = 1 << (next() % 7);
                        let layout = Layout::from_size_align(size, align).unwrap();
                        let p = unsafe { GLOBAL.alloc(layout) };
                        assert!(!p.is_null());
                        let seed = r as u8;
                        fill(p, size.min(64), seed);
                        live.push((p, layout, seed));
                    } else if !live.is_empty() {
                        let i = (r as usize / 3) % live.len();
                        let (p, layout, seed) = live.swap_remove(i);
                        check(p, layout.size().min(64), seed);
                        unsafe { GLOBAL.dealloc(p, layout) };
                    }
                }
                for (p, layout, seed) in live {
                    check(p, layout.size().min(64), seed);
                    unsafe { GLOBAL.dealloc(p, layout) };
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

/// A purge while other tests allocate alongside leaves the purged runs
/// usable; how much it returns is checked in `stats.rs`, alone
#[test]
fn purge_while_threads_allocate() {
    let blocks: Vec<Vec<u8>> = (0..64).map(|i| vec![i as u8; 600_000]).collect();
    drop(blocks);
    stoolap_jemalloc::purge();
    let blocks: Vec<Vec<u8>> = (0..64).map(|i| vec![i as u8; 600_000]).collect();
    for (i, b) in blocks.iter().enumerate() {
        assert!(b[0] == i as u8 && b[599_999] == i as u8);
    }
}

static LATE_DROPS: AtomicUsize = AtomicUsize::new(0);

/// Allocates from its destructor, which runs as the thread exits
struct LateUser;

impl Drop for LateUser {
    fn drop(&mut self) {
        let small: Vec<Box<[u8; 100]>> = (0..1000).map(|_| Box::new([7; 100])).collect();
        let large = vec![1u8; 100_000];
        let huge = vec![2u8; 3 << 20];
        assert!(small.iter().all(|b| b[99] == 7));
        assert_eq!(large[99_999] + huge[(3 << 20) - 1], 3);
        for align in [64, 8192] {
            let layout = Layout::from_size_align(5000, align).unwrap();
            unsafe {
                let p = GLOBAL.alloc_zeroed(layout);
                assert_eq!(p as usize % align, 0);
                assert_eq!(*p.add(4999), 0);
                GLOBAL.dealloc(p, layout);
            }
        }
        LATE_DROPS.fetch_add(1, Ordering::SeqCst);
    }
}

thread_local! {
    static LATE: LateUser = const { LateUser };
}

/// Destructors of thread locals that are registered before the thread's
/// cache run after it is gone; their allocations go to the arenas directly
#[test]
fn allocations_during_thread_exit() {
    let before = LATE_DROPS.load(Ordering::SeqCst);
    thread::spawn(|| {
        LATE.with(|_| {});
        let v: Vec<u64> = (0..1000).collect();
        assert_eq!(v.iter().sum::<u64>(), 499_500);
    })
    .join()
    .unwrap();
    assert_eq!(LATE_DROPS.load(Ordering::SeqCst), before + 1);
}

/// Shrinking or growing within a size class keeps the pointer
#[test]
fn realloc_within_class_stays_in_place() {
    unsafe {
        let layout = Layout::from_size_align(100, 8).unwrap();
        let p = GLOBAL.alloc(layout);
        fill(p, 100, 9);
        // 100 and 104 bytes share the 104-byte class
        let q = GLOBAL.realloc(p, layout, 104);
        assert_eq!(p, q);
        check(q, 100, 9);
        let r = GLOBAL.realloc(q, Layout::from_size_align(104, 8).unwrap(), 2 << 20);
        check(r, 100, 9);
        let s = GLOBAL.realloc(
            r,
            Layout::from_size_align(2 << 20, 8).unwrap(),
            (2 << 20) - 4096,
        );
        assert_eq!(r, s, "huge allocations shrink in place");
        GLOBAL.dealloc(s, Layout::from_size_align((2 << 20) - 4096, 8).unwrap());
    }
}

/// Requests too large to satisfy return null rather than panic, in debug
/// builds too, where arithmetic overflow would panic inside the
/// allocator. A 32-bit process may well get 2 GiB, so only 64-bit targets
/// require null.
#[test]
fn oversized_requests_do_not_panic() {
    let max = isize::MAX as usize;
    let small = Layout::from_size_align(64, 8).unwrap();
    for size in [max, max - 4095, max / 2 + 1] {
        let layout = Layout::from_size_align(size, 1).unwrap();
        unsafe {
            let p = GLOBAL.alloc(layout);
            let z = GLOBAL.alloc_zeroed(layout);
            let q = GLOBAL.alloc(small);
            let r = GLOBAL.realloc(q, small, size);
            if cfg!(target_pointer_width = "64") {
                assert!(p.is_null() && z.is_null() && r.is_null(), "size {size}");
            }
            for ptr in [p, z, r] {
                if !ptr.is_null() {
                    GLOBAL.dealloc(ptr, layout);
                }
            }
            if r.is_null() {
                GLOBAL.dealloc(q, small);
            }
        }
    }
}
