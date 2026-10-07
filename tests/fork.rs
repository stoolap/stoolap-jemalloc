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

//! Forking while other threads allocate, in a binary of its own. A child
//! starts with only the forking thread, so a lock that another thread held
//! at the fork would never be released in it.
#![cfg(unix)]

use std::hint::black_box;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use stoolap_jemalloc::{Jemalloc, background, prof};

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc::new();

static STOP: AtomicBool = AtomicBool::new(false);

/// Allocates and frees in every size range, taking every kind of lock
fn churn(seed: usize) {
    let mut kept: Vec<Vec<u8>> = Vec::new();
    let mut i = seed as u64;
    while !STOP.load(Ordering::Relaxed) {
        i = i.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        let size = match i % 100 {
            0 => 2 << 20,
            1..=10 => 100_000,
            _ => 16 + (i % 4000) as usize,
        };
        kept.push(black_box(vec![1u8; size]));
        if kept.len() > 64 {
            kept.swap_remove((i % 64) as usize % kept.len());
        }
        if i.is_multiple_of(1000) {
            stoolap_jemalloc::purge();
        }
    }
}

/// What a child does: allocate in every size range, then exit. `dump`
/// also writes a profile, which loads the symbol tables: slow, so only
/// some children do.
fn child_work(dump: bool) -> ! {
    let mut v: Vec<Vec<u8>> = (0..2000).map(|i| vec![2u8; 16 + i * 37 % 5000]).collect();
    v.push(vec![3u8; 300_000]);
    v.push(vec![4u8; 3 << 20]);
    // Bytes spread over each block: memory handed out twice differs there
    let ok = v
        .iter()
        .all(|b| b.iter().step_by(61).chain(b.last()).all(|&x| x == b[0]));
    drop(v);
    stoolap_jemalloc::purge();
    let ok = ok && (!dump || prof::dump_pprof().is_ok());
    // The parent's background thread did not come along
    let ok = ok && !background::is_running();
    unsafe { libc::_exit(i32::from(!ok)) }
}

#[test]
fn fork_while_other_threads_allocate() {
    prof::set_sample_interval(4096);
    prof::activate();
    // Forks also happen while it holds the decay lock
    background::start();
    let workers: Vec<_> = (0..4).map(|t| thread::spawn(move || churn(t))).collect();
    // At least 20 forks, then until 3 seconds or 200 forks: without the
    // fork handlers, the first few children already fail
    let start = Instant::now();
    let mut round = 0;
    while round < 20 || (round < 200 && start.elapsed() < Duration::from_secs(3)) {
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            child_work(round % 10 == 0);
        }
        // A child that does not exit within the deadline is stuck on a lock
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut status = 0;
        loop {
            let done = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) };
            if done == pid {
                break;
            }
            if Instant::now() > deadline {
                unsafe { libc::kill(pid, libc::SIGKILL) };
                unsafe { libc::waitpid(pid, &raw mut status, 0) };
                STOP.store(true, Ordering::Relaxed);
                panic!("child {round} deadlocked after fork");
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "child {round} failed: {status}"
        );
        round += 1;
    }
    STOP.store(true, Ordering::Relaxed);
    for w in workers {
        w.join().unwrap();
    }
}
