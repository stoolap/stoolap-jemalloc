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

//! Profiles that cover a library loaded after the first dump, in a binary
//! of its own
#![cfg(unix)]

mod common;

use std::hint::black_box;
use stoolap_jemalloc::{Jemalloc, prof};

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

extern "C" fn allocate_from_library() {
    std::mem::forget(black_box(vec![5u8; 64 << 10]));
}

/// A library loaded after the first dump may be unknown to the in-process
/// symbolizer. Its mapping must then not claim that its locations carry
/// their functions, so that pprof symbolizes them instead.
#[test]
fn libraries_loaded_after_a_dump() {
    let code = "#[unsafe(no_mangle)] pub extern \"C\" fn call_back(f: extern \"C\" fn()) { f() }\n";
    let Some(lib) = common::build_library("dlcallback", code) else {
        eprintln!("rustc not found; skipped");
        return;
    };
    prof::set_sample_interval(1);
    prof::activate();
    drop(black_box(vec![0u8; 2 << 20]));
    prof::dump_pprof().unwrap();

    let path = std::ffi::CString::new(lib.to_str().unwrap()).unwrap();
    let profile = unsafe {
        let h = libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        assert!(!h.is_null());
        let sym = libc::dlsym(h, c"call_back".as_ptr());
        assert!(!sym.is_null());
        let call_back: extern "C" fn(extern "C" fn()) = std::mem::transmute(sym);
        call_back(allocate_from_library);
        let profile = common::parse(&prof::dump_pprof().unwrap());
        libc::dlclose(h);
        profile
    };

    let library = profile
        .mappings
        .iter()
        .find(|m| m.1.contains("libdlcallback"))
        .map(|m| m.0)
        .expect("the library's mapping is in the profile");
    assert!(
        profile.location_mappings.values().any(|&m| m == library),
        "a sample's stack goes through the library"
    );
    for (id, _, has_functions) in &profile.mappings {
        if *has_functions {
            for (&location, &mapping) in &profile.location_mappings {
                assert!(
                    mapping != *id || profile.lines(location) > 0,
                    "mapping {id} claims functions its location {location} lacks"
                );
            }
        }
    }
    let _ = std::fs::remove_dir_all(lib.parent().unwrap());
}
