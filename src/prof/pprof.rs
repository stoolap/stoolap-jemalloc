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

//! Encodes the profile as pprof's `profile.proto`, by hand to stay free
//! of dependencies

use super::StackCounts;
use super::maps::{self, Mapping};
use std::collections::HashMap;
use std::fmt;

/// Why a heap profile could not be dumped
#[derive(Debug)]
pub enum DumpError {
    /// Profiling was never activated, so there is nothing to dump
    NotActivated,
    /// Writing the profile file failed
    Io(std::io::Error),
}

impl fmt::Display for DumpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DumpError::NotActivated => f.write_str("heap profiling was never activated"),
            DumpError::Io(e) => write!(f, "writing the heap profile: {e}"),
        }
    }
}

impl std::error::Error for DumpError {}

// Wire types
const VARINT: u64 = 0;
const LEN: u64 = 2;

fn varint(b: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        b.push(v as u8 | 0x80);
        v >>= 7;
    }
    b.push(v as u8);
}

fn tag(b: &mut Vec<u8>, field: u64, wire: u64) {
    varint(b, field << 3 | wire);
}

fn uint(b: &mut Vec<u8>, field: u64, v: u64) {
    if v != 0 {
        tag(b, field, VARINT);
        varint(b, v);
    }
}

fn bytes(b: &mut Vec<u8>, field: u64, data: &[u8]) {
    tag(b, field, LEN);
    varint(b, data.len() as u64);
    b.extend_from_slice(data);
}

fn packed(b: &mut Vec<u8>, field: u64, values: impl IntoIterator<Item = u64>) {
    let mut tmp = Vec::new();
    for v in values {
        varint(&mut tmp, v);
    }
    bytes(b, field, &tmp);
}

#[derive(Default)]
struct Strings {
    ids: HashMap<String, u64>,
    list: Vec<String>,
}

impl Strings {
    fn id(&mut self, s: &str) -> u64 {
        if self.list.is_empty() {
            self.list.push(String::new());
            self.ids.insert(String::new(), 0);
        }
        if let Some(&id) = self.ids.get(s) {
            return id;
        }
        let id = self.list.len() as u64;
        self.list.push(s.to_string());
        self.ids.insert(s.to_string(), id);
        id
    }
}

/// `ValueType { type, unit }`
fn value_type(strings: &mut Strings, ty: &str, unit: &str) -> Vec<u8> {
    let mut b = Vec::new();
    uint(&mut b, 1, strings.id(ty));
    uint(&mut b, 2, strings.id(unit));
    b
}

/// A source line of a location: the function's name, simplified for
/// display and in full, its file and the line number
struct Line {
    name: String,
    system_name: String,
    file: String,
    number: u64,
}

#[cfg(feature = "symbolize")]
/// Rust's demangled name without generic arguments and trait
/// qualifications, which pprof would otherwise cut into pieces:
/// `<core::result::Result<T, E> as FnOnce<A>>::call_once` becomes
/// `core::result::Result::call_once`
fn simplify(name: &str) -> String {
    enum Bracket {
        Generic,
        Qualified { trait_part: bool },
    }
    let mut out = String::with_capacity(name.len());
    let mut stack: Vec<Bracket> = Vec::new();
    let mut rest = name;
    let mut prev = ' ';
    while let Some(c) = rest.chars().next() {
        let skipping = stack.iter().any(|b| {
            matches!(
                b,
                Bracket::Generic | Bracket::Qualified { trait_part: true }
            )
        });
        match c {
            '<' => {
                // `Vec<T>` and `f::<T>` are generic; `<impl Trait for T>` is not
                let generic = skipping
                    || (!rest.starts_with("<impl ")
                        && out.ends_with(|p: char| p.is_alphanumeric() || p == '_' || p == ':'));
                if generic && !skipping && out.ends_with("::") {
                    out.truncate(out.len() - 2);
                }
                stack.push(if generic {
                    Bracket::Generic
                } else {
                    Bracket::Qualified { trait_part: false }
                });
            }
            // `->` inside generic arguments is not a closing bracket
            '>' if !stack.is_empty() && prev != '-' => {
                stack.pop();
            }
            _ if skipping => {}
            ' ' if rest.starts_with(" as ")
                && matches!(stack.last(), Some(Bracket::Qualified { .. })) =>
            {
                stack.pop();
                stack.push(Bracket::Qualified { trait_part: true });
                rest = &rest[" as".len()..];
            }
            _ => out.push(c),
        }
        rest = &rest[c.len_utf8()..];
        prev = c;
    }
    out
}

