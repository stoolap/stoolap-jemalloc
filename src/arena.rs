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

//! Arenas own chunks. Each has a page lock for runs and a lock per small
//! class for its slabs. A new thread takes the arena with the fewest
//! threads, out of up to 4 per CPU.

use crate::base;
use crate::chunk::{
    ArenaChunk, CHUNK, HEADER_PAGES, PAGES, RUN_FREE, RUN_LARGE, RUN_SLAB, Run, USABLE_PAGES,
};
use crate::lock::SpinLock;
use crate::os;
use crate::size_class::{CLASS_SIZE, LARGE_MAX, NSMALL, PAGE, PAGE_SHIFT, SLAB_OBJS, SLAB_PAGES};
use crate::stats;
use core::ptr::{self, null_mut};
use core::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Freed pages go back to the OS after one to two epochs, and spare
/// chunks are unmapped after as long
pub(crate) const DECAY_EPOCH: Duration = Duration::from_secs(5);
const MAX_ARENAS: usize = 256;

pub struct Arena {
    sampled: bool,
    /// Threads whose cache uses this arena
    threads: AtomicUsize,
    pages: SpinLock<Pages>,
    bins: [Bin; NSMALL],
}

#[repr(align(64))]
struct Bin(SpinLock<BinState>);

struct BinState {
    /// Slabs with free objects
    nonfull: *mut Run,
}

/// Free runs are binned by length in pages; the last bin holds the runs
/// of `FREE_BINS - 1` pages and more
const FREE_BINS: usize = (LARGE_MAX >> PAGE_SHIFT) + 1;
const FREE_WORDS: usize = FREE_BINS.div_ceil(64);

struct Pages {
    /// The arena's chunks, by address
    chunks: *mut ArenaChunk,
    /// Free runs of every chunk, most recently freed first, and which bins
    /// are not empty
    free: [*mut Run; FREE_BINS],
    nonempty: [u64; FREE_WORDS],
}

static ARENAS: [AtomicPtr<Arena>; MAX_ARENAS] = [const { AtomicPtr::new(null_mut()) }; MAX_ARENAS];
static NARENAS: AtomicUsize = AtomicUsize::new(0);
static INIT: SpinLock<()> = SpinLock::new(());
static SAMPLED: AtomicPtr<Arena> = AtomicPtr::new(null_mut());

fn narenas() -> usize {
    let n = NARENAS.load(Ordering::Relaxed);
    if n != 0 {
        return n;
    }
    let n = (os::ncpus() * 4).min(MAX_ARENAS);
    NARENAS.store(n, Ordering::Relaxed);
    n
}

unsafe fn create(sampled: bool) -> *mut Arena {
    let a: *mut Arena = base::alloc(size_of::<Arena>(), align_of::<Arena>()).cast();
    if !a.is_null() {
        ptr::write(
            a,
            Arena {
                sampled,
                threads: AtomicUsize::new(0),
                pages: SpinLock::new(Pages {
                    chunks: null_mut(),
                    free: [null_mut(); FREE_BINS],
                    nonempty: [0; FREE_WORDS],
                }),
                bins: [const {
                    Bin(SpinLock::new(BinState {
                        nonfull: null_mut(),
                    }))
                }; NSMALL],
            },
        );
    }
    a
}

unsafe fn get_or_create(slot: &AtomicPtr<Arena>, sampled: bool) -> *mut Arena {
    let a = slot.load(Ordering::Acquire);
    if !a.is_null() {
        return a;
    }
    let _init = INIT.lock();
    let a = slot.load(Ordering::Acquire);
    if !a.is_null() {
        return a;
    }
    let a = create(sampled);
    slot.store(a, Ordering::Release);
    a
}

/// The arena for a new thread: an idle one, else a new one while there
/// are fewer than the limit, else the least used. Reusing idle arenas
/// keeps the memory that exited threads freed in use.
pub unsafe fn choose() -> *mut Arena {
    let mut best = null_mut::<Arena>();
    let mut best_threads = usize::MAX;
    for slot in &ARENAS[..narenas()] {
        let a = slot.load(Ordering::Acquire);
        if a.is_null() {
            // Arenas are created in order, so the rest are missing too
            if best_threads > 0 {
                best = get_or_create(slot, false);
            }
            break;
        }
        let threads = (*a).threads.load(Ordering::Relaxed);
        if threads < best_threads {
            best = a;
            best_threads = threads;
            if threads == 0 {
                break;
            }
        }
    }
    if !best.is_null() {
        (*best).threads.fetch_add(1, Ordering::Relaxed);
    }
    best
}

