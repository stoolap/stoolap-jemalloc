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

//! Heap profiling, in a binary of its own since it changes global state

use std::hint::black_box;
use std::thread;
use stoolap_jemalloc::{Jemalloc, prof};

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

#[inline(never)]
fn leaky_function(n: usize) -> Vec<Vec<u8>> {
    (0..n).map(|i| vec![i as u8; 4096]).collect()
}

#[test]
fn profile_records_live_allocations() {
    assert!(prof::dump_pprof().is_err());
    prof::set_sample_interval(64 * 1024);
    prof::activate();

    // 40 MiB that stays live, and churn that does not
    let kept = black_box(leaky_function(10_000));
    let workers: Vec<_> = (0..4)
        .map(|_| {
            thread::spawn(|| {
                for i in 0..20_000 {
                    black_box(vec![0u8; 1000 + i % 5000]);
                }
            })
        })
        .collect();
    for w in workers {
        w.join().unwrap();
    }

    let profile = prof::dump_pprof().unwrap();
    assert!(profile.len() > 100);
    let text = String::from_utf8_lossy(&profile);
    for name in [
        "inuse_space",
        "alloc_space",
        "inuse_objects",
        "alloc_objects",
    ] {
        assert!(text.contains(name), "{name}");
    }
    #[cfg(feature = "symbolize")]
    assert!(
        text.contains("leaky_function"),
        "symbol names are in the profile"
    );

    let path = std::env::temp_dir().join(format!("stoolap-jemalloc-{}.pb", std::process::id()));
    let written = prof::write_pprof(&path).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().len() as usize, written);
    if let Some(dir) = std::env::var_os("PPROF_OUT") {
        std::fs::copy(&path, std::path::Path::new(&dir).join("heap.pb")).unwrap();
    }
    std::fs::remove_file(&path).unwrap();

    // Sampled memory is freed through the normal paths
    drop(kept);
    prof::deactivate();
    let mut v = Vec::new();
    for i in 0..100_000 {
        v.push(vec![i as u8; 100]);
    }
    drop(v);
}
