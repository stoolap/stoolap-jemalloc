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

//! Statistics and purging, in a binary of its own so that no other test
//! changes the counters meanwhile

use stoolap_jemalloc::Jemalloc;

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc::new();

#[test]
fn stats_follow_huge_allocations_and_purge() {
    let before = stoolap_jemalloc::stats();
    let block = vec![1u8; 8 << 20];
    let during = stoolap_jemalloc::stats();
    assert!(during.huge >= before.huge + (8 << 20));
    assert_eq!(
        during.mapped(),
        during.chunks + during.huge + during.huge_cached + during.metadata
    );

    // A freed huge mapping is kept for reuse, until a purge unmaps it
    drop(block);
    let freed = stoolap_jemalloc::stats();
    assert!(freed.huge_cached >= 8 << 20);
    assert_eq!(freed.huge, before.huge);

    // The next allocation of that size takes it back out
    let reused = vec![2u8; 8 << 20];
    assert!(stoolap_jemalloc::stats().huge_cached + (8 << 20) <= freed.huge_cached);
    drop(reused);

    // Small and large memory comes back after a purge too
    let many: Vec<Vec<u8>> = (0..2000).map(|i| vec![i as u8; 2000]).collect();
    drop(many);
    stoolap_jemalloc::purge();
    let after = stoolap_jemalloc::stats();
    assert_eq!(after.huge_cached, 0);
    assert_eq!(after.huge, before.huge);
    assert!(after.chunks <= during.chunks + (4 << 20));
}