/// A thread that used `arena` exited. The last one out frees what the
/// arena keeps for reuse, so that other arenas can take its chunks.
pub unsafe fn leave(arena: *mut Arena) {
    if (*arena).threads.fetch_sub(1, Ordering::Relaxed) == 1 {
        (*arena).trim();
    }
}

/// The arena of threads without a thread cache
pub unsafe fn fallback() -> *mut Arena {
    get_or_create(&ARENAS[0], false)
}

/// The arena for sampled allocations, whose chunks are marked so that
/// frees can tell them apart
pub unsafe fn sampled() -> *mut Arena {
    get_or_create(&SAMPLED, true)
}

/// Calls `f` on every arena created so far
pub unsafe fn for_each(mut f: impl FnMut(&Arena)) {
    for slot in ARENAS.iter().chain(core::iter::once(&SAMPLED)) {
        let a = slot.load(Ordering::Acquire);
        if !a.is_null() {
            f(&*a);
        }
    }
}

impl Arena {
    /// Allocates a run of `n` pages, at most a bin's worth; returns its
    /// chunk and first page
    unsafe fn alloc_run(&self, pages: &mut Pages, n: usize) -> Option<(*mut ArenaChunk, usize)> {
        let mut run = pages.find_free(n);
        if run.is_null() {
            let c = spare_pop();
            let c = if c.is_null() {
                ArenaChunk::create(self, self.sampled)
            } else {
                (*c).arena = self;
                (*c).head.sampled = u8::from(self.sampled);
                c
            };
            if c.is_null() {
                return None;
            }
            // Keep chunks by address, for decay to walk
            let mut link = &raw mut pages.chunks;
            while !(*link).is_null() && (*link).addr() < c.addr() {
                link = &raw mut (**link).next;
            }
            (*c).next = *link;
            *link = c;
            run = pages.insert_free(c, HEADER_PAGES, USABLE_PAGES);
        }
        pages.remove_free(run);
        let c = ArenaChunk::of(run.cast());
        let p = ArenaChunk::run_page(c, run);
        let len = (*run).npages as usize;
        ArenaChunk::take(c, p, p, n);
        if len > n {
            pages.insert_free(c, p + n, len - n);
        }
        Some((c, p))
    }

    /// Allocates a large run of `n` pages
    pub unsafe fn alloc_pages(&self, n: usize, class: u8) -> *mut Run {
        let mut pages = self.pages.lock();
        let Some((c, p)) = self.alloc_run(&mut pages, n) else {
            return null_mut();
        };
        let run = ArenaChunk::run(c, p);
        (*run).npages = n as u16;
        (*run).state = RUN_LARGE;
        (*run).class = class;
        (*run).sample = null_mut();
        run
    }

    pub unsafe fn alloc_large(&self, class: usize) -> *mut u8 {
        let run = self.alloc_pages(CLASS_SIZE[class] as usize >> PAGE_SHIFT, class as u8);
        if run.is_null() {
            return null_mut();
        }
        run_addr(run)
    }

    /// Fills `out` with up to `want` objects of a small class
    pub unsafe fn refill(&self, class: usize, out: *mut *mut u8, want: usize) -> usize {
        let size = CLASS_SIZE[class] as usize;
        let mut bin = self.bins[class].0.lock();
        let mut n = 0;
        while n < want {
            let mut run = bin.nonfull;
            if run.is_null() {
                run = self.new_slab(class);
                if run.is_null() {
                    break;
                }
                list_push(&mut bin.nonfull, run);
            }
            let base = run_addr(run);
            while n < want && (*run).nfree > 0 {
                let obj = if (*run).free_list.is_null() {
                    let obj = base.add((*run).bump as usize * size);
                    (*run).bump += 1;
                    obj
                } else {
                    let obj = (*run).free_list;
                    (*run).free_list = *obj.cast::<*mut u8>();
                    obj
                };
                (*run).nfree -= 1;
                *out.add(n) = obj;
                n += 1;
            }
            if (*run).nfree == 0 {
                list_remove(&mut bin.nonfull, run);
            }
        }
        n
    }

    unsafe fn new_slab(&self, class: usize) -> *mut Run {
        let mut pages = self.pages.lock();
        let Some((c, p)) = self.alloc_run(&mut pages, SLAB_PAGES[class] as usize) else {
            return null_mut();
        };
        let run = ArenaChunk::run(c, p);
        (*run).npages = u16::from(SLAB_PAGES[class]);
        (*run).class = class as u8;
        (*run).state = RUN_SLAB;
        (*run).nfree = SLAB_OBJS[class];
        (*run).bump = 0;
        (*run).free_list = null_mut();
        (*run).sample = null_mut();
        run
    }

