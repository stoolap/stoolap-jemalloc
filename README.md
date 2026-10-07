<div align="center">
  <h1>stoolap-jemalloc</h1>

  <h3>A jemalloc-Style Memory Allocator in Pure Rust, with a pprof Heap Profiler</h3>

  <p>
    <a href="#quick-start">Quick start</a> •
    <a href="#heap-profiling">Heap profiling</a> •
    <a href="#design">Design</a> •
    <a href="#benchmarks">Benchmarks</a>
  </p>

  <p>
    <a href="https://github.com/stoolap/stoolap-jemalloc/actions/workflows/ci.yml"><img src="https://github.com/stoolap/stoolap-jemalloc/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
    <a href="https://crates.io/crates/stoolap-jemalloc"><img src="https://img.shields.io/crates/v/stoolap-jemalloc.svg" alt="Crates.io"></a>
    <a href="https://docs.rs/stoolap-jemalloc"><img src="https://docs.rs/stoolap-jemalloc/badge.svg" alt="docs.rs"></a>
    <a href="https://codecov.io/gh/stoolap/stoolap-jemalloc"><img src="https://codecov.io/gh/stoolap/stoolap-jemalloc/branch/main/graph/badge.svg" alt="codecov"></a>
    <a href="https://github.com/stoolap/stoolap-jemalloc/actions/workflows/audit.yml"><img src="https://github.com/stoolap/stoolap-jemalloc/actions/workflows/audit.yml/badge.svg" alt="Security Audit"></a>
    <a href="Cargo.toml"><img src="https://img.shields.io/badge/MSRV-1.88-orange.svg" alt="MSRV 1.88"></a>
    <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache%202.0-blue.svg" alt="License"></a>
  </p>
</div>

---

stoolap-jemalloc is a fast, memory-frugal, jemalloc-style allocator written
in pure Rust, with a built-in sampling heap profiler that writes [pprof]
profiles.

- **Pure Rust.** No C code is compiled. The crate binds only to the OS:
  `libc` on unix and `windows-sys` on Windows.
- **Fast.** Thread caches serve most allocations without locks. Frees use
  the size from the `Layout`, so they read no metadata.
- **Small.** Size classes are tuned for Rust types. Empty memory is shared
  between arenas and returned to the OS.
- **Profiling built in.** Heap profiles with stacks, symbols and inlined
  frames, readable by `go tool pprof`, with no startup options.

[pprof]: https://github.com/google/pprof

## Contents