/// Whether a line belongs to the allocator itself: by its source file,
/// or by its name, for the allocator shims that live in the program.
/// Builds with `debug = "line-tables-only"` name functions without their
/// paths, so the name alone does not tell.
fn is_internal(line: &Line) -> bool {
    let own_source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    std::path::Path::new(&line.file).starts_with(own_source)
        || ["stoolap_jemalloc::", "__rustc::", "__rust_", "__rg_"]
            .iter()
            .any(|p| line.name.starts_with(p))
}

/// A location's lines without the allocator's own, which inlining puts at
/// the innermost end; `None` when every line is the allocator's
fn user_lines(addr: usize) -> Option<Vec<Line>> {
    let mut lines = symbolize(addr);
    let internal = lines.iter().take_while(|l| is_internal(l)).count();
    if internal > 0 && internal == lines.len() {
        return None;
    }
    lines.drain(..internal);
    Some(lines)
}

#[cfg(feature = "symbolize")]
fn symbolize(addr: usize) -> Vec<Line> {
    let mut lines = Vec::new();
    // Innermost first, as pprof wants inlined frames ordered
    backtrace::resolve(addr as *mut core::ffi::c_void, |sym| {
        let Some(name) = sym.name() else { return };
        let system_name = format!("{name:#}");
        lines.push(Line {
            name: simplify(&system_name),
            system_name,
            file: sym
                .filename()
                .map(|f| f.display().to_string())
                .unwrap_or_default(),
            number: u64::from(sym.lineno().unwrap_or(0)),
        });
    });
    lines
}

#[cfg(not(feature = "symbolize"))]
fn symbolize(_addr: usize) -> Vec<Line> {
    Vec::new()
}

pub(super) fn encode(stacks: &[StackCounts], interval: usize) -> Vec<u8> {
    let mut strings = Strings::default();
    strings.id("");
    let mut out = Vec::new();

    for (ty, unit) in [
        ("alloc_objects", "count"),
        ("alloc_space", "bytes"),
        ("inuse_objects", "count"),
        ("inuse_space", "bytes"),
    ] {
        let vt = value_type(&mut strings, ty, unit);
        bytes(&mut out, 1, &vt);
    }

    let mut mappings = maps::mappings();
    mappings.sort_by_key(|m| m.start);
    let mapping_of = |addr: u64| -> u64 {
        let i = mappings.partition_point(|m| m.start <= addr);
        if i > 0 && addr < mappings[i - 1].limit {
            i as u64
        } else {
            0
        }
    };

    let mut symbols: HashMap<usize, Option<Vec<Line>>> = HashMap::new();
    let mut locations = Locations::default();
    // Whether every location in each mapping got symbols; the symbolizer
    // misses libraries loaded after its first use, and those are left for
    // pprof to symbolize
    let mut symbolized = vec![cfg!(feature = "symbolize"); mappings.len() + 1];

    for s in stacks {
        if s.alloc_objects == 0 {
            continue;
        }
        let mut ids = Vec::with_capacity(s.frames.len());
        for &addr in s.frames {
            let Some(lines) = symbols.entry(addr).or_insert_with(|| user_lines(addr)) else {
                // The allocator's frames are only at the top of a stack
                continue;
            };
            let mapping = mapping_of(addr as u64);
            if lines.is_empty() {
                symbolized[mapping as usize] = false;
            }
            ids.push(locations.id(addr, lines, mapping, &mut strings));
        }
        let mut sample = Vec::new();
        packed(&mut sample, 1, ids);
        packed(
            &mut sample,
            2,
            [
                s.alloc_objects,
                s.alloc_bytes,
                s.inuse_objects,
                s.inuse_bytes,
            ],
        );
        bytes(&mut out, 2, &sample);
    }

    for (i, m) in mappings.iter().enumerate() {
        let complete = u64::from(symbolized[i + 1]);
        out.extend(encode_mapping(&mut strings, i as u64 + 1, m, complete));
    }
    out.extend(locations.encoded);
    out.extend(locations.functions_encoded);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    uint(&mut out, 9, now);
    let period_type = value_type(&mut strings, "space", "bytes");
    bytes(&mut out, 11, &period_type);
    uint(&mut out, 12, interval as u64);
    let default = strings.id("inuse_space");
    uint(&mut out, 14, default);

    for s in &strings.list {
        bytes(&mut out, 6, s.as_bytes());
    }
    out
}

