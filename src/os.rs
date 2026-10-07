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

//! Virtual memory from the operating system. Fresh mappings are zeroed.

use core::sync::atomic::{AtomicUsize, Ordering};

static PAGE_SIZE: AtomicUsize = AtomicUsize::new(0);

/// The OS page size (4 KiB, or 16 KiB on Apple silicon)
#[inline]
pub fn page_size() -> usize {
    let p = PAGE_SIZE.load(Ordering::Relaxed);
    if p != 0 {
        return p;
    }
    let p = sys::page_size();
    PAGE_SIZE.store(p, Ordering::Relaxed);
    p
}

#[inline]
pub fn round_up(n: usize, align: usize) -> usize {
    (n + align - 1) & !(align - 1)
}

/// Maps `size` bytes aligned to `align`; `size` must be a multiple of the
/// OS page size and `align` a power of two
pub unsafe fn map_aligned(size: usize, align: usize) -> *mut u8 {
    let p = sys::map(size);
    if p.is_null() || p.addr() & (align - 1) == 0 {
        return p;
    }
    sys::unmap(p, size);
    sys::map_aligned_slow(size, align)
}

pub unsafe fn map(size: usize) -> *mut u8 {
    sys::map(size)
}

/// Unmaps a whole mapping made by `map` or `map_aligned`
pub unsafe fn unmap(p: *mut u8, size: usize) {
    sys::unmap(p, size);
}

/// Gives the pages back to the OS but keeps the range mapped; their
/// contents become undefined. Partial OS pages at the ends are kept.
pub unsafe fn purge(p: *mut u8, size: usize) {
    let ps = page_size();
    let start = p.map_addr(|a| round_up(a, ps));
    let end = (p.addr() + size) & !(ps - 1);
    if end > start.addr() {
        sys::purge(start, end - start.addr());
    }
}

/// Keeps transparent huge pages out of a range on Linux. A huge page stays
/// resident while any of its 4 KiB is in use, and with huge pages a
/// multi-threaded `Vec` growth benchmark ran over twice as slow.
pub unsafe fn no_huge_pages(p: *mut u8, size: usize) {
    #[cfg(all(any(target_os = "linux", target_os = "android"), not(miri)))]
    libc::madvise(p.cast(), size, libc::MADV_NOHUGEPAGE);
    #[cfg(not(all(any(target_os = "linux", target_os = "android"), not(miri))))]
    let _ = (p, size);
}

pub fn ncpus() -> usize {
    sys::ncpus().max(1)
}

#[cfg(all(unix, not(miri)))]
mod sys {
    use super::round_up;
    use core::ptr::null_mut;

    pub fn page_size() -> usize {
        unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
    }

    pub fn ncpus() -> usize {
        unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN).max(1) as usize }
    }

    pub unsafe fn map(size: usize) -> *mut u8 {
        let p = libc::mmap(
            null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        );
        if p == libc::MAP_FAILED {
            null_mut()
        } else {
            p.cast()
        }
    }

    pub unsafe fn map_aligned_slow(size: usize, align: usize) -> *mut u8 {
        let total = size + align;
        let p = map(total);
        if p.is_null() {
            return p;
        }
        let a = p.map_addr(|x| round_up(x, align));
        let head = a.addr() - p.addr();
        if head > 0 {
            unmap(p, head);
        }
        let tail = total - head - size;
        if tail > 0 {
            unmap(a.add(size), tail);
        }
        a
    }

    pub unsafe fn unmap(p: *mut u8, size: usize) {
        libc::munmap(p.cast(), size);
    }

    pub unsafe fn purge(p: *mut u8, size: usize) {
        #[cfg(target_vendor = "apple")]
        let advice = libc::MADV_FREE;
        #[cfg(not(target_vendor = "apple"))]
        let advice = libc::MADV_DONTNEED;
        libc::madvise(p.cast(), size, advice);
    }
}

#[cfg(all(windows, not(miri)))]
mod sys {
    use super::round_up;
    use core::ptr::null_mut;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, MEM_RESET, PAGE_NOACCESS, PAGE_READWRITE,
        VirtualAlloc, VirtualFree,
    };
    use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};

    fn info() -> SYSTEM_INFO {
        unsafe {
            let mut si = core::mem::zeroed();
            GetSystemInfo(&raw mut si);
            si
        }
    }

    pub fn page_size() -> usize {
        info().dwPageSize as usize
    }

    pub fn ncpus() -> usize {
        info().dwNumberOfProcessors as usize
    }

    pub unsafe fn map(size: usize) -> *mut u8 {
        VirtualAlloc(
            core::ptr::null(),
            size,
            MEM_RESERVE | MEM_COMMIT,
            PAGE_READWRITE,
        )
        .cast()
    }

    /// Reserves a larger range to find an aligned address, releases it and
    /// maps at that address, retrying if another thread took it meanwhile
    pub unsafe fn map_aligned_slow(size: usize, align: usize) -> *mut u8 {
        for _ in 0..64 {
            let r = VirtualAlloc(core::ptr::null(), size + align, MEM_RESERVE, PAGE_NOACCESS);
            if r.is_null() {
                return null_mut();
            }
            VirtualFree(r, 0, MEM_RELEASE);
            let a = round_up(r.addr(), align);
            let p = VirtualAlloc(
                core::ptr::without_provenance(a),
                size,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            );
            if p.addr() == a {
                return p.cast();
            }
            if !p.is_null() {
                VirtualFree(p, 0, MEM_RELEASE);
            }
        }
        null_mut()
    }

    pub unsafe fn unmap(p: *mut u8, _size: usize) {
        VirtualFree(p.cast(), 0, MEM_RELEASE);
    }

    pub unsafe fn purge(p: *mut u8, size: usize) {
        VirtualAlloc(p.cast(), size, MEM_RESET, PAGE_READWRITE);
    }
}

/// Under Miri, which cannot run mmap, memory comes from the system
/// allocator and is never returned; run Miri with `-Zmiri-ignore-leaks`
#[cfg(miri)]
mod sys {
    use std::alloc::{GlobalAlloc, Layout, System};

    pub fn page_size() -> usize {
        4096
    }

    pub fn ncpus() -> usize {
        2
    }

    pub unsafe fn map(size: usize) -> *mut u8 {
        map_aligned_slow(size, page_size())
    }

    pub unsafe fn map_aligned_slow(size: usize, align: usize) -> *mut u8 {
        System.alloc_zeroed(Layout::from_size_align(size, align).unwrap())
    }

    pub unsafe fn unmap(_p: *mut u8, _size: usize) {}

    pub unsafe fn purge(_p: *mut u8, _size: usize) {}
}
