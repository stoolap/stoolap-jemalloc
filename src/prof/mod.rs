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

//! Sampling heap profiler.
//!
//! As in jemalloc, a thread takes a sample after a random number of
//! allocated bytes, exponentially distributed around the sample interval,
//! so each allocation is sampled with probability `1 - exp(-size /
//! interval)`. A sample records the allocation's stack; counts per stack
//! are scaled by the inverse of that probability, which makes them
//! unbiased estimates of the real totals.
//!
//! Sampled allocations live in chunks of their own, so a free only needs
//! to look at the chunk header to know whether it frees a sample.

mod backtrace;
mod maps;
mod pprof;

use crate::arena::{self, run_addr};
use crate::base::{self, Pool};
use crate::chunk::{KIND_HUGE, head_of};
use crate::huge;
use crate::lock::SpinLock;
use crate::os;
use crate::size_class::{LARGE_MAX, PAGE, PAGE_SHIFT};
use core::ptr::{self, null_mut};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

pub use pprof::DumpError;

/// Set once profiling was ever activated; frees check for samples from then on
pub(crate) static EVER: AtomicBool = AtomicBool::new(false);
pub(crate) static ACTIVE: AtomicBool = AtomicBool::new(false);
static INTERVAL: AtomicUsize = AtomicUsize::new(DEFAULT_INTERVAL);

/// 512 KiB, jemalloc's default (`lg_prof_sample:19`)
pub const DEFAULT_INTERVAL: usize = 1 << 19;
const MAX_FRAMES: usize = 128;

/// Starts sampling allocations
pub fn activate() {
    EVER.store(true, Ordering::SeqCst);
    ACTIVE.store(true, Ordering::SeqCst);
}

/// Stops taking new samples; live samples stay in the profile
pub fn deactivate() {
    ACTIVE.store(false, Ordering::SeqCst);
}

/// Whether allocations are being sampled
pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Sets the mean number of bytes between samples, from 1 byte to 128 TiB;
/// values outside are clamped
pub fn set_sample_interval(bytes: usize) {
    INTERVAL.store(bytes.clamp(1, MAX_INTERVAL), Ordering::Relaxed);
}

/// The largest interval: a sample's weight in bytes, about the interval in
/// fixed point, then still fits in a `u64`
const MAX_INTERVAL: usize = {
    let max = 1u64 << 47;
    if max < usize::MAX as u64 {
        max as usize
    } else {
        usize::MAX
    }
};

/// The mean number of bytes between samples
pub fn sample_interval() -> usize {
    INTERVAL.load(Ordering::Relaxed)
}

/// Weights are fixed point with this many fractional bits, so that
/// fractional estimates add up exactly and are rounded only in a dump
const WEIGHT_SHIFT: u32 = 16;
/// Waits never exceed this, so that the event counters cannot overflow
pub(crate) const MAX_WAIT: i64 = 1 << 62;

/// A thread's random number seed: each thread gets its own, also when it
/// reuses the cache memory of a thread that exited
pub(crate) fn new_seed() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed)
        ^ (u64::from(std::process::id()) << 32);
    // splitmix64's finalizer; xorshift needs a non-zero state
    let mut z = n;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    (z ^ (z >> 31)) | 1
}

/// Bytes until the next sample: exponentially distributed with `interval`
/// as mean
pub(crate) fn next_wait(rng: &mut u64, interval: usize) -> i64 {
    // xorshift64*
    *rng ^= *rng >> 12;
    *rng ^= *rng << 25;
    *rng ^= *rng >> 27;
    let r = rng.wrapping_mul(0x2545_f491_4f6c_dd1d);
    let u = ((r >> 11) + 1) as f64 / (1u64 << 53) as f64;
    let wait = -u.ln() * interval as f64;
    (wait as i64).clamp(1, MAX_WAIT)
}

/// A live sampled allocation, with the fixed point weights it added
pub struct Sample {
    stack: *mut Stack,
    objects: u64,
    bytes: u64,
}

static SAMPLES: Pool<Sample> = Pool::new();

/// A unique stack with its counts: estimates in fixed point
#[repr(C)]
struct Stack {
    next: *mut Stack,
    hash: u64,
    depth: usize,
    alloc_objects: u128,
    alloc_bytes: u128,
    inuse_objects: u128,
    inuse_bytes: u128,
    // `depth` frames follow
}

impl Stack {
    /// The frames stored after the stack; they never change
    unsafe fn frames<'a>(s: *const Stack) -> &'a [usize] {
        core::slice::from_raw_parts(s.add(1).cast::<usize>(), (*s).depth)
    }
}

/// Stacks by hash, in an open addressing table that grows by doubling.
/// Stacks never move or go away, so a dump can read their frames
/// without the lock.
struct Stacks {
    table: *mut *mut Stack,
    cap: usize,
    len: usize,
    all: *mut Stack,
}

