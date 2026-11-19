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

//! Purged pages leave the resident set, in a binary of its own so that no
//! other test moves it meanwhile
#![cfg(any(unix, windows))]

use std::hint::black_box;
use stoolap_jemalloc::Jemalloc;

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc::new();

/// Bytes of the process resident in memory, None without an exact count
#[cfg_attr(
    not(all(unix, not(target_vendor = "apple"))),
    allow(clippy::unnecessary_wraps, reason = "only Linux may lack the count")
)]
fn resident() -> Option<usize> {
    #[cfg(target_vendor = "apple")]
    unsafe {
        let mut info: libc::rusage_info_v4 = std::mem::zeroed();
        let rc =
            libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V4, (&raw mut info).cast());
        assert_eq!(rc, 0);
        Some(info.ri_resident_size as usize)
    }
    // statm reads per-CPU counters that may lag by tens of MiB on large
    // machines; smaps_rollup counts the page tables, as proc(5) advises,
    // and is missing from kernels without CONFIG_PROC_PAGE_MONITOR
    #[cfg(all(unix, not(target_vendor = "apple")))]
    {
        let rollup = std::fs::read_to_string("/proc/self/smaps_rollup").ok()?;
        let line = rollup.lines().find(|l| l.starts_with("Rss:"))?;
        let kib: usize = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kib << 10)
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        let size = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        assert_ne!(
            GetProcessMemoryInfo(GetCurrentProcess(), &raw mut counters, size),
            0
        );
        Some(counters.WorkingSetSize)
    }
}

#[test]
fn a_purge_takes_freed_pages_of_chunks_in_use_out_of_memory() {
    // 64 MiB of large runs with every other one kept: each chunk keeps live
    // runs, so the freed ones can only be purged, never unmapped
    let fill = |i: usize| (i as u8) | 1;
    let mut blocks: Vec<Option<Vec<u8>>> = (0..128)
        .map(|i| Some(black_box(vec![fill(i); 512 << 10])))
        .collect();
    for block in blocks.iter_mut().step_by(2) {
        *block = None;
    }
    let before = resident();
    stoolap_jemalloc::purge();
    match (before, resident()) {
        (Some(before), Some(after)) => assert!(
            before.saturating_sub(after) >= 24 << 20,
            "32 MiB freed, resident {} -> {} MiB",
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
    let again: Vec<Vec<u8>> = (0..64)
        .map(|i| black_box(vec![fill(i); 512 << 10]))
        .collect();
    for (i, block) in again.iter().enumerate() {
        assert!(block.iter().all(|&b| b == fill(i)), "block {i} reused");
    }
}
