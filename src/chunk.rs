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

//! Chunks: CHUNK-aligned regions whose first bytes describe them. Any
//! pointer the allocator returns finds its chunk header by masking
//! `ptr - 1`, so no global lookup structure is needed. Arena chunks are
//! split into pages that form slabs and large runs; a huge allocation
//! has a mapping of its own with a small header in front.
//!
//! Pointers that come back from users carry only the provenance of their
//! allocation, so chunk mappings expose theirs when they are made and
//! `head_of` and `owned` derive pointers from it.

use crate::arena::Arena;
use crate::os;
use crate::prof::Sample;
use crate::size_class::{PAGE, PAGE_SHIFT};
use crate::stats;
use core::ptr::null_mut;

pub const CHUNK: usize = 1 << 22;
pub const PAGES: usize = CHUNK >> PAGE_SHIFT;
const WORDS: usize = PAGES / 64;

pub const KIND_ARENA: u8 = 1;
pub const KIND_HUGE: u8 = 2;

pub const RUN_FREE: u8 = 0;
pub const RUN_SLAB: u8 = 1;
pub const RUN_LARGE: u8 = 2;

/// Common to arena chunks and huge headers
#[repr(C)]
pub struct ChunkHead {
    pub kind: u8,
    /// Set when the memory belongs to sampled allocations
    pub sampled: u8,
}

#[inline(always)]
pub fn head_of(ptr: *mut u8) -> *mut ChunkHead {
    core::ptr::with_exposed_provenance_mut((ptr.addr() - 1) & !(CHUNK - 1))
}

/// `ptr`, from a user, as a pointer the allocator may use for the whole
/// mapping again. The user's own provenance is exposed too: its caller may
/// still hold a protected `Box` to the memory, and writes through the
/// result, such as free-list links, then act under that `Box`'s
/// permission instead of invalidating it.
#[inline(always)]
pub fn owned(ptr: *mut u8) -> *mut u8 {
    core::ptr::with_exposed_provenance_mut(ptr.expose_provenance())
}

/// Metadata of a page run, kept at the index of its first page
#[repr(C)]
pub struct Run {
    pub free_list: *mut u8,
    pub next: *mut Run,
    pub prev: *mut Run,
    pub sample: *mut Sample,
    pub npages: u16,
    pub nfree: u16,
    pub bump: u16,
    pub class: u8,
    pub state: u8,
}

pub type Bitmap = [u64; WORDS];

#[repr(C)]
pub struct ArenaChunk {
    pub head: ChunkHead,
    pub nfree: u32,
    pub arena: *const Arena,
    /// Next chunk of the arena, by address
    pub next: *mut ArenaChunk,
    /// Decay epoch in which the chunk became spare
    pub spare_epoch: u64,
    /// Free pages
    pub free: Bitmap,
    /// Free pages that hold memory freed in this decay epoch, and in the
    /// previous one; these are purged when the epoch ends
    pub dirty_new: Bitmap,
    pub dirty_old: Bitmap,
    /// First page of the run that holds each page
    pub run_start: [u16; PAGES],
    pub runs: [Run; PAGES],
}

pub const HEADER_PAGES: usize = size_of::<ArenaChunk>().div_ceil(PAGE);
pub const USABLE_PAGES: usize = PAGES - HEADER_PAGES;

/// Chunk operations take a raw pointer and touch only the fields they
/// need: threads holding different locks work on different runs of the
/// same chunk at once, so no reference may span the whole header. The
/// bitmaps, `nfree` and `next` belong to the arena's page lock; a slab's
/// `Run` to its bin lock; a large run's to its owner.
impl ArenaChunk {
    #[inline(always)]
    pub fn of(ptr: *mut u8) -> *mut ArenaChunk {
        head_of(ptr).cast()
    }

    #[inline(always)]
    pub fn page_of(c: *mut ArenaChunk, ptr: *mut u8) -> usize {
        (ptr.addr() - c.addr()) >> PAGE_SHIFT
    }

    #[inline(always)]
    pub unsafe fn page_addr(c: *mut ArenaChunk, page: usize) -> *mut u8 {
        c.cast::<u8>().add(page << PAGE_SHIFT)
    }

    /// The metadata slot of the run starting at page `p`
    #[inline(always)]
    pub unsafe fn run(c: *mut ArenaChunk, p: usize) -> *mut Run {
        (&raw mut (*c).runs).cast::<Run>().add(p)
    }