/// Held through a dump, and across `fork` before every other lock: the
/// symbolizer's own lock, which a dump takes, is then free in the child
static DUMP: SpinLock<()> = SpinLock::new(());

static STACKS: SpinLock<Stacks> = SpinLock::new(Stacks {
    table: null_mut(),
    cap: 0,
    len: 0,
    all: null_mut(),
});

impl Stacks {
    unsafe fn find_or_insert(&mut self, hash: u64, frames: &[usize]) -> *mut Stack {
        if (self.len + 1) * 2 > self.cap && !self.grow() {
            return null_mut();
        }
        let mask = self.cap - 1;
        let mut i = hash as usize & mask;
        loop {
            let s = *self.table.add(i);
            if s.is_null() {
                break;
            }
            if (*s).hash == hash && Stack::frames(s) == frames {
                return s;
            }
            i = (i + 1) & mask;
        }
        let bytes = size_of::<Stack>() + std::mem::size_of_val(frames);
        let s: *mut Stack = base::alloc(bytes, align_of::<Stack>()).cast();
        if s.is_null() {
            return s;
        }
        ptr::write(
            s,
            Stack {
                next: self.all,
                hash,
                depth: frames.len(),
                alloc_objects: 0,
                alloc_bytes: 0,
                inuse_objects: 0,
                inuse_bytes: 0,
            },
        );
        ptr::copy_nonoverlapping(frames.as_ptr(), s.add(1).cast::<usize>(), frames.len());
        self.all = s;
        *self.table.add(i) = s;
        self.len += 1;
        s
    }

    unsafe fn grow(&mut self) -> bool {
        let cap = (self.cap * 2).max(1024);
        let bytes = os::round_up(cap * size_of::<usize>(), os::page_size());
        let table: *mut *mut Stack = os::map(bytes).cast();
        if table.is_null() {
            return false;
        }
        let mut s = self.all;
        while !s.is_null() {
            let mut i = (*s).hash as usize & (cap - 1);
            while !(*table.add(i)).is_null() {
                i = (i + 1) & (cap - 1);
            }
            *table.add(i) = s;
            s = (*s).next;
        }
        if !self.table.is_null() {
            let old = os::round_up(self.cap * size_of::<usize>(), os::page_size());
            os::unmap(self.table.cast(), old);
            crate::stats::METADATA_BYTES.sub(old);
        }
        crate::stats::METADATA_BYTES.add(bytes);
        self.table = table;
        self.cap = cap;
        true
    }
}

#[cfg(all(unix, not(miri)))]
/// Waits for a dump to end and holds off the next until after `fork`
pub(crate) fn fork_lock_dump() {
    DUMP.acquire();
}

#[cfg(all(unix, not(miri)))]
pub(crate) unsafe fn fork_unlock_dump() {
    DUMP.release();
}

#[cfg(all(unix, not(miri)))]
/// Holds the profiler's locks across `fork`, stacks before samples
pub(crate) fn fork_lock() {
    STACKS.acquire();
    SAMPLES.fork_lock();
}

#[cfg(all(unix, not(miri)))]
pub(crate) unsafe fn fork_unlock() {
    SAMPLES.fork_unlock();
    STACKS.release();
}

