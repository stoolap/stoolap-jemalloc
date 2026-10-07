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
//! static GLOBAL: Jemalloc = Jemalloc::new();
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
pub mod background;
mod base;
mod chunk;
#[cfg(all(unix, not(miri)))]
mod fork;
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

/// The allocator; install it with `#[global_allocator]`:
///
/// ```no_run
/// use stoolap_jemalloc::Jemalloc;
///
/// #[global_allocator]
/// static GLOBAL: Jemalloc = Jemalloc::new();
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct Jemalloc {
    profiling: bool,
}

impl Jemalloc {
    /// The allocator, with heap profiling off until `prof::activate()`
    #[must_use]
    pub const fn new() -> Self {
        Jemalloc { profiling: false }
    }

    /// The allocator with heap profiling on from the process's first
    /// allocation, as jemalloc's `prof:true,prof_active:true` options
    /// set it, without a call to `prof::activate()`.
    /// `prof::deactivate()` still stops it.
    #[must_use]
    pub const fn with_profiling(self) -> Self {
        Jemalloc { profiling: true }
    }
}

// Calls, not inlined: inlined, the allocator's paths grew the loops that
// allocate or drop, and a full table scan in Stoolap ran 20% slower even
// though it allocated only 32 times
unsafe impl GlobalAlloc for Jemalloc {
    #[inline(never)]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        malloc::alloc(layout.size(), layout.align(), self.profiling)
    }

    #[inline(never)]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        malloc::dealloc(ptr, layout.size(), layout.align());
    }

    #[inline(never)]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        malloc::alloc_zeroed(layout.size(), layout.align(), self.profiling)
    }

    #[inline(never)]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        malloc::realloc(ptr, layout.size(), layout.align(), new_size, self.profiling)
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