    #[inline(always)]
    pub unsafe fn run_page(c: *mut ArenaChunk, run: *const Run) -> usize {
        (run.addr() - Self::run(c, 0).addr()) / size_of::<Run>()
    }

    /// First page of the run that holds page `p`
    #[inline(always)]
    pub unsafe fn run_start(c: *mut ArenaChunk, p: usize) -> usize {
        *(&raw const (*c).run_start).cast::<u16>().add(p) as usize
    }

    #[inline(always)]
    pub unsafe fn set_run_start(c: *mut ArenaChunk, p: usize, start: usize) {
        *(&raw mut (*c).run_start).cast::<u16>().add(p) = start as u16;
    }

    /// The run that holds `ptr`, with its first page
    #[inline(always)]
    pub unsafe fn run_of(c: *mut ArenaChunk, ptr: *mut u8) -> (usize, *mut Run) {
        let p = Self::run_start(c, Self::page_of(c, ptr));
        (p, Self::run(c, p))
    }

    pub unsafe fn create(arena: *const Arena, sampled: bool) -> *mut ArenaChunk {
        let c: *mut ArenaChunk = crate::arena::map_or_make_room(CHUNK, CHUNK).cast();
        if c.is_null() {
            return c;
        }
        c.expose_provenance();
        os::no_huge_pages(c.cast(), CHUNK);
        stats::CHUNK_BYTES.add(CHUNK);
        // The mapping is zeroed, so only the non-zero fields need setting
        (*c).head.kind = KIND_ARENA;
        (*c).head.sampled = u8::from(sampled);
        (*c).arena = arena;
        (*c).next = null_mut();
        (*c).nfree = USABLE_PAGES as u32;
        set_range(&mut (*c).free, HEADER_PAGES, USABLE_PAGES);
        c
    }

    #[inline]
    pub unsafe fn is_free_page(c: *mut ArenaChunk, p: usize) -> bool {
        (*c).free[p / 64] & (1 << (p % 64)) != 0
    }

    /// Marks pages `p..p + n` as in use and points them at run `start`
    pub unsafe fn take(c: *mut ArenaChunk, start: usize, p: usize, n: usize) {
        clear_range(&mut (*c).free, p, n);
        clear_range(&mut (*c).dirty_new, p, n);
        clear_range(&mut (*c).dirty_old, p, n);
        (*c).nfree -= n as u32;
        for i in p..p + n {
            Self::set_run_start(c, i, start);
        }
    }

    /// Marks pages `p..p + n` as free and dirty
    pub unsafe fn give_back(c: *mut ArenaChunk, p: usize, n: usize) {
        set_range(&mut (*c).free, p, n);
        set_range(&mut (*c).dirty_new, p, n);
        (*c).nfree += n as u32;
    }

    pub unsafe fn is_empty(c: *mut ArenaChunk) -> bool {
        (*c).nfree as usize == USABLE_PAGES
    }

    /// Ends a decay epoch: purges the pages that stayed free since the
    /// previous one, or every dirty page when `all` is set
    pub unsafe fn decay(c: *mut ArenaChunk, all: bool) {
        let os_pages = (os::page_size() >> PAGE_SHIFT).max(1);
        let purged = decay_bits(
            &(*c).free,
            &mut (*c).dirty_old,
            &mut (*c).dirty_new,
            all,
            os_pages,
        );
        let mut i = 0;
        loop {
            let s = next_set(&purged, i);
            if s >= PAGES {
                break;
            }
            let e = next_clear(&purged, s);
            os::purge(Self::page_addr(c, s), (e - s) << PAGE_SHIFT);
            i = e;
        }
    }
}

/// Picks the pages to purge at the end of a decay epoch and ages the
/// dirty bits. The OS purges whole pages of `os_pages` allocator pages, so
/// an OS page is purged only when all of it is free and some of it is due.
/// Due pages that could not be purged stay due for the next epoch.
fn decay_bits(
    free: &Bitmap,
    old: &mut Bitmap,
    new: &mut Bitmap,
    all: bool,
    os_pages: usize,
) -> Bitmap {
    let mut purged: Bitmap = [0; WORDS];
    let mut p = 0;
    while p < PAGES {
        let n = os_pages.min(PAGES - p);
        let due = (p..p + n).any(|i| {
            let bit = 1 << (i % 64);
            old[i / 64] & bit != 0 || (all && new[i / 64] & bit != 0)
        });
        let all_free = (p..p + n).all(|i| free[i / 64] & (1 << (i % 64)) != 0);
        if due && all_free {
            set_range(&mut purged, p, n);
            clear_range(old, p, n);
            clear_range(new, p, n);
        }
        p += n;
    }
    for w in 0..WORDS {
        old[w] |= new[w];
        new[w] = 0;
    }
    purged
}