fn hash_frames(frames: &[usize]) -> u64 {
    let mut h: u64 = 0;
    for &f in frames {
        h = (h.rotate_left(5) ^ f as u64).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
    h
}

/// Allocates and records a sample; called when a thread's sampling
/// counter runs out. `usable` is what the allocation took off the counter
/// and `interval` the mean of the wait that ran out, which together set
/// the chance it had to be sampled.
#[inline(never)]
pub(crate) unsafe fn sample_alloc(
    size: usize,
    align: usize,
    zero: bool,
    usable: usize,
    interval: usize,
) -> *mut u8 {
    // A free of this allocation on any thread is ordered after this store,
    // so it sees that frees must check for samples
    EVER.store(true, Ordering::Relaxed);
    let mut frames = [0usize; MAX_FRAMES];
    // Leave out this function
    let depth = backtrace::capture(&mut frames, 1);
    let frames = &frames[..depth];

    let sample = SAMPLES.alloc();
    if sample.is_null() {
        return null_mut();
    }
    let ptr = alloc_sampled(size, align, zero, sample);
    if ptr.is_null() {
        SAMPLES.free(sample);
        return ptr;
    }

    // Each sample stands for 1 / P(sampled) allocations of its size
    let usable = usable.max(1) as f64;
    // 1 - e^-x, exact also when x is tiny
    let scale = 1.0 / -(-usable / interval as f64).exp_m1();
    let one = f64::from(1u32 << WEIGHT_SHIFT);
    let objects = (scale * one).round() as u64;
    let bytes = (usable * scale * one).round() as u64;
    let hash = hash_frames(frames);
    let stack = {
        let mut stacks = STACKS.lock();
        let s = stacks.find_or_insert(hash, frames);
        if !s.is_null() {
            (*s).alloc_objects += u128::from(objects);
            (*s).alloc_bytes += u128::from(bytes);
            (*s).inuse_objects += u128::from(objects);
            (*s).inuse_bytes += u128::from(bytes);
        }
        s
    };
    ptr::write(
        sample,
        Sample {
            stack,
            objects,
            bytes,
        },
    );
    ptr
}

/// Memory for a sample: a page run in the sampled arena, or a huge mapping
unsafe fn alloc_sampled(size: usize, align: usize, zero: bool, sample: *mut Sample) -> *mut u8 {
    let align = align.max(1);
    let pages = (os::round_up(size.max(1), PAGE) + align.max(PAGE) - PAGE) >> PAGE_SHIFT;
    if pages << PAGE_SHIFT > LARGE_MAX {
        return huge::alloc(size, align, zero, sample);
    }
    let arena = arena::sampled();
    if arena.is_null() {
        return null_mut();
    }
    let run = (*arena).alloc_pages(pages, u8::MAX);
    if run.is_null() {
        return null_mut();
    }
    (*run).sample = sample;
    let p = run_addr(run).map_addr(|a| os::round_up(a, align));
    if zero {
        ptr::write_bytes(p, 0, size);
    }
    p
}

/// Frees a sampled allocation and takes it out of the live counts
pub(crate) unsafe fn sample_free(ptr: *mut u8) {
    let huge = (*head_of(ptr)).kind == KIND_HUGE;
    let sample = if huge {
        (*huge::head(ptr)).sample
    } else {
        (*arena::sampled_run(ptr)).sample
    };
    let s = &*sample;
    if !s.stack.is_null() {
        let _stacks = STACKS.lock();
        (*s.stack).inuse_objects -= u128::from(s.objects);
        (*s.stack).inuse_bytes -= u128::from(s.bytes);
    }
    SAMPLES.free(sample);
    if huge {
        huge::free(ptr);
    } else {
        arena::free_sampled_run(ptr);
    }
}

/// Counts of one stack, copied out for a dump
struct StackCounts {
    frames: &'static [usize],
    alloc_objects: u64,
    alloc_bytes: u64,
    inuse_objects: u64,
    inuse_bytes: u64,
}

/// Copies every stack's counts without allocating under the lock
fn snapshot() -> Vec<StackCounts> {
    let mut out = Vec::new();
    loop {
        let need = STACKS.lock().len;
        out.reserve(need + 64);
        let stacks = STACKS.lock();
        if stacks.len > out.capacity() {
            continue;
        }
        let round = |w: u128| ((w + (1 << (WEIGHT_SHIFT - 1))) >> WEIGHT_SHIFT) as u64;
        let mut s = stacks.all;
        while !s.is_null() {
            unsafe {
                out.push(StackCounts {
                    frames: Stack::frames(s),
                    alloc_objects: round((*s).alloc_objects),
                    alloc_bytes: round((*s).alloc_bytes),
                    inuse_objects: round((*s).inuse_objects),
                    inuse_bytes: round((*s).inuse_bytes),
                });
                s = (*s).next;
            }
        }
        return out;
    }
}

/// The heap profile in pprof's protobuf format, uncompressed (`pprof`
/// reads both plain and gzipped files). Its sample types follow Go's heap
/// profiles: `alloc_objects`, `alloc_space`, `inuse_objects` and
/// `inuse_space`, the default.
///
/// # Errors
///
/// [`DumpError::NotActivated`] when profiling was never activated.
pub fn dump_pprof() -> Result<Vec<u8>, DumpError> {
    if !EVER.load(Ordering::Relaxed) {
        return Err(DumpError::NotActivated);
    }
    let _dump = DUMP.lock();
    let profile = pprof::encode(&snapshot(), sample_interval());
    // The symbolizer keeps the debug information it parsed, tens of
    // megabytes for a large library
    #[cfg(feature = "symbolize")]
    ::backtrace::clear_symbol_cache();
    Ok(profile)
}

/// Writes the heap profile to `path`; returns its size in bytes
///
/// # Errors
///
/// [`DumpError::NotActivated`] when profiling was never activated, and
/// [`DumpError::Io`] when the file cannot be written.
pub fn write_pprof(path: impl AsRef<std::path::Path>) -> Result<usize, DumpError> {
    let profile = dump_pprof()?;
    std::fs::write(path, &profile).map_err(DumpError::Io)?;
    Ok(profile.len())
}
