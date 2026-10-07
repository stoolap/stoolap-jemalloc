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

//! Allocations above `LARGE_MAX` get mappings of their own. A header sits in
//! front of the data, at the CHUNK boundary that `head_of` finds; a few
//! freed mappings are kept to spare the page faults of mapping anew.

use crate::chunk::{CHUNK, ChunkHead, KIND_HUGE, head_of};
use crate::lock::SpinLock;
use crate::os;
use crate::prof::Sample;
use crate::stats;
use core::ptr::{self, null_mut};
use std::time::{Duration, Instant};

#[repr(C)]
pub struct HugeHead {
    pub head: ChunkHead,
    pub map_base: *mut u8,
    pub map_len: usize,
    /// Bytes usable from the returned pointer to the end of the mapping
    pub usable: usize,
    pub sample: *mut Sample,
}

const CACHE_SLOTS: usize = 8;
const CACHE_MAX_BYTES: usize = 64 << 20;
const CACHE_TTL: Duration = Duration::from_secs(10);

struct Cache {
    entries: [(*mut HugeHead, Option<Instant>); CACHE_SLOTS],
    len: usize,
    bytes: usize,
}

static CACHE: SpinLock<Cache> = SpinLock::new(Cache {
    entries: [(null_mut(), None); CACHE_SLOTS],
    len: 0,
    bytes: 0,
});

#[inline]
pub unsafe fn head(ptr: *mut u8) -> *mut HugeHead {
    head_of(ptr).cast()
}

pub unsafe fn alloc(size: usize, align: usize, zero: bool, sample: *mut Sample) -> *mut u8 {
    let ps = os::page_size();
    let offset = ps.max(align);
    let usable = os::round_up(size.max(1), ps);
    if offset == ps {
        let h = cache_take(usable);
        if !h.is_null() {
            (*h).sample = sample;
            (*h).head.sampled = u8::from(!sample.is_null());
            // Rebuilt from the exposed provenance: the last user of the
            // mapping may still hold a protected `Box` to it, which writes
            // through `map_base`'s own provenance would invalidate
            let p = core::ptr::with_exposed_provenance_mut::<u8>((*h).map_base.addr() + ps);
            if zero {
                ptr::write_bytes(p, 0, size);
            }
            return p;
        }
    }
    let len = offset + usable;
    let base = os::map_aligned(len, CHUNK.max(align));
    if base.is_null() {
        return null_mut();
    }
    base.expose_provenance();
    stats::HUGE_BYTES.add(len);
    let p = base.add(offset);
    let h = head(p);
    ptr::write(
        h,
        HugeHead {
            head: ChunkHead {
                kind: KIND_HUGE,
                sampled: u8::from(!sample.is_null()),
            },
            map_base: base,
            map_len: len,
            usable,
            sample,
        },
    );
    p
}

pub unsafe fn free(ptr: *mut u8) {
    let h = head(ptr);
    let len = (*h).map_len;
    if ptr.addr() - (*h).map_base.addr() == os::page_size() && len <= CACHE_MAX_BYTES / 2 {
        let mut evict = [null_mut(); CACHE_SLOTS + 1];
        let n;
        {
            let mut cache = CACHE.lock();
            let now = Instant::now();
            let i = cache.len;
            cache.entries[i] = (h, Some(now));
            cache.len += 1;
            cache.bytes += len;
            stats::HUGE_BYTES.sub(len);
            stats::HUGE_CACHED_BYTES.add(len);
            n = cache.evict(
                &mut evict,
                |c| c.len == CACHE_SLOTS || c.bytes > CACHE_MAX_BYTES,
                now,
            );
        }
        for &h in &evict[..n] {
            stats::HUGE_CACHED_BYTES.sub((*h).map_len);
            os::unmap((*h).map_base, (*h).map_len);
        }
        return;
    }
    stats::HUGE_BYTES.sub(len);
    os::unmap((*h).map_base, len);
}

impl Cache {
    /// Takes out entries past their time, then the oldest while `over`
    unsafe fn evict(
        &mut self,
        out: &mut [*mut HugeHead],
        over: impl Fn(&Cache) -> bool,
        now: Instant,
    ) -> usize {
        let mut n = 0;
        let mut i = 0;
        while i < self.len {
            let (h, t) = self.entries[i];
            if t.is_some_and(|t| now.duration_since(t) >= CACHE_TTL) {
                out[n] = h;
                n += 1;
                self.remove(i);
            } else {
                i += 1;
            }
        }
        while self.len > 0 && over(self) {
            out[n] = self.entries[0].0;
            n += 1;
            self.remove(0);
        }
        n
    }

    unsafe fn remove(&mut self, i: usize) {
        self.bytes -= (*self.entries[i].0).map_len;
        self.entries.copy_within(i + 1..self.len, i);
        self.len -= 1;
    }
}

/// The best cached mapping for `usable` bytes, wasting at most a quarter
unsafe fn cache_take(usable: usize) -> *mut HugeHead {
    let mut cache = CACHE.lock();
    let mut best = usize::MAX;
    for i in 0..cache.len {
        let u = (*cache.entries[i].0).usable;
        if u >= usable
            && u - usable <= usable / 4
            && (best == usize::MAX || u < (*cache.entries[best].0).usable)
        {
            best = i;
        }
    }
    if best == usize::MAX {
        return null_mut();
    }
    let h = cache.entries[best].0;
    cache.remove(best);
    stats::HUGE_CACHED_BYTES.sub((*h).map_len);
    stats::HUGE_BYTES.add((*h).map_len);
    h
}

/// Resizes in place when `size` still fits and wastes at most half
pub unsafe fn resize_in_place(ptr: *mut u8, size: usize) -> bool {
    let usable = (*head(ptr)).usable;
    size <= usable && size > usable / 2
}

#[cfg(all(unix, not(miri)))]
/// Holds the cache's lock across `fork`
pub(crate) fn fork_lock() {
    CACHE.acquire();
}

#[cfg(all(unix, not(miri)))]
pub(crate) unsafe fn fork_unlock() {
    CACHE.release();
}

/// Unmaps every cached mapping
pub unsafe fn purge() {
    release(true, Instant::now());
}

/// Unmaps the cached mappings that have stayed unused for their time;
/// runs with each decay epoch
pub unsafe fn decay() {
    release(false, Instant::now());
}

/// Unmaps the cached mappings older than `CACHE_TTL` at `now`, or all
unsafe fn release(all: bool, now: Instant) {
    let mut evict = [null_mut(); CACHE_SLOTS + 1];
    let n = CACHE.lock().evict(&mut evict, |_| all, now);
    for &h in &evict[..n] {
        stats::HUGE_CACHED_BYTES.sub((*h).map_len);
        os::unmap((*h).map_base, (*h).map_len);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The time limit holds without further huge frees
    #[test]
    fn cached_mappings_expire() {
        unsafe {
            let p = alloc(3 << 20, 8, false, null_mut());
            assert!(!p.is_null());
            free(p);
            let cached = |h: *mut HugeHead| {
                let cache = CACHE.lock();
                cache.entries[..cache.len].iter().any(|e| e.0 == h)
            };
            let h = head(p);
            assert!(cached(h));
            release(false, Instant::now());
            assert!(cached(h), "kept while fresh");
            release(false, Instant::now() + CACHE_TTL);
            assert!(!cached(h), "unmapped once its time is up");
        }
    }
}
