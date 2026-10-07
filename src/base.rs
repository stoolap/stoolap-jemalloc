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

//! Metadata memory: a bump allocator over OS mappings that never returns
//! memory, and fixed-size pools with free lists on top of it

use crate::lock::SpinLock;
use crate::os;
use crate::stats;
use core::marker::PhantomData;
use core::ptr::null_mut;

const BLOCK: usize = 1 << 21;

/// The free part of the current block: its start and end address
static BASE: SpinLock<(*mut u8, usize)> = SpinLock::new((null_mut(), 0));

/// Zeroed metadata memory, or null when the OS is out of memory
pub unsafe fn alloc(size: usize, align: usize) -> *mut u8 {
    let mut base = BASE.lock();
    let (cur, end) = *base;
    let mut p = cur.map_addr(|a| os::round_up(a, align));
    if cur.is_null() || p.addr() + size > end {
        let len = BLOCK.max(os::round_up(size + align, os::page_size()));
        let m = os::map(len);
        if m.is_null() {
            return null_mut();
        }
        stats::add_metadata(len as isize);
        p = m.map_addr(|a| os::round_up(a, align));
        *base = (p, m.addr() + len);
    }
    base.0 = p.add(size);
    p
}

/// Free list of `T`-sized blocks; a freed block's first word links it.
/// Blocks from the pool are zeroed only when they are new.
pub struct Pool<T> {
    free: SpinLock<*mut u8>,
    _t: PhantomData<*mut T>,
}

// The pool hands out raw memory; it holds no `T`
unsafe impl<T> Sync for Pool<T> {}

impl<T> Pool<T> {
    pub const fn new() -> Self {
        Pool {
            free: SpinLock::new(null_mut()),
            _t: PhantomData,
        }
    }

    pub unsafe fn alloc(&self) -> *mut T {
        {
            let mut head = self.free.lock();
            let p = *head;
            if !p.is_null() {
                *head = *p.cast::<*mut u8>();
                return p.cast();
            }
        }
        alloc(size_of::<T>().max(8), align_of::<T>().max(8)).cast()
    }

    pub unsafe fn free(&self, p: *mut T) {
        let mut head = self.free.lock();
        *p.cast::<*mut u8>() = *head;
        *head = p.cast();
    }
}
