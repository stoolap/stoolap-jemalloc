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

//! The allocation paths. Frees use the size from the `Layout`, so the
//! fast paths find the class without reading any metadata.

use crate::arena::{self, run_addr};
use crate::chunk::{ArenaChunk, KIND_HUGE, head_of, owned};
use crate::huge;
use crate::os;
use crate::prof;
use crate::size_class::{
    CLASS_SIZE, LARGE_MAX, NCACHED, NSMALL, PAGE, TCACHE_MAX, class_for, class_of,
};
use crate::tcache;
use core::ptr::{self, null_mut};
use core::sync::atomic::Ordering::Relaxed;

#[inline(always)]
/// `profiling` is the allocator's setting to sample from the first
/// allocation on; it only matters on the slow path
pub unsafe fn alloc(size: usize, align: usize, profiling: bool) -> *mut u8 {
    if align <= 8 && size <= TCACHE_MAX {
        let class = class_of(size);
        let t = tcache::current();
        if !t.is_null() {
            let t = &mut *t;
            let left = t.until_event - i64::from(*CLASS_SIZE.get_unchecked(class));
            let bin = t.bins.get_unchecked_mut(class);
            if left >= 0 && bin.n != 0 {
                t.until_event = left;
                bin.n -= 1;
                if bin.n < bin.low_water {
                    bin.low_water = bin.n;
                }
                return *bin.slots.add(bin.n as usize);
            }
        }
    }
    alloc_slow(size, align, false, profiling)
}

#[inline(never)]
pub unsafe fn alloc_slow(size: usize, align: usize, zero: bool, profiling: bool) -> *mut u8 {
    // The process's first allocation comes this way, before any thread
    // cache exists, so its cache already samples
    if profiling && !prof::EVER.load(Relaxed) {
        prof::activate();
    }
    let class = if align <= PAGE {
        class_for(size, align)
    } else {
        None
    };
    let t = tcache::get_or_init();
    if !t.is_null() {
        let t = &mut *t;
        // Counted as the bytes it takes; capped far above any allocation
        // that can succeed, so that the counters cannot overflow
        let usable = class.map_or(size, |c| CLASS_SIZE[c] as usize);
        let bytes = (usable as u64).min(1 << 48) as i64;
        t.until_event -= bytes;
        if t.until_event < 0
            && let Some(interval) = tcache::event(t, bytes)
        {
            return prof::sample_alloc(size, align, zero, usable, interval);
        }
    }
    let arena = if t.is_null() {
        arena::fallback()
    } else {
        (*t).arena
    };
    let p = match class {
        Some(c) if c < NCACHED && !t.is_null() => tcache::alloc(&mut *t, c),
        _ if arena.is_null() => null_mut(),
        Some(c) if c < NSMALL => {
            let mut p = null_mut();
            (*arena).refill(c, &raw mut p, 1);
            p
        }
        Some(c) => (*arena).alloc_large(c),
        // Aligned beyond a page: a run with room to align in, if it fits
        None if align > PAGE && os::round_up(size.max(1), PAGE) + align - PAGE <= LARGE_MAX => {
            let run = arena::alloc_aligned(&*arena, size, align);
            if run.is_null() {
                null_mut()
            } else {
                run_addr(run).map_addr(|a| os::round_up(a, align))
            }
        }
        None => return huge::alloc(size, align, zero, null_mut()),
    };
    if zero && !p.is_null() {
        ptr::write_bytes(p, 0, size);
    }
    p
}

#[inline]
pub unsafe fn alloc_zeroed(size: usize, align: usize, profiling: bool) -> *mut u8 {
    if align <= 8 && size <= TCACHE_MAX {
        let p = alloc(size, align, profiling);
        if !p.is_null() {
            ptr::write_bytes(p, 0, size);
        }
        p
    } else {
        alloc_slow(size, align, true, profiling)
    }
}

#[inline(always)]
fn is_sampled(ptr: *mut u8) -> bool {
    prof::EVER.load(Relaxed) && unsafe { (*head_of(ptr)).sampled != 0 }
}

#[inline(always)]
pub unsafe fn dealloc(ptr: *mut u8, size: usize, align: usize) {
    let ptr = owned(ptr);
    if is_sampled(ptr) {
        return prof::sample_free(ptr);
    }
    if align <= 8 && size <= TCACHE_MAX {
        let class = class_of(size);
        let t = tcache::current();
        if !t.is_null() {
            let bin = (*t).bins.get_unchecked_mut(class);
            if bin.n < bin.cap {
                *bin.slots.add(bin.n as usize) = ptr;
                bin.n += 1;
                return;
            }
        }
    }
    dealloc_slow(ptr, size, align);
}

#[inline(never)]
unsafe fn dealloc_slow(ptr: *mut u8, size: usize, align: usize) {
    let class = if align <= PAGE {
        class_for(size, align)
    } else {
        None
    };
    match class {
        Some(c) => {
            // A thread may free before it allocates; its cache starts here
            let t = tcache::get_or_init();
            if c < NCACHED && !t.is_null() {
                tcache::free(&mut *t, c, ptr);
            } else {
                arena::free(ptr);
            }
        }
        None if (*head_of(ptr)).kind == KIND_HUGE => huge::free(ptr),
        None => arena::free(ptr),
    }
}

#[inline]
pub unsafe fn realloc(
    ptr: *mut u8,
    size: usize,
    align: usize,
    new_size: usize,
    profiling: bool,
) -> *mut u8 {
    let ptr = owned(ptr);
    // Cached classes move through the thread cache's fast paths
    if align <= 8 && size <= TCACHE_MAX && new_size <= TCACHE_MAX && !is_sampled(ptr) {
        if class_of(size) == class_of(new_size) {
            return ptr;
        }
        return realloc_move(ptr, size, align, new_size, profiling);
    }
    realloc_slow(ptr, size, align, new_size, profiling)
}

#[inline(never)]
unsafe fn realloc_slow(
    ptr: *mut u8,
    size: usize,
    align: usize,
    new_size: usize,
    profiling: bool,
) -> *mut u8 {
    if !is_sampled(ptr) && align <= PAGE {
        match (class_for(size, align), class_for(new_size, align)) {
            (Some(a), Some(b)) => {
                if a == b {
                    return ptr;
                }
                if a >= NSMALL && b >= NSMALL {
                    let c = ArenaChunk::of(ptr);
                    if (*(*c).arena).resize_large(c, ptr, b) {
                        return ptr;
                    }
                }
            }
            (None, None) if huge::resize_in_place(ptr, new_size) => {
                return ptr;
            }
            _ => {}
        }
    }
    realloc_move(ptr, size, align, new_size, profiling)
}

#[inline(always)]
unsafe fn realloc_move(
    ptr: *mut u8,
    size: usize,
    align: usize,
    new_size: usize,
    profiling: bool,
) -> *mut u8 {
    let new = alloc(new_size, align, profiling);
    if !new.is_null() {
        ptr::copy_nonoverlapping(ptr, new, size.min(new_size));
        dealloc(ptr, size, align);
    }
    new
}
