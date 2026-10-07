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

//! Thread caches: a stack of free objects per class, so that most
//! allocations and frees touch no lock and no shared memory. Full caches
//! flush their oldest half to the arenas; a garbage collector returns
//! what a thread stopped using.

use crate::arena::{self, Arena};
use crate::base::Pool;
use crate::prof;
use crate::size_class::{CACHE_CAP, NCACHED, NSMALL, TOTAL_SLOTS};
use core::cell::Cell;
use core::ptr::{self, null_mut};
use core::sync::atomic::Ordering;

/// Allocated bytes between garbage collection steps; each step visits
/// one class
const GC_INTERVAL: i64 = 64 << 10;
/// Allocated bytes between checks for activated profiling
const PROF_CHECK_INTERVAL: i64 = 1 << 20;

#[repr(C)]
pub struct CacheBin {
    pub slots: *mut *mut u8,
    pub n: u32,
    pub cap: u32,
    /// Fewest objects the bin held since the last collection
    pub low_water: u32,
}

#[repr(C)]
pub struct TCache {
    /// Bytes until the next event: a sample or a collection step
    pub until_event: i64,
    pub bins: [CacheBin; NCACHED],
    pub arena: *mut Arena,
    event_start: i64,
    sample_wait: i64,
    gc_wait: i64,
    /// Whether `sample_wait` was drawn while profiling was active, and the
    /// sample interval it was drawn with
    sampling: bool,
    interval: usize,
    gc_bin: usize,
    rng: u64,
}

/// A cache and the slots its bins point into. The slots sit outside the
/// `TCache`, so that a `&mut TCache` never covers memory reached through
/// the bins' slot pointers.
#[repr(C)]
struct Block {
    cache: TCache,
    slots: [*mut u8; TOTAL_SLOTS],
}

static POOL: Pool<Block> = Pool::new();

// Addresses up to DEAD are states, not caches
const INITIALIZING: usize = 1;
const DEAD: usize = 2;

fn state(s: usize) -> *mut TCache {
    ptr::without_provenance_mut(s)
}

thread_local! {
    static CURRENT: Cell<*mut TCache> = const { Cell::new(null_mut()) };
    static GUARD: Guard = const { Guard };
}

/// Flushes the thread's cache when the thread exits
struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        let t = CURRENT.with(|c| c.replace(state(DEAD)));
        if t.addr() > DEAD {
            unsafe {
                flush_all(&mut *t);
                arena::leave((*t).arena);
                // The cache is the block's first field
                POOL.free(t.cast::<Block>());
            }
        }
    }
}

#[cfg(all(unix, not(miri)))]
/// Holds the cache pool's lock across `fork`
pub(crate) fn fork_lock() {
    POOL.fork_lock();
}

#[cfg(all(unix, not(miri)))]
pub(crate) unsafe fn fork_unlock() {
    POOL.fork_unlock();
}

/// The thread's cache, or null before it exists or after the thread
/// started exiting
#[inline(always)]
pub fn current() -> *mut TCache {
    let t = CURRENT.with(Cell::get);
    if t.addr() > DEAD { t } else { null_mut() }
}

/// The thread's cache, created on first use; null while it is being
/// created (allocations that registering the destructor makes) and once
/// the thread is exiting
pub unsafe fn get_or_init() -> *mut TCache {
    let t = CURRENT.with(Cell::get);
    if t.addr() > DEAD {
        return t;
    }
    if !t.is_null() {
        return null_mut();
    }
    CURRENT.with(|c| c.set(state(INITIALIZING)));
    if GUARD.try_with(|_| ()).is_err() {
        CURRENT.with(|c| c.set(state(DEAD)));
        return null_mut();
    }
    let block = POOL.alloc();
    let arena = if block.is_null() {
        null_mut()
    } else {
        arena::choose()
    };
    if arena.is_null() {
        if !block.is_null() {
            POOL.free(block);
        }
        CURRENT.with(|c| c.set(null_mut()));
        return null_mut();
    }
    let t = init(block, arena);
    CURRENT.with(|c| c.set(t));
    #[cfg(all(unix, not(miri)))]
    crate::fork::register();
    t
}

unsafe fn init(block: *mut Block, arena: *mut Arena) -> *mut TCache {
    let t = &raw mut (*block).cache;
    let slots = (&raw mut (*block).slots).cast::<*mut u8>();
    let mut offset = 0;
    for c in 0..NCACHED {
        let cap = u32::from(CACHE_CAP[c]);
        ptr::write(
            ptr::addr_of_mut!((*t).bins[c]),
            CacheBin {
                slots: slots.add(offset),
                n: 0,
                cap,
                low_water: 0,
            },
        );
        offset += cap as usize;
    }
    (*t).arena = arena;
    (*t).gc_wait = GC_INTERVAL;
    (*t).gc_bin = 0;
    (*t).rng = prof::new_seed();
    // With profiling active, sampling starts with the first allocation
    let active = prof::ACTIVE.load(Ordering::Relaxed);
    let interval = prof::sample_interval();
    (*t).sampling = active;
    (*t).interval = interval;
    (*t).sample_wait = if active {
        prof::next_wait(&mut (*t).rng, interval)
    } else {
        PROF_CHECK_INTERVAL
    };
    let next = (*t).sample_wait.min(GC_INTERVAL);
    (*t).until_event = next;
    (*t).event_start = next;
    t
}