- [Quick start](#quick-start)
- [Heap profiling](#heap-profiling)
- [Statistics and purging](#statistics-and-purging)
- [Background purging](#background-purging)
- [Platforms](#platforms)
- [Design](#design)
- [Benchmarks](#benchmarks)
- [Development](#development)
- [Limitations](#limitations)
- [License](#license)

## Quick start

```sh
cargo add stoolap-jemalloc
```

Or in `Cargo.toml`:

```toml
[dependencies]
stoolap-jemalloc = "0.1"
```

Then install it as the global allocator:

```rust
use stoolap_jemalloc::Jemalloc;

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc::new();
```

## Heap profiling

To profile the whole process, turn profiling on in the allocator itself.
It then samples from the process's first allocation, as jemalloc's
`prof:true,prof_active:true` options do:

```rust
use stoolap_jemalloc::{Jemalloc, prof};

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc::new().with_profiling();

fn main() -> Result<(), prof::DumpError> {
    // ... the program runs ...
    prof::write_pprof("heap.pb")?;
    Ok(())
}
```

Or switch it on at run time:

```rust
use stoolap_jemalloc::prof;

prof::set_sample_interval(512 * 1024); // optional: 512 KiB is the default
prof::activate();

// ... the program runs ...

prof::write_pprof("heap.pb")?;         // or prof::dump_pprof() -> Vec<u8>
```

```sh
go tool pprof -http=: heap.pb                       # flame graph in the browser
go tool pprof -top heap.pb                          # live memory by function
go tool pprof -sample_index=alloc_space -top heap.pb
```

### What the profile holds

| Sample type     | Meaning                                     |
|-----------------|---------------------------------------------|
| `inuse_space`   | Bytes allocated and not yet freed (default) |
| `inuse_objects` | Allocations not yet freed                   |
| `alloc_space`   | Bytes allocated since activation            |
| `alloc_objects` | Allocations since activation                |

Each thread takes a sample after a random number of allocated bytes,
exponentially distributed around the sample interval, as jemalloc does.
Counts are scaled by the inverse of the sampling probability, so they are
unbiased estimates of the real totals. As in jemalloc, an allocation's
bytes are those of its size class: what it really takes from the heap.
Weights are kept with fractions and rounded only when a profile is
written.

With the default `symbolize` feature, the profile includes function names,
files, line numbers and inlined frames, and the allocator's own frames are
left out of the stacks. Functions are told apart by their full names and
files, while pprof shows simplified names. Without the feature, the
profile holds raw addresses and the process's mappings, and `pprof`
symbolizes them from the binary. The same goes for a library that the
in-process symbolizer does not know, such as one loaded after the first
dump: its mapping is marked as not symbolized.

### API

| Function                        | Purpose                              |
|---------------------------------|--------------------------------------|
| `Jemalloc::new().with_profiling()` | An allocator that samples from the first allocation |
| `prof::activate()`              | Start sampling                       |
| `prof::deactivate()`            | Stop sampling; live samples stay     |
| `prof::is_active()`             | Whether sampling is on               |
| `prof::set_sample_interval(n)`  | Mean bytes between samples           |
| `prof::sample_interval()`       | The current interval                 |
| `prof::dump_pprof()`            | The profile as protobuf bytes        |
| `prof::write_pprof(path)`       | Write the profile; returns its size  |

`dump_pprof` and `write_pprof` fail with `DumpError::NotActivated` if
profiling was never activated, and `write_pprof` with `DumpError::Io` if
the file cannot be written.

Notes:

- With `prof::activate()`, allocations made before the call are not in
  the profile; `with_profiling()` leaves none out.
- Threads created while profiling is active sample from their first
  allocation. Threads that existed before notice activation within 1 MiB
  of their own allocations. Until then, the allocation fast path carries
  no profiling check.
- A change of the sample interval reaches each thread within 64 KiB of
  its allocations. A sample is weighed with the interval its wait was
  drawn with, so counts stay right across the change.
- Dumping is safe while other threads load and unload libraries: on macOS
  the loaded images are read through `vm_read_overwrite`, which fails
  instead of crashing when an image goes away.
- Profiles are written uncompressed. `pprof` reads plain and gzipped files
  alike.

## Statistics and purging

```rust
let s = stoolap_jemalloc::stats();
println!("mapped {} bytes", s.mapped());

stoolap_jemalloc::purge(); // return free memory to the OS now
```

`Stats` reports the memory held from the OS:

| Field         | Memory                                          |
|---------------|-------------------------------------------------|
| `chunks`      | Chunks for small and large allocations          |
| `huge`        | Mappings of huge allocations in use             |
| `huge_cached` | Freed huge mappings kept for reuse              |
| `metadata`    | The allocator's own metadata, profiler included |

`mapped()` is the sum of the four.

## Background purging

Free pages go back to the OS as the allocator is used. A process that
stops allocating keeps them, unless it starts the background thread:

```rust
stoolap_jemalloc::background::start(); // once, at startup
```

The thread ends a decay epoch every 5 seconds, so pages that stay free
for 5 to 10 seconds go back to the OS, and empty chunks are unmapped, even
while the process allocates nothing. In a test, 120 MiB freed by an idle
process stayed mapped for 30 seconds without the thread, and went back
within about 10 seconds with it.

As in jemalloc, the thread is off by default: a library that starts
threads on its own surprises sandboxes and programs that count or fork
their threads. `background::is_running()` tells whether it runs. A child
process after `fork` has no such thread and may start its own.

## Platforms

All of these run the test suite, in release and debug builds:

| Platform              | Where                        |
|-----------------------|------------------------------|
| Linux, x86_64         | CI                           |
| Linux, x86 (32-bit)   | CI, in a 32-bit container    |
| Linux, aarch64        | Locally, in Docker           |
| macOS, Apple silicon  | CI and locally               |
| Windows, x86_64       | CI                           |

## Design

The allocator follows jemalloc's architecture, simplified where Rust
allows it.

### Size classes

- 8-byte steps up to 128 bytes. Rust does not need malloc's 16-byte
  minimum alignment, so a 24-byte object takes 24 bytes.
- Eight classes per doubling from 128 bytes to 1 KiB, where Rust structs
  and B-tree nodes cluster. Padding stays under 12.5% there.
- Four classes per doubling above 1 KiB, as in jemalloc. Padding stays
  under 25% there.

| Kind  | Sizes        | Served from                               |
|-------|--------------|-------------------------------------------|
| Small | up to 14 KiB | Slabs: page runs split into equal objects |
| Large | up to 1 MiB  | Page runs                                 |
| Huge  | above 1 MiB  | A mapping of their own                    |

### Thread caches

Each thread keeps a stack of free objects for every class up to 32 KiB.
A stack of a small class holds up to 200 objects or 16 KiB, and at least
8 objects; a stack of a larger class holds 4.

- Allocations and frees on a cache hit touch no lock and no shared memory.
- Full caches flush their oldest half to the arenas in one batch.
- A collector gradually returns objects that a thread stopped using.

### Arenas and chunks

- **Arenas.** There are up to 4 arenas per CPU, and a new thread takes the
  arena with the fewest threads. Each small class has its own lock in each
  arena.
- **Chunks.** Arenas carve slabs and large runs out of 4 MiB chunks. Every
  pointer finds its chunk header by masking `ptr - 1`, so no global lookup
  structure is needed.
- **Free runs.** Free runs are binned by length, so finding one is a bitmap
  scan whatever the heap size. Freed runs merge with their neighbours.
- **In-place `realloc`.** Large runs grow and shrink in place when they can.
- **Sharing.** A chunk that becomes empty goes to a global spare pool that
  any arena can take from. When the last thread of an arena exits, the
  arena frees what it was keeping for reuse.

### Returning memory to the OS

- **Decay.** Pages that stay free for 5–10 seconds go back to the OS:
  `MADV_DONTNEED` on Linux, `MADV_FREE` on macOS and `MEM_RESET` on
  Windows. Every arena is covered, including those whose threads have
  exited. Spare chunks are unmapped on the same schedule. Where an OS page
  holds several 4 KiB allocator pages, as the 16 KiB pages of Apple
  silicon do, it goes back once all of them are free.
- **Huge mappings.** Freed huge mappings are unmapped, except a few that
  are kept for reuse: up to 64 MiB, for 10 to 15 seconds.
- **No transparent huge pages** for chunks on Linux. A 2 MiB huge page
  stays resident while any 4 KiB of it is in use. With huge pages, a
  multi-threaded `Vec` growth benchmark also ran over twice as slow.

### Memory safety

The allocator is checked with Miri under both Stacked Borrows and Tree
Borrows, including runs where threads work on the same chunk at once.

- **No references span shared metadata.** Chunk headers are only reached
  through raw pointers, field by field, since threads holding different
  locks work on different runs of the same chunk.
- **Thread caches do not alias their slots.** The slots live beside the
  cache, outside any `&mut` to it.
- **Provenance.** Chunks and huge mappings expose their provenance when
  they are made. Pointers coming back from users carry only the
  provenance of their allocation, so the allocator derives its own
  pointers from the mapping's. It exposes the user's provenance as well:
  a caller may still hold a protected `Box` to memory it frees, even
  through an allocator that wraps this one, and the allocator's writes,
  such as free-list links, then act under that permission instead of
  invalidating it.

### Fork safety

A child process starts with only the thread that forked. A lock another
thread held at the fork, or a structure it was changing, would stay so in
the child. Handlers registered with `pthread_atfork` take every allocator
lock before the fork, in the order the allocator nests them, and release
them after it in both processes. In the child, the arenas also forget the
threads that did not come along.

### Profiler internals

- Stacks are captured without allocating: the system unwinder on unix and
  `RtlCaptureStackBackTrace` on Windows.
- Identical stacks are counted together in one table.
- Sampled allocations live in chunks of their own, so a free can tell
  whether it frees a sample from the chunk header alone. Until profiling is
  first activated, frees check a single global flag; after that, they also
  read the chunk header.

## Benchmarks

`bench/` compares this allocator with mimalloc, jemalloc (C, through
`tikv-jemallocator`) and the system allocator. It is a separate crate, so
the C allocators never become dependencies of this one.

```sh
cd bench
cargo run --release --features ours         # or mimalloc, jemalloc, or none
cargo run --release --features ours mixed   # only benchmarks named mixed*
ONCE=1 cargo run --release --features ours  # one run instead of the best of 5
STATS=1 cargo run --release --features ours # allocator counters after each run
```

| Benchmark           | Workload                                                      |
|---------------------|---------------------------------------------------------------|
| `small_churn`       | Allocate and free 64 small boxes, last in first out           |
| `mixed`             | 4000 live allocations of mixed sizes, freed in random order   |
| `strings_tree`      | A `BTreeMap<String, String>` of 300,000 entries, half removed |
| `vec_growth`        | Grow a `Vec<u32>` to 100,000 elements                         |
| `producer_consumer` | Objects allocated on one thread and freed on another          |

`1t` runs on one thread; `Nt` runs on one thread per CPU.

### Time

Lower is better; ms.

macOS, Apple M5 with 10 cores (best of 4 runs):

| Benchmark         | stoolap-jemalloc | mimalloc | jemalloc (C) | system |
|-------------------|-----------------:|---------:|-------------:|-------:|
| small_churn 1t    | **23.7**         | 30.1     | 42.9         | 72.0   |
| small_churn Nt    | 42.3             | **42.1** | 65.3         | 116.5  |
| mixed 1t          | 32.4             | **30.8** | 36.5         | 57.1   |
| mixed Nt          | **80.6**         | 84.8     | 82.8         | 182.2  |
| strings_tree 1t   | **56.0**         | 56.2     | 68.3         | 64.8   |
| strings_tree Nt   | 100.5            | **96.8** | 116.3        | 138.8  |
| vec_growth Nt     | **12.3**         | 18.9     | 13.3         | 16.2   |
| producer_consumer | 9.8              | 10.2     | **8.2**      | 11.9   |

Linux aarch64 in a Docker VM with 8 CPUs (median of 5 single runs):

| Benchmark         | stoolap-jemalloc | mimalloc | jemalloc (C) | system |
|-------------------|-----------------:|---------:|-------------:|-------:|
| small_churn 1t    | **40.0**         | 45.9     | 41.4         | 55.7   |
| small_churn Nt    | **40.3**         | 50.9     | **40.3**     | 96.1   |
| mixed 1t          | 38.8             | 36.2     | **35.7**     | 62.6   |
| mixed Nt          | 126.4            | 142.4    | **122.4**    | 129.9  |
| strings_tree 1t   | 64.5             | **64.4** | 72.6         | 76.4   |
| strings_tree Nt   | 100.2            | **94.6** | 103.1        | 116.8  |
| vec_growth Nt     | **11.3**         | 35.5     | 12.4         | 45.5   |
| producer_consumer | 11.6             | 25.2     | **9.0**      | 44.6   |

### Memory

Peak RSS of one run; lower is better; MB.

| Benchmark              | stoolap-jemalloc | mimalloc | jemalloc (C) | system |
|------------------------|-----------------:|---------:|-------------:|-------:|
| strings_tree Nt, macOS | **369**          | 403      | 405          | 393    |
| strings_tree Nt, Linux | **309**          | 326      | 350          | 367    |
| mixed Nt, macOS        | **96**           | 203      | 129          | 104    |
| mixed Nt, Linux        | 96               | 226      | 199          | **73** |

Results depend on the machine and the workload, so measure with your own.
The Linux numbers come from a virtual machine, where page table walks cost
more than on bare metal.

## Development

Tests run the allocator as the global allocator of each test binary:

```sh
cargo test --release
cargo test --release --no-default-features   # without symbolization
```

The tests cover:

- sizes from 0 bytes to 12 MiB, with alignments up to 16 MiB
- sizes too large to allocate, which fail without panicking, on 64-bit
  and 32-bit targets
- `realloc` across every kind of allocation
- frees from other threads, and many short-lived threads
- random multi-threaded stress
- allocations made while a thread exits
- forks while other threads allocate, purge and profile, with the
  background thread running
- the background thread returning memory of an idle process
- statistics, purging and the expiry of cached huge mappings
- decay on systems whose OS pages hold several allocator pages
- the profiler, with every allocation sampled
- profiling on from the first allocation, with `with_profiling()`
- the profiler's estimates against known allocation counts, across
  changes of the sample interval and over many short-lived threads
- functions whose simplified names collide
- libraries loaded after the first dump
- profile dumps while another thread loads and unloads a library

Miri runs a smaller set of workloads against Rust's aliasing and data
race rules, including an allocator that wraps this one and, right after a
free, purges or takes a huge mapping back out of the cache:

```sh
export MIRIFLAGS="-Zmiri-ignore-leaks -Zmiri-permissive-provenance"
cargo +nightly miri test --test miri --test miri_wrapper
MIRIFLAGS="$MIRIFLAGS -Zmiri-tree-borrows" cargo +nightly miri test --test miri --test miri_wrapper
```

Line coverage is about 97%; the remaining lines are mostly out-of-memory
paths:

```sh
cargo llvm-cov --release --summary-only
```

Lints run with warnings as errors. Clippy's pedantic group is on in
`Cargo.toml`, except the cast lints that bit-level allocator code trips on
purpose.

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --no-default-features -- -D warnings
cargo clippy --all-targets --target x86_64-pc-windows-gnu -- -D warnings
cargo clippy --all-targets --target i686-unknown-linux-gnu -- -D warnings
# Builds the tests too, without linking, which finds what check does not
CARGO_TARGET_I686_UNKNOWN_LINUX_GNU_LINKER=true cargo build --all-targets --target i686-unknown-linux-gnu
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```

### Continuous integration

`.github/workflows/ci.yml` runs on every push and pull request:

| Job          | What it checks                                                        |
|--------------|-----------------------------------------------------------------------|
| Lint         | Formatting, clippy (also for Windows and 32-bit Linux), docs         |
| MSRV         | Builds with Rust 1.88                                                 |
| Test         | Tests on Linux, macOS and Windows, with and without `symbolize`, in release and debug |
| Test (32-bit Linux) | Tests in a 32-bit container                                    |
| Miri         | The Miri tests under Stacked Borrows and Tree Borrows                 |
| Coverage     | Line coverage, uploaded to Codecov                                    |
| License      | The license header in every Rust file                                 |

`.github/workflows/audit.yml` runs `cargo audit` daily and on dependency
changes.

## Limitations

- **Thread caches of other threads are lost to a child after `fork`**, as
  in jemalloc. A full cache holds up to about 1.7 MiB.
- **Idle thread caches.** A thread that stops allocating keeps the objects
  in its cache, up to about 1.7 MiB when every size class is full. Only the
  thread itself may touch its cache, so the background thread cannot trim
  it.
- **Sampled allocations take at least one 4 KiB page**, as in jemalloc.
- **Stacks are code addresses, symbolized when a profile is written.** A
  sample whose stack went through a library that was unloaded since, with
  another loaded at its addresses, is shown in the new library, as in
  jemalloc's profiles.
- **A dump's own allocations are sampled too**, so a profile written
  after another may show what the first one still holds.

## License

Apache-2.0; see [LICENSE](LICENSE). Every source file carries the
license header, which CI checks.
