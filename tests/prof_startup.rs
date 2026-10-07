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

//! Profiling on from the allocator's setting, in a binary of its own: it
//! samples from the process's first allocation, with no call to activate

mod common;

use std::hint::black_box;
use stoolap_jemalloc::{Jemalloc, prof};

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc::new().with_profiling();

#[inline(never)]
fn startup_block() -> Vec<u8> {
    black_box(vec![1u8; 8 << 20])
}

#[test]
fn profiling_runs_from_the_first_allocation() {
    assert!(prof::is_active(), "on before the test ran");
    let kept = startup_block();
    let profile = prof::dump_pprof().unwrap();
    #[cfg(feature = "symbolize")]
    {
        let live = common::parse(&profile).totals("startup_block")[3];
        assert!(live >= 8 << 20, "{live} live bytes for 8 MiB");
    }
    #[cfg(not(feature = "symbolize"))]
    assert!(profile.len() > 16);
    drop(kept);

    // Switched off, it stays off: the setting does not turn it back on
    prof::deactivate();
    drop(black_box(vec![0u8; 4 << 20]));
    assert!(!prof::is_active());
}
