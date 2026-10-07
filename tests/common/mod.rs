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

//! A minimal pprof reader for the tests
#![allow(dead_code)]

pub mod traits;

use std::collections::HashMap;

/// A function as the profile names it
pub struct Function {
    pub name: String,
    pub system_name: String,
    pub file: String,
}

/// The parts of a profile that the tests look at
pub struct Profile {
    pub functions: HashMap<u64, Function>,
    /// Function ids of each location's lines
    locations: HashMap<u64, Vec<u64>>,
    /// Mapping id of each location
    pub location_mappings: HashMap<u64, u64>,
    /// Location ids and values of each sample
    samples: Vec<(Vec<u64>, Vec<u64>)>,
    /// Id and path of each mapping, and whether it says its locations
    /// have their functions
    pub mappings: Vec<(u64, String, bool)>,
}

pub fn parse(profile: &[u8]) -> Profile {
    let mut strings = Vec::new();
    let mut raw_functions = Vec::new();
    let mut raw_mappings = Vec::new();
    let mut locations = HashMap::new();
    let mut location_mappings = HashMap::new();
    let mut samples = Vec::new();
    for (field, value) in fields(profile) {
        let Field::Bytes(b) = value else { continue };
        match field {
            2 => {
                let (mut locs, mut values) = (Vec::new(), Vec::new());
                for (f, v) in fields(b) {
                    match (f, v) {
                        (1, Field::Bytes(p)) => locs = packed(p),
                        (2, Field::Bytes(p)) => values = packed(p),
                        _ => {}
                    }
                }
                samples.push((locs, values));
            }
            3 => raw_mappings.push(varints(b)),
            4 => {
                let (mut id, mut fns) = (0, Vec::new());
                for (f, v) in fields(b) {
                    match (f, v) {
                        (1, Field::Varint(v)) => id = v,
                        (2, Field::Varint(v)) => {
                            location_mappings.insert(id, v);
                        }
                        (4, Field::Bytes(line)) => fns.extend(varints(line).get(&1)),
                        _ => {}
                    }
                }
                locations.insert(id, fns);
            }
            5 => raw_functions.push(varints(b)),
            6 => strings.push(String::from_utf8_lossy(b).into_owned()),
            _ => {}
        }
    }
    let string = |m: &HashMap<u64, u64>, f: u64| strings[*m.get(&f).unwrap_or(&0) as usize].clone();
    let functions = raw_functions
        .iter()
        .map(|f| {
            let function = Function {
                name: string(f, 2),
                system_name: string(f, 3),
                file: string(f, 4),
            };
            (f[&1], function)
        })
        .collect();
    let mappings = raw_mappings
        .iter()
        .map(|m| (m[&1], string(m, 5), m.get(&7).copied().unwrap_or(0) != 0))
        .collect();
    Profile {
        functions,
        locations,
        location_mappings,
        samples,
        mappings,
    }
}

/// A small dynamic library built with rustc from `code`, or None when
/// there is no rustc to build it with. It goes without std, so without
/// thread locals, which would keep it loaded.
pub fn build_library(name: &str, code: &str) -> Option<std::path::PathBuf> {
    let dir = std::env::temp_dir().join(format!("stoolap-jemalloc-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("lib.rs");
    let code = format!(
        "#![no_std]\n\
         #[panic_handler] fn panic(_: &core::panic::PanicInfo) -> ! {{ loop {{}} }}\n{code}"
    );
    std::fs::write(&src, code).unwrap();
    let out = dir.join(format!("lib{name}.so"));
    let mut rustc = std::process::Command::new("rustc");
    rustc.args([
        "--crate-type",
        "cdylib",
        "-C",
        "panic=abort",
        "--crate-name",
        name,
    ]);
    // Unwind tables let stacks be captured through the library's frames
    rustc.args(["-C", "force-unwind-tables=yes"]);
    if cfg!(target_vendor = "apple") {
        rustc.args(["-C", "link-arg=-lSystem"]);
    }
    let output = rustc.arg("-o").arg(&out).arg(&src).output().ok()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Some(out)
}

impl Profile {
    /// The lines of a location
    pub fn lines(&self, location: u64) -> usize {
        self.locations.get(&location).map_or(0, Vec::len)
    }

    /// The four sample values summed over the samples whose stack has a
    /// function whose name contains `function`: allocated objects and
    /// bytes, then live objects and bytes
    pub fn totals(&self, function: &str) -> [u64; 4] {
        let matches = |location: &u64| {
            self.locations.get(location).is_some_and(|fns| {
                fns.iter()
                    .any(|f| self.functions[f].name.contains(function))
            })
        };
        let mut sums = [0u64; 4];
        for (locations, values) in &self.samples {
            if locations.iter().any(matches) {
                for (sum, v) in sums.iter_mut().zip(values) {
                    *sum += v;
                }
            }
        }
        sums
    }
}

pub enum Field<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

fn varint(b: &[u8], at: &mut usize) -> u64 {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let byte = b[*at];
        *at += 1;
        v |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return v;
        }
        shift += 7;
    }
}

fn packed(b: &[u8]) -> Vec<u64> {
    let mut at = 0;
    let mut out = Vec::new();
    while at < b.len() {
        out.push(varint(b, &mut at));
    }
    out
}

/// The varint fields of a message, by field number
fn varints(b: &[u8]) -> HashMap<u64, u64> {
    fields(b)
        .into_iter()
        .filter_map(|(f, v)| match v {
            Field::Varint(v) => Some((f, v)),
            Field::Bytes(_) => None,
        })
        .collect()
}

/// The fields of a message, with length-delimited ones as bytes
pub fn fields(b: &[u8]) -> Vec<(u64, Field<'_>)> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < b.len() {
        let key = varint(b, &mut at);
        match key & 7 {
            0 => out.push((key >> 3, Field::Varint(varint(b, &mut at)))),
            2 => {
                let len = varint(b, &mut at) as usize;
                out.push((key >> 3, Field::Bytes(&b[at..at + len])));
                at += len;
            }
            wire => panic!("unexpected wire type {wire}"),
        }
    }
    out
}