/// Runs the events that are due once the current allocation of `bytes`
/// is counted. Returns the interval to weigh a sample of it with, when it
/// is to be sampled.
#[cold]
pub unsafe fn event(t: &mut TCache, bytes: i64) -> Option<usize> {
    let elapsed = t.event_start - t.until_event;
    t.sample_wait -= elapsed;
    t.gc_wait -= elapsed;
    let active = prof::ACTIVE.load(Ordering::Relaxed);
    let interval = prof::sample_interval();
    let mut sample = None;
    if t.sample_wait <= 0 {
        if !active {
            t.sampling = false;
            t.sample_wait = PROF_CHECK_INTERVAL;
        } else if t.sampling {
            // The wait that ran out was drawn with `t.interval`, which sets
            // this allocation's chance to be sampled
            sample = Some(t.interval);
            t.interval = interval;
            t.sample_wait = prof::next_wait(&mut t.rng, interval);
        } else {
            // Profiling was activated since the last check: sampling starts
            // with this allocation, which is sampled with the probability
            // that a wait drawn at its start ends within it
            t.sampling = true;
            t.interval = interval;
            let wait = prof::next_wait(&mut t.rng, interval);
            if wait <= bytes {
                sample = Some(interval);
                t.sample_wait = prof::next_wait(&mut t.rng, interval);
            } else {
                t.sample_wait = wait - bytes;
            }
        }
    } else if active && t.sampling && t.interval != interval {
        // The interval changed: waits are memoryless, so the rest of the
        // current one is drawn again with the new interval
        t.interval = interval;
        t.sample_wait = prof::next_wait(&mut t.rng, interval);
    }
    if t.gc_wait <= 0 {
        gc_step(t);
        t.gc_wait = GC_INTERVAL;
    }
    let next = t.sample_wait.min(t.gc_wait);
    t.until_event = next;
    t.event_start = next;
    sample
}

/// Flushes three quarters of what one class did not use since its last
/// step; ends a decay epoch if one is due
unsafe fn gc_step(t: &mut TCache) {
    let c = t.gc_bin;
    t.gc_bin = (c + 1) % NCACHED;
    let low = t.bins[c].low_water;
    if low > 0 {
        flush(t, c, (low - low / 4) as usize);
    }
    t.bins[c].low_water = t.bins[c].n;
    // Once per round of the classes, so the clock is read rarely
    if t.gc_bin == 0 {
        arena::decay_tick();
    }
}

/// An object of a cached class, from the cache or refilled into it
pub unsafe fn alloc(t: &mut TCache, class: usize) -> *mut u8 {
    let bin = &mut t.bins[class];
    if bin.n == 0 {
        if class >= NSMALL {
            return (*t.arena).alloc_large(class);
        }
        let want = (bin.cap / 2).max(1) as usize;
        let n = (*t.arena).refill(class, bin.slots, want);
        if n == 0 {
            return null_mut();
        }
        // Hand out the lowest addresses first
        core::slice::from_raw_parts_mut(bin.slots, n).reverse();
        bin.n = n as u32;
    }
    bin.n -= 1;
    bin.low_water = bin.low_water.min(bin.n);
    *bin.slots.add(bin.n as usize)
}

/// Caches a freed object, flushing half the bin when it is full
pub unsafe fn free(t: &mut TCache, class: usize, ptr: *mut u8) {
    let bin = &t.bins[class];
    if bin.n == bin.cap {
        flush(t, class, (bin.cap / 2).max(1) as usize);
    }
    let bin = &mut t.bins[class];
    *bin.slots.add(bin.n as usize) = ptr;
    bin.n += 1;
}

/// Returns the `count` oldest objects of a class to their arenas
unsafe fn flush(t: &mut TCache, class: usize, count: usize) {
    let bin = &mut t.bins[class];
    let count = count.min(bin.n as usize);
    if count == 0 {
        return;
    }
    let objs = core::slice::from_raw_parts_mut(bin.slots, count);
    if class < NSMALL {
        arena::free_small_batch(objs, class);
    } else {
        for &p in objs.iter() {
            arena::free(p);
        }
    }
    ptr::copy(bin.slots.add(count), bin.slots, bin.n as usize - count);
    bin.n -= count as u32;
    bin.low_water = bin.low_water.min(bin.n);
}

pub unsafe fn flush_all(t: &mut TCache) {
    for c in 0..NCACHED {
        flush(t, c, t.bins[c].n as usize);
    }
}
