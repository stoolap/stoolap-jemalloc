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

//! Forking while another thread dumps, in a binary of its own
#![cfg(all(unix, feature = "symbolize"))]

use std::hint::black_box;
use std::thread;
use std::time::{Duration, Instant};
use stoolap_jemalloc::{Jemalloc, prof};

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc::new();

/// A dump takes the symbolizer's lock, which a child forked mid-dump would
/// inherit held. The fork must wait for the dump, so that the child's own
/// dump can take the lock.
#[test]
fn fork_while_a_dump_runs() {
    prof::set_sample_interval(4096);
    prof::activate();
    drop(black_box(vec![0u8; 2 << 20]));
    for round in 0..3 {
        // Each dump parses the debug information again, which takes far
        // longer than the pause before the fork
        let dumper = thread::spawn(|| black_box(prof::dump_pprof().unwrap().len()));
        thread::sleep(Duration::from_millis(3));
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            let ok = prof::dump_pprof().is_ok();
            unsafe { libc::_exit(i32::from(!ok)) }
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut status = 0;
        while unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) } != pid {
            if Instant::now() > deadline {
                unsafe { libc::kill(pid, libc::SIGKILL) };
                unsafe { libc::waitpid(pid, &raw mut status, 0) };
                panic!("child {round} deadlocked in its dump");
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "child {round} failed: {status}"
        );
        dumper.join().unwrap();
    }
}