    /// Returns a small object to its slab; the bin lock is held
    unsafe fn put_back(&self, bin: &mut BinState, class: usize, obj: *mut u8) {
        let c = ArenaChunk::of(obj);
        let (_, run) = ArenaChunk::run_of(c, obj);
        *obj.cast::<*mut u8>() = (*run).free_list;
        (*run).free_list = obj;
        (*run).nfree += 1;
        if (*run).nfree == 1 {
            list_push(&mut bin.nonfull, run);
        }
        // An empty slab goes back to the pages unless it is the bin's last
        if (*run).nfree == SLAB_OBJS[class] && !((*run).next.is_null() && (*run).prev.is_null()) {
            list_remove(&mut bin.nonfull, run);
            self.free_run(run);
        }
    }

    unsafe fn free_run(&self, run: *mut Run) {
        let mut pages = self.pages.lock();
        pages.release(run);
    }

    /// Grows or shrinks a large run in place to `class`
    pub unsafe fn resize_large(&self, c: *mut ArenaChunk, ptr: *mut u8, class: usize) -> bool {
        let mut pages = self.pages.lock();
        let (p, run) = ArenaChunk::run_of(c, ptr);
        if ArenaChunk::page_addr(c, p) != ptr || (*run).state != RUN_LARGE {
            return false;
        }
        let old = (*run).npages as usize;
        let new = CLASS_SIZE[class] as usize >> PAGE_SHIFT;
        if new < old {
            (*run).npages = new as u16;
            pages.give_back(c, p + new, old - new);
        } else if new > old {
            // Take the start of the free run that follows
            let e = p + old;
            if e >= PAGES || !ArenaChunk::is_free_page(c, e) {
                return false;
            }
            let next = ArenaChunk::run(c, e);
            let len = (*next).npages as usize;
            if len < new - old {
                return false;
            }
            pages.remove_free(next);
            ArenaChunk::take(c, p, e, new - old);
            if len > new - old {
                pages.insert_free(c, p + new, len - (new - old));
            }
            (*run).npages = new as u16;
        }
        (*run).class = class as u8;
        true
    }

    /// Frees the empty slab each bin keeps, and moves every empty chunk to
    /// the spare pool
    unsafe fn trim(&self) {
        for (class, bin) in self.bins.iter().enumerate() {
            let mut bin = bin.0.lock();
            let run = bin.nonfull;
            if !run.is_null() && (*run).next.is_null() && (*run).nfree == SLAB_OBJS[class] {
                list_remove(&mut bin.nonfull, run);
                self.free_run(run);
            }
        }
        let mut pages = self.pages.lock();
        let mut c = pages.chunks;
        while !c.is_null() {
            let next = (*c).next;
            if ArenaChunk::is_empty(c) {
                pages.retire(c);
            }
            c = next;
        }
    }

    /// Ends a decay epoch, or purges every dirty page when `all` is set;
    /// arenas without threads are trimmed too
    unsafe fn decay(&self, all: bool) {
        if all || self.threads.load(Ordering::Relaxed) == 0 {
            self.trim();
        }
        let pages = self.pages.lock();
        let mut c = pages.chunks;
        while !c.is_null() {
            ArenaChunk::decay(c, all);
            c = (*c).next;
        }
    }
}

impl Pages {
    /// Frees a run's pages; an emptied chunk goes to the spare pool unless
    /// it is the arena's last
    unsafe fn release(&mut self, run: *mut Run) {
        let c = ArenaChunk::of(run.cast());
        self.give_back(c, ArenaChunk::run_page(c, run), (*run).npages as usize);
        if ArenaChunk::is_empty(c) && (self.chunks != c || !(*c).next.is_null()) {
            self.retire(c);
        }
    }

    /// Frees pages `p..p + n`, merged with the free runs on either side
    unsafe fn give_back(&mut self, c: *mut ArenaChunk, p: usize, n: usize) {
        ArenaChunk::give_back(c, p, n);
        let (mut start, mut len) = (p, n);
        if p > HEADER_PAGES && ArenaChunk::is_free_page(c, p - 1) {
            start = ArenaChunk::run_start(c, p - 1);
            let left = ArenaChunk::run(c, start);
            self.remove_free(left);
            len += (*left).npages as usize;
        }
        if p + n < PAGES && ArenaChunk::is_free_page(c, p + n) {
            let right = ArenaChunk::run(c, p + n);
            self.remove_free(right);
            len += (*right).npages as usize;
        }
        self.insert_free(c, start, len);
    }

