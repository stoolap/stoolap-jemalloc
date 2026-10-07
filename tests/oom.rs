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

//! Allocations when the OS refuses memory, in a binary of its own. Linux
//! only: an address space limit makes mmap fail on demand there.
#![cfg(target_os = "linux")]

use std::alloc::{GlobalAlloc, Layout};
use std::time::{Duration, Instant};
use stoolap_jemalloc::Jemalloc;

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc::new();

fn vm_size() -> libc::rlim_t {
    let st = std::fs::read_to_string("/proc/self/status").unwrap();
    let kb: libc::rlim_t = st
        .lines()
        .find(|l| l.starts_with("VmSize:"))
        .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        .unwrap();
    kb << 10
}

/// Runs `f` in a child process and returns its exit code
fn in_child(f: fn() -> bool) -> i32 {
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        unsafe { libc::_exit(i32::from(!f())) };
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut status = 0;
    while unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) } != pid {
        if Instant::now() > deadline {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            unsafe { libc::waitpid(pid, &raw mut status, 0) };
            return -1;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        -2
    }
}

/// Freed huge mappings wait in a cache for reuse by the same size. When
/// the OS refuses a mapping of another size, they go back to it first.
#[test]
fn cached_mappings_make_room() {
    let code = in_child(|| unsafe {
        // Two 12 MiB mappings, freed into the cache
        let freed = Layout::from_size_align(12 << 20, 8).unwrap();
        let a = GLOBAL.alloc(freed);
        let b = GLOBAL.alloc(freed);
        GLOBAL.dealloc(a, freed);
        GLOBAL.dealloc(b, freed);
        // Room for 8 MiB more than now, so 20 MiB fits only in what the
        // cache gives back
        let limit = vm_size() + (8 << 20);
        let rl = libc::rlimit {
            rlim_cur: limit,
            rlim_max: libc::RLIM_INFINITY,
        };
        assert_eq!(libc::setrlimit(libc::RLIMIT_AS, &raw const rl), 0);
        let want = Layout::from_size_align(20 << 20, 8).unwrap();
        let p = GLOBAL.alloc(want);
        if p.is_null() {
            return false;
        }
        p.write(1);
        GLOBAL.dealloc(p, want);
        true
    });
    assert_eq!(
        code, 0,
        "a 20 MiB allocation failed while 24 MiB sat in the cache"
    );
}

/// Allocations past the limit return null, and leave the allocator
/// working once memory is back
#[test]
fn null_past_the_limit_then_recovery() {
    let code = in_child(|| unsafe {
        let limit = vm_size() + (64 << 20);
        let rl = libc::rlimit {
            rlim_cur: limit,
            rlim_max: libc::RLIM_INFINITY,
        };
        assert_eq!(libc::setrlimit(libc::RLIMIT_AS, &raw const rl), 0);
        let mut live: [(*mut u8, Layout); 512] =
            [(core::ptr::null_mut(), Layout::new::<u8>()); 512];
        let mut n = 0;
        let mut nulls = 0;
        let sizes = [48usize, 3000, 40_000, 300_000, 3 << 20];
        for i in 0..4096 {
            let l = Layout::from_size_align(sizes[i % sizes.len()], 8).unwrap();
            let p = GLOBAL.alloc(l);
            if p.is_null() {
                nulls += 1;
                continue;
            }
            p.write(i as u8);
            if n < live.len() {
                live[n] = (p, l);
                n += 1;
            } else {
                GLOBAL.dealloc(p, l);
            }
        }
        for &(p, l) in &live[..n] {
            GLOBAL.dealloc(p, l);
        }
        let rl = libc::rlimit {
            rlim_cur: libc::RLIM_INFINITY,
            rlim_max: libc::RLIM_INFINITY,
        };
        libc::setrlimit(libc::RLIMIT_AS, &raw const rl);
        let l = Layout::from_size_align(5 << 20, 8).unwrap();
        let p = GLOBAL.alloc(l);
        let back = !p.is_null();
        if back {
            GLOBAL.dealloc(p, l);
        }
        nulls > 0 && back
    });
    assert_eq!(code, 0, "no null past the limit, or no recovery after it");
}
