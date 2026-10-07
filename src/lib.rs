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

//! A jemalloc-style memory allocator in pure Rust, with a sampling heap
//! profiler that writes pprof profiles.
//!
//! ```no_run
//! use stoolap_jemalloc::Jemalloc;
//!
//! #[global_allocator]
//! static GLOBAL: Jemalloc = Jemalloc;
//!
//! fn main() {
//!     stoolap_jemalloc::prof::activate();
//!     // ... work ...
//!     stoolap_jemalloc::prof::write_pprof("heap.pb").unwrap();
//! }
//! ```
//!
//! Then `go tool pprof -http=: heap.pb`.
//!
//! Design, after jemalloc:
//! - size classes with 8-byte steps up to 128 bytes, then four per doubling
//! - a thread cache per thread, flushed in batches and trimmed over time
//! - 4 arenas per CPU; small classes come from slabs, larger ones from page
//!   runs inside 4 MiB chunks, and those above 1 MiB from their own mappings
//! - freed pages go back to the OS after 5 to 10 seconds
//! - heap profiling by sampling, on average once per 512 KiB allocated

mod arena;
mod base;
mod chunk;
mod huge;
mod lock;
mod malloc;
mod os;
pub mod prof;
mod size_class;
mod stats;
mod tcache;

pub use stats::{Stats, stats};

use core::alloc::{GlobalAlloc, Layout};

/// The allocator; install it with `#[global_allocator]`
#[derive(Debug, Clone, Copy, Default)]
pub struct Jemalloc;

unsafe impl GlobalAlloc for Jemalloc {
    #[inline(always)]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        malloc::alloc(layout.size(), layout.align())
    }

    #[inline(always)]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        malloc::dealloc(ptr, layout.size(), layout.align());
    }

    #[inline(always)]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        malloc::alloc_zeroed(layout.size(), layout.align())
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        malloc::realloc(ptr, layout.size(), layout.align(), new_size)
    }
}

/// Returns the calling thread's cached objects, then every free page and
/// cached huge mapping, to the OS
pub fn purge() {
    unsafe {
        let t = tcache::current();
        if !t.is_null() {
            tcache::flush_all(&mut *t);
        }
        arena::purge();
        huge::purge();
    }
}