fn update_range(bm: &mut Bitmap, start: usize, n: usize, set: bool) {
    let end = start + n;
    let mut i = start;
    while i < end {
        let bit = i % 64;
        let cnt = (64 - bit).min(end - i);
        let mask = if cnt == 64 {
            !0
        } else {
            ((1u64 << cnt) - 1) << bit
        };
        if set {
            bm[i / 64] |= mask;
        } else {
            bm[i / 64] &= !mask;
        }
        i += cnt;
    }
}

fn set_range(bm: &mut Bitmap, start: usize, n: usize) {
    update_range(bm, start, n, true);
}

fn clear_range(bm: &mut Bitmap, start: usize, n: usize) {
    update_range(bm, start, n, false);
}

/// The first bit at or after `from` that is set (`want`) or clear, or PAGES
#[inline]
fn next_bit(bm: &Bitmap, from: usize, want: bool) -> usize {
    let mut w = from / 64;
    if w >= WORDS {
        return PAGES;
    }
    let flip = if want { 0 } else { !0 };
    let mut bits = (bm[w] ^ flip) & (!0u64 << (from % 64));
    loop {
        if bits != 0 {
            return w * 64 + bits.trailing_zeros() as usize;
        }
        w += 1;
        if w == WORDS {
            return PAGES;
        }
        bits = bm[w] ^ flip;
    }
}

fn next_set(bm: &Bitmap, from: usize) -> usize {
    next_bit(bm, from, true)
}

fn next_clear(bm: &Bitmap, from: usize) -> usize {
    next_bit(bm, from, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmap_runs() {
        let mut bm: Bitmap = [0; WORDS];
        set_range(&mut bm, 10, 100);
        assert_eq!(next_set(&bm, 0), 10);
        assert_eq!(next_clear(&bm, 10), 110);
        clear_range(&mut bm, 60, 10);
        assert_eq!(next_clear(&bm, 10), 60);
        assert_eq!(next_set(&bm, 60), 70);
        set_range(&mut bm, 0, PAGES);
        assert_eq!(next_clear(&bm, 0), PAGES);
    }

    /// Allocator pages freed at different times inside one OS page are
    /// purged once the whole OS page is free, and not lost before
    #[test]
    fn decay_waits_for_whole_os_pages() {
        let (mut free, mut old, mut new): (Bitmap, Bitmap, Bitmap) =
            ([0; WORDS], [0; WORDS], [0; WORDS]);
        let os_pages = 4;
        // Page 16 of the OS page 16..20 is freed; 17..20 are in use
        set_range(&mut free, 16, 1);
        set_range(&mut new, 16, 1);
        let purged = decay_bits(&free, &mut old, &mut new, false, os_pages);
        assert_eq!(next_set(&purged, 0), PAGES, "nothing is due yet");
        let purged = decay_bits(&free, &mut old, &mut new, false, os_pages);
        assert_eq!(next_set(&purged, 0), PAGES, "the OS page is not all free");
        assert_eq!(next_set(&old, 0), 16, "page 16 stays due");

        // Pages 17..20 are freed later; the OS page goes once they are due
        set_range(&mut free, 17, 3);
        set_range(&mut new, 17, 3);
        let purged = decay_bits(&free, &mut old, &mut new, false, os_pages);
        assert_eq!((next_set(&purged, 0), next_clear(&purged, 16)), (16, 20));
        assert_eq!(next_set(&old, 0), PAGES);
        assert_eq!(next_set(&new, 0), PAGES);

        // A purge of everything takes pages freed this epoch too
        set_range(&mut free, 64, 8);
        set_range(&mut new, 64, 8);
        let purged = decay_bits(&free, &mut old, &mut new, true, os_pages);
        assert_eq!((next_set(&purged, 0), next_clear(&purged, 64)), (64, 72));
    }

    #[test]
    fn header_fits() {
        const { assert!(HEADER_PAGES < 16) };
    }
}
