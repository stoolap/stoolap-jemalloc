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

//! Process-wide counters, updated only on slow paths

use core::sync::atomic::{AtomicUsize, Ordering::Relaxed};

static CHUNKS: AtomicUsize = AtomicUsize::new(0);
static HUGE: AtomicUsize = AtomicUsize::new(0);
static HUGE_CACHED: AtomicUsize = AtomicUsize::new(0);
static METADATA: AtomicUsize = AtomicUsize::new(0);

/// Memory the allocator holds from the OS, in bytes
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Chunks that hold small and large allocations
    pub chunks: usize,
    /// Mappings of huge allocations that are in use
    pub huge: usize,
    /// Freed huge mappings kept for reuse
    pub huge_cached: usize,
    /// The allocator's own metadata, including the heap profiler's
    pub metadata: usize,
}

impl Stats {
    /// All memory mapped from the OS
    #[must_use]
    pub fn mapped(&self) -> usize {
        self.chunks + self.huge + self.huge_cached + self.metadata
    }
}

/// A snapshot of the allocator's counters
pub fn stats() -> Stats {
    Stats {
        chunks: CHUNKS.load(Relaxed),
        huge: HUGE.load(Relaxed),
        huge_cached: HUGE_CACHED.load(Relaxed),
        metadata: METADATA.load(Relaxed),
    }
}

pub(crate) fn add_chunks(n: isize) {
    CHUNKS.fetch_add(n as usize, Relaxed);
}

pub(crate) fn add_huge(n: isize) {
    HUGE.fetch_add(n as usize, Relaxed);
}

pub(crate) fn add_huge_cached(n: isize) {
    HUGE_CACHED.fetch_add(n as usize, Relaxed);
}

pub(crate) fn add_metadata(n: isize) {
    METADATA.fetch_add(n as usize, Relaxed);
}
