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

//! An allocator that wraps this one and reclaims memory right after it
//! forwards a free, while the caller's `Box` is still protected. Run it
//! under Miri like `tests/miri.rs`.

use std::alloc::{GlobalAlloc, Layout};
use std::hint::black_box;
use std::sync::atomic::{AtomicBool, Ordering};
use stoolap_jemalloc::Jemalloc;

struct Reclaiming;

static RECLAIM: AtomicBool = AtomicBool::new(false);

#[global_allocator]
static GLOBAL: Reclaiming = Reclaiming;

unsafe impl GlobalAlloc for Reclaiming {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { Jemalloc.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { Jemalloc.dealloc(ptr, layout) };
        if RECLAIM.load(Ordering::Relaxed) {
            if layout.size() > 1 << 20 {
                // Takes the huge mapping just freed back out of the cache
                // and zeroes it
                unsafe {
                    let again = Jemalloc.alloc_zeroed(layout);
                    assert!(!again.is_null());
                    Jemalloc.dealloc(again, layout);
                }
            } else {
                // Writes free-list links into the object just freed
                stoolap_jemalloc::purge();
            }
        }
    }
}

#[test]
fn reclaim_right_after_free() {
    for _ in 0..3 {
        let small = black_box(Box::new([7u64; 4]));
        let tiny = black_box(Box::new(1u8));
        let huge = black_box(vec![3u8; (1 << 20) + 4096].into_boxed_slice());
        RECLAIM.store(true, Ordering::Relaxed);
        drop(small);
        drop(tiny);
        drop(huge);
        RECLAIM.store(false, Ordering::Relaxed);
    }
}
