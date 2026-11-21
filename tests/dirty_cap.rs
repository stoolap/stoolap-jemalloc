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

//! A burst of frees leaves memory before the decay epoch ends, in a binary
//! of its own so that no other test moves the resident set or the faults
#![cfg(unix)]

use std::hint::black_box;
use std::sync::{Mutex, PoisonError};
use stoolap_jemalloc::Jemalloc;

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc::new();

/// The tests read process-wide counters, so they take turns
static SERIAL: Mutex<()> = Mutex::new(());

fn usage() -> libc::rusage {
    unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        assert_eq!(libc::getrusage(libc::RUSAGE_SELF, &raw mut ru), 0);
        ru
    }
}

#[cfg(target_vendor = "apple")]
#[allow(clippy::unnecessary_wraps, reason = "only Linux may lack the count")]
fn resident() -> Option<usize> {
    unsafe {
        let mut info: libc::rusage_info_v4 = std::mem::zeroed();
        let rc =
            libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V4, (&raw mut info).cast());
        assert_eq!(rc, 0);
        Some(info.ri_resident_size as usize)
    }
}

#[cfg(not(target_vendor = "apple"))]
fn resident() -> Option<usize> {
    let rollup = std::fs::read_to_string("/proc/self/smaps_rollup").ok()?;
    let line = rollup.lines().find(|l| l.starts_with("Rss:"))?;
    let kib: usize = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib << 10)
}

#[test]
fn a_burst_of_frees_leaves_memory_without_waiting_for_decay() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    // 192 MiB of large runs with every other one kept: the chunks stay in
    // use, so the 96 MiB freed can only be purged
    let fill = |i: usize| (i as u8) | 1;
    let mut blocks: Vec<Option<Vec<u8>>> = (0..384)
        .map(|i| Some(black_box(vec![fill(i); 512 << 10])))
        .collect();
    let before = resident();
    for block in blocks.iter_mut().step_by(2) {
        *block = None;
    }
    match (before, resident()) {
        (Some(before), Some(after)) => assert!(
            before.saturating_sub(after) >= 24 << 20,
            "96 MiB freed, resident {} -> {} MiB",
            before >> 20,
            after >> 20
        ),
        _ => eprintln!("no exact resident count here: only the contents are checked"),
    }
    for (i, block) in blocks.iter().enumerate() {
        if let Some(block) = block {
            assert!(block.iter().all(|&b| b == fill(i)), "block {i} kept");
        }
    }
}

#[test]
fn memory_freed_and_taken_again_is_not_purged() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    black_box(vec![1u8; 512 << 10]);
    let before = usage().ru_minflt;
    // 512 MiB freed in all, never more than 512 KiB of it at once
    for i in 0..1024 {
        black_box(vec![i as u8; 512 << 10]);
    }
    let faults = usage().ru_minflt - before;
    assert!(faults < 64, "{faults} page faults reusing one run");
}