/// Locations and the functions their lines name, each encoded once
#[derive(Default)]
struct Locations {
    ids: HashMap<usize, u64>,
    /// By full name and file: simplified names of different functions,
    /// such as two traits' methods on one type, can be the same
    functions: HashMap<(String, String), u64>,
    encoded: Vec<u8>,
    functions_encoded: Vec<u8>,
}

impl Locations {
    /// The id of the location at `addr`, encoding it on first use
    fn id(&mut self, addr: usize, lines: &[Line], mapping: u64, strings: &mut Strings) -> u64 {
        if let Some(&id) = self.ids.get(&addr) {
            return id;
        }
        let id = self.ids.len() as u64 + 1;
        self.ids.insert(addr, id);
        let mut loc = Vec::new();
        uint(&mut loc, 1, id);
        uint(&mut loc, 2, mapping);
        uint(&mut loc, 3, addr as u64);
        for line in lines {
            let mut l = Vec::new();
            uint(&mut l, 1, self.function(line, strings));
            uint(&mut l, 2, line.number);
            bytes(&mut loc, 4, &l);
        }
        bytes(&mut self.encoded, 4, &loc);
        id
    }

    fn function(&mut self, line: &Line, strings: &mut Strings) -> u64 {
        let key = (line.system_name.clone(), line.file.clone());
        if let Some(&id) = self.functions.get(&key) {
            return id;
        }
        let id = self.functions.len() as u64 + 1;
        let mut f = Vec::new();
        uint(&mut f, 1, id);
        uint(&mut f, 2, strings.id(&line.name));
        uint(&mut f, 3, strings.id(&line.system_name));
        uint(&mut f, 4, strings.id(&line.file));
        bytes(&mut self.functions_encoded, 5, &f);
        self.functions.insert(key, id);
        id
    }
}

/// A mapping; `symbolized` says whether its locations carry their
/// functions and lines already
fn encode_mapping(strings: &mut Strings, id: u64, m: &Mapping, symbolized: u64) -> Vec<u8> {
    let mut b = Vec::new();
    uint(&mut b, 1, id);
    uint(&mut b, 2, m.start);
    uint(&mut b, 3, m.limit);
    uint(&mut b, 4, m.offset);
    uint(&mut b, 5, strings.id(&m.path));
    for field in 7..=10 {
        uint(&mut b, field, symbolized);
    }
    let mut out = Vec::new();
    bytes(&mut out, 3, &b);
    out
}

#[cfg(all(test, feature = "symbolize"))]
mod tests {
    use super::{Line, is_internal, simplify};

    fn line(name: &str, file: &str) -> Line {
        Line {
            name: name.to_string(),
            system_name: name.to_string(),
            file: file.to_string(),
            number: 1,
        }
    }

    /// With line tables only, names come without paths: the allocator's own
    /// frames are still told by their files
    #[test]
    fn internal_frames_without_paths() {
        let own = format!("{}/src/malloc.rs", env!("CARGO_MANIFEST_DIR"));
        assert!(is_internal(&line("alloc", &own)));
        assert!(is_internal(&line("stoolap_jemalloc::malloc::alloc", "")));
        assert!(is_internal(&line("__rust_alloc", "/app/src/lib.rs")));
        assert!(!is_internal(&line(
            "alloc",
            "/rustlib/src/rust/library/alloc/src/alloc.rs"
        )));
        assert!(!is_internal(&line(
            "add_versions_batch",
            "/app/src/storage/mvcc.rs"
        )));
    }

    #[test]
    fn simplifies_rust_names() {
        let cases = [
            ("alloc::raw_vec::finish_grow", "alloc::raw_vec::finish_grow"),
            (
                "<alloc::raw_vec::RawVec<T,A>>::finish_grow",
                "alloc::raw_vec::RawVec::finish_grow",
            ),
            (
                "<core::result::Result<T, E> as core::ops::function::FnOnce<A>>::call_once",
                "core::result::Result::call_once",
            ),
            ("<&T as core::fmt::Display>::fmt", "&T::fmt"),
            (
                "std::thread::Builder::spawn::{{closure}}",
                "std::thread::Builder::spawn::{{closure}}",
            ),
            ("foo::<impl Fn() -> u8>::bar", "foo::impl Fn() -> u8::bar"),
            (
                "RawVecInner::reserve::do_reserve_and_handle::<alloc::alloc::Global>",
                "RawVecInner::reserve::do_reserve_and_handle",
            ),
            (
                "core::ptr::drop_in_place::<alloc::vec::Vec<u8>>",
                "core::ptr::drop_in_place",
            ),
        ];
        for (name, want) in cases {
            assert_eq!(simplify(name), want, "{name}");
        }
    }
}
