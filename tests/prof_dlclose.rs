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

//! Dumping a profile while another thread loads and unloads a library, in
//! a binary of its own. Reading the mappings of a library that is being
//! unloaded must not crash.
#![cfg(unix)]

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use stoolap_jemalloc::{Jemalloc, prof};

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

static STOP: AtomicBool = AtomicBool::new(false);

#[test]
fn dump_while_libraries_unload() {
    let answer = "#[unsafe(no_mangle)] pub extern \"C\" fn answer() -> i32 { 42 }\n";
    let Some(lib) = common::build_library("dltarget", answer) else {
        eprintln!("rustc not found; skipped");
        return;
    };
    prof::activate();
    let path = std::ffi::CString::new(lib.to_str().unwrap()).unwrap();
    let churn = thread::spawn(move || {
        let mut cycles = 0u32;
        while !STOP.load(Ordering::Relaxed) {
            unsafe {
                let h = libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
                assert!(!h.is_null());
                libc::dlclose(h);
            }
            cycles += 1;
        }
        cycles
    });
    for _ in 0..150 {
        let profile = prof::dump_pprof().unwrap();
        assert!(profile.len() > 16);
    }
    STOP.store(true, Ordering::Relaxed);
    assert!(churn.join().unwrap() > 0);
    let _ = std::fs::remove_dir_all(lib.parent().unwrap());
}