    /// Moves an empty chunk to the spare pool
    unsafe fn retire(&mut self, c: *mut ArenaChunk) {
        self.remove_free(ArenaChunk::run(c, HEADER_PAGES));
        self.unlink(c);
        spare_push(c);
    }

    /// Records pages `p..p + len` as a free run; its first and last pages
    /// lead to its metadata
    unsafe fn insert_free(&mut self, c: *mut ArenaChunk, p: usize, len: usize) -> *mut Run {
        let run = ArenaChunk::run(c, p);
        (*run).state = RUN_FREE;
        (*run).npages = len as u16;
        ArenaChunk::set_run_start(c, p, p);
        ArenaChunk::set_run_start(c, p + len - 1, p);
        let b = len.min(FREE_BINS - 1);
        list_push(&mut self.free[b], run);
        self.nonempty[b / 64] |= 1 << (b % 64);
        run
    }

    unsafe fn remove_free(&mut self, run: *mut Run) {
        let b = ((*run).npages as usize).min(FREE_BINS - 1);
        list_remove(&mut self.free[b], run);
        if self.free[b].is_null() {
            self.nonempty[b / 64] &= !(1 << (b % 64));
        }
    }

    /// The best fitting free run of at least `n` pages, or null
    fn find_free(&self, n: usize) -> *mut Run {
        let mut w = n / 64;
        let mut bits = self.nonempty[w] & (!0u64 << (n % 64));
        loop {
            if bits != 0 {
                return self.free[w * 64 + bits.trailing_zeros() as usize];
            }
            w += 1;
            if w == FREE_WORDS {
                return null_mut();
            }
            bits = self.nonempty[w];
        }
    }

    unsafe fn unlink(&mut self, c: *mut ArenaChunk) {
        let mut link = &raw mut self.chunks;
        while *link != c {
            link = &raw mut (**link).next;
        }
        *link = (*c).next;
        (*c).next = null_mut();
    }
}

/// Empty chunks that any arena can take, most recent first. They are
/// unmapped after staying unused for a whole decay epoch.
static SPARE: SpinLock<*mut ArenaChunk> = SpinLock::new(null_mut());

/// Decay epochs so far
static EPOCH: AtomicU64 = AtomicU64::new(0);
/// Nanoseconds since BASE at which the next epoch ends
static NEXT_EPOCH: AtomicU64 = AtomicU64::new(0);
static BASE: OnceLock<Instant> = OnceLock::new();
static DECAY: SpinLock<()> = SpinLock::new(());

unsafe fn spare_push(c: *mut ArenaChunk) {
    (*c).spare_epoch = EPOCH.load(Ordering::Relaxed);
    let mut spare = SPARE.lock();
    (*c).next = *spare;
    *spare = c;
}

unsafe fn spare_pop() -> *mut ArenaChunk {
    let mut spare = SPARE.lock();
    let c = *spare;
    if !c.is_null() {
        *spare = (*c).next;
        (*c).next = null_mut();
    }
    c
}

/// Unmaps the spare chunks that were not taken since the previous epoch,
/// or all of them
unsafe fn spare_decay(all: bool) {
    let epoch = EPOCH.load(Ordering::Relaxed);
    let mut unmap = null_mut::<ArenaChunk>();
    {
        let mut spare = SPARE.lock();
        let mut link = &raw mut *spare;
        while !(*link).is_null() {
            let c = *link;
            if all || (*c).spare_epoch + 1 < epoch {
                *link = (*c).next;
                (*c).next = unmap;
                unmap = c;
            } else {
                link = &raw mut (*c).next;
            }
        }
    }
    while !unmap.is_null() {
        let c = unmap;
        unmap = (*c).next;
        os::unmap(c.cast(), CHUNK);
        stats::CHUNK_BYTES.sub(CHUNK);
    }
}

#[cfg(all(unix, not(miri)))]
/// Takes every arena lock across `fork`, in the order the allocator nests
/// them: decay, arena creation, each arena's bins then pages, then spare
/// chunks
pub(crate) fn fork_lock() {
    DECAY.acquire();
    INIT.acquire();
    unsafe {
        for_each(|a| {
            for bin in &a.bins {
                bin.0.acquire();
            }
            a.pages.acquire();
        });
    }
    SPARE.acquire();
}

#[cfg(all(unix, not(miri)))]
pub(crate) unsafe fn fork_unlock() {
    SPARE.release();
    for_each(|a| {
        a.pages.release();
        for bin in &a.bins {
            bin.0.release();
        }
    });
    INIT.release();
    DECAY.release();
}

