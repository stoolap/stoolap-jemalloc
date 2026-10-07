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

use std::collections::BTreeMap;
use std::hint::black_box;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(feature = "ours")]
#[global_allocator]
static GLOBAL: stoolap_jemalloc::Jemalloc = stoolap_jemalloc::Jemalloc::new();
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

const NAME: &str = if cfg!(feature = "ours") {
    "stoolap-jemalloc"
} else if cfg!(feature = "mimalloc") {
    "mimalloc"
} else if cfg!(feature = "jemalloc") {
    "jemalloc (C)"
} else {
    "system"
};

fn threads() -> usize {
    thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Allocate and free small boxes in LIFO order
fn small_churn(n: usize) {
    for _ in 0..n {
        let v: Vec<Box<[u64; 4]>> = (0..64).map(|i| Box::new([i; 4])).collect();
        black_box(&v);
    }
}

/// Live set of mixed sizes with random frees
fn mixed(seed: u64, n: usize) {
    let mut rng = Rng(seed | 1);
    let mut live: Vec<Vec<u8>> = Vec::with_capacity(4096);
    for _ in 0..n {
        let r = rng.next();
        if live.len() < 4000 && !r.is_multiple_of(3) {
            let size = match r % 100 {
                0 => (rng.next() % 200_000) as usize,
                1..=9 => (rng.next() % 4000) as usize,
                _ => (rng.next() % 256) as usize + 1,
            };
            live.push(vec![1u8; size]);
        } else if !live.is_empty() {
            let i = (r as usize >> 3) % live.len();
            live.swap_remove(i);
        }
    }
}

fn strings_tree(n: usize) {
    let mut t = BTreeMap::new();
    for i in 0..n {
        t.insert(format!("key-{i:08}"), format!("value {i}"));
    }
    for i in (0..n).step_by(2) {
        t.remove(&format!("key-{i:08}"));
    }
    black_box(t);
}

fn vec_growth(n: usize) {
    for _ in 0..n {
        let mut v = Vec::new();
        for i in 0..100_000u32 {
            v.push(i);
        }
        black_box(v);
    }
}

/// Objects made on one thread and freed on another
fn producer_consumer(pairs: usize, n: usize) {
    let mut handles = Vec::new();
    for _ in 0..pairs {
        let (tx, rx) = mpsc::sync_channel::<Vec<Box<[u8; 48]>>>(16);
        handles.push(thread::spawn(move || {
            for _ in 0..n {
                tx.send((0..256).map(|_| Box::new([0u8; 48])).collect())
                    .unwrap();
            }
        }));
        handles.push(thread::spawn(move || {
            for v in rx {
                black_box(v);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
}

fn parallel(f: impl Fn(u64) + Sync) {
    thread::scope(|s| {
        for t in 0..threads() {
            let f = &f;
            s.spawn(move || f(t as u64 + 1));
        }
    });
}

fn peak_rss_mb() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let scale = if cfg!(target_os = "macos") {
        1.0
    } else {
        1024.0
    };
    ru.ru_maxrss as f64 * scale / (1024.0 * 1024.0)
}

fn run(name: &str, f: impl Fn()) {
    if std::env::args()
        .nth(1)
        .is_some_and(|o| !name.starts_with(&o))
    {
        return;
    }
    let mut best = Duration::MAX;
    let runs = if std::env::var_os("ONCE").is_some() {
        1
    } else {
        5
    };
    for _ in 0..runs {
        let t = Instant::now();
        f();
        best = best.min(t.elapsed());
        #[cfg(feature = "ours")]
        if std::env::var_os("STATS").is_some() {
            let s = stoolap_jemalloc::stats();
            eprintln!(
                "  chunks {:.1} MB, huge {:.1} MB, cached {:.1} MB, meta {:.1} MB, rss {:.1} MB",
                s.chunks as f64 / 1048576.0,
                s.huge as f64 / 1048576.0,
                s.huge_cached as f64 / 1048576.0,
                s.metadata as f64 / 1048576.0,
                peak_rss_mb()
            );
        }
    }
    println!(
        "{NAME:>18} | {name:<26} | {:>9.2} ms",
        best.as_secs_f64() * 1e3
    );
}

fn main() {
    // An argument picks the benchmarks whose names start with it
    let only: Option<String> = std::env::args().nth(1);
    let want = |n: &str| {
        only.as_deref()
            .is_none_or(|o| o.starts_with(n) || n.starts_with(o))
    };
    if want("small_churn") {
        run("small_churn 1t", || small_churn(100_000));
    }
    if want("small_churn") {
        run("small_churn Nt", || parallel(|_| small_churn(100_000)));
    }
    if want("mixed") {
        run("mixed 1t", || mixed(7, 2_000_000));
        run("mixed Nt", || parallel(|s| mixed(s, 2_000_000)));
    }
    if want("strings") {
        run("strings_tree 1t", || strings_tree(300_000));
        run("strings_tree Nt", || parallel(|_| strings_tree(300_000)));
    }
    if want("vec") {
        run("vec_growth Nt", || parallel(|_| vec_growth(200)));
    }
    if want("producer") {
        run("producer_consumer", || {
            producer_consumer(threads() / 2, 2_000)
        });
    }
    println!(
        "{NAME:>18} | {:<26} | {:>9.1} MB",
        "peak RSS",
        peak_rss_mb()
    );
}