#[cfg(all(unix, not(miri)))]
/// In a child after `fork`, only the forking thread is left: no arena has
/// threads but its own
pub(crate) unsafe fn fork_child(own: *mut Arena) {
    for_each(|a| a.threads.store(0, Ordering::Relaxed));
    if !own.is_null() {
        (*own).threads.store(1, Ordering::Relaxed);
    }
}

/// Ends a decay epoch across all arenas if one is due; cheap otherwise
pub unsafe fn decay_tick() {
    // Before the first epoch there is no base and NEXT_EPOCH is 0, so the
    // first call goes on to the lock
    let now = BASE.get().map_or(0, |b| b.elapsed().as_nanos() as u64);
    if now < NEXT_EPOCH.load(Ordering::Relaxed) {
        return;
    }
    let Some(_decay) = DECAY.try_lock() else {
        return;
    };
    // Set under the decay lock, which `fork` holds: a child never finds it
    // half set by a thread that did not come along
    let now = BASE.get_or_init(Instant::now).elapsed().as_nanos() as u64;
    if now < NEXT_EPOCH.load(Ordering::Relaxed) {
        return;
    }
    NEXT_EPOCH.store(now + DECAY_EPOCH.as_nanos() as u64, Ordering::Relaxed);
    EPOCH.fetch_add(1, Ordering::Relaxed);
    for_each(|a| a.decay(false));
    spare_decay(false);
    crate::huge::decay();
}

/// Purges every dirty page and unmaps every empty chunk
pub unsafe fn purge() {
    let _decay = DECAY.lock();
    for_each(|a| a.decay(true));
    spare_decay(true);
}

#[inline]
pub unsafe fn run_addr(run: *mut Run) -> *mut u8 {
    let c = ArenaChunk::of(run.cast());
    ArenaChunk::page_addr(c, ArenaChunk::run_page(c, run))
}

unsafe fn list_push(head: &mut *mut Run, run: *mut Run) {
    (*run).prev = null_mut();
    (*run).next = *head;
    if !head.is_null() {
        (**head).prev = run;
    }
    *head = run;
}

unsafe fn list_remove(head: &mut *mut Run, run: *mut Run) {
    if (*run).prev.is_null() {
        *head = (*run).next;
    } else {
        (*(*run).prev).next = (*run).next;
    }
    if !(*run).next.is_null() {
        (*(*run).next).prev = (*run).prev;
    }
    (*run).next = null_mut();
    (*run).prev = null_mut();
}

/// Returns small objects of one class to their slabs, taking each arena's
/// bin lock once; `objs` is reordered
pub unsafe fn free_small_batch(objs: &mut [*mut u8], class: usize) {
    let mut left = objs.len();
    while left > 0 {
        let arena = (*ArenaChunk::of(objs[0])).arena;
        let mut bin = (*arena).bins[class].0.lock();
        let mut keep = 0;
        for i in 0..left {
            let obj = objs[i];
            if (*ArenaChunk::of(obj)).arena == arena {
                (*arena).put_back(&mut bin, class, obj);
            } else {
                objs[keep] = obj;
                keep += 1;
            }
        }
        left = keep;
    }
}

/// Frees a small object or a large run from an arena chunk, for a caller
/// without a thread cache
pub unsafe fn free(ptr: *mut u8) {
    let c = ArenaChunk::of(ptr);
    let arena = &*(*c).arena;
    // Slab or large is fixed while the object is live
    let (_, run) = ArenaChunk::run_of(c, ptr);
    if (*run).state == RUN_SLAB {
        let class = (*run).class as usize;
        let mut bin = arena.bins[class].0.lock();
        arena.put_back(&mut bin, class, ptr);
    } else {
        arena.free_run(run);
    }
}

/// The run that holds `ptr` in a sampled chunk, with its arena
pub unsafe fn sampled_run(ptr: *mut u8) -> *mut Run {
    ArenaChunk::run_of(ArenaChunk::of(ptr), ptr).1
}

pub unsafe fn free_sampled_run(ptr: *mut u8) {
    let c = ArenaChunk::of(ptr);
    let (_, run) = ArenaChunk::run_of(c, ptr);
    (*(*c).arena).free_run(run);
}

/// Large allocations aligned beyond a page: a run with room to align in
pub unsafe fn alloc_aligned(arena: &Arena, size: usize, align: usize) -> *mut Run {
    let n = (os::round_up(size.max(1), PAGE) + align - PAGE) >> PAGE_SHIFT;
    arena.alloc_pages(n, u8::MAX)
}
