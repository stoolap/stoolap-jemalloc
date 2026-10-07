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

//! A thread that returns free memory to the OS on time. Without it, decay
//! runs as the allocator is used, so a process that stops allocating keeps
//! its free pages. It is started explicitly, as in jemalloc, where
//! background threads are off by default: a library that starts threads
//! on its own surprises sandboxes, thread counts and forking programs.

use crate::arena;
use core::sync::atomic::{AtomicBool, Ordering};
use std::thread;

static RUNNING: AtomicBool = AtomicBool::new(false);

/// Starts the background thread, which ends a decay epoch every 5 seconds
/// in all arenas, so that pages free for 5 to 10 seconds go back to the OS
/// even while the process allocates nothing. Returns whether this call
/// started it; it runs until the process exits.
///
/// A child process after `fork` has no such thread and may call this
/// again.
///
/// # Panics
///
/// When the OS cannot create a thread.
pub fn start() -> bool {
    if RUNNING.swap(true, Ordering::AcqRel) {
        return false;
    }
    let spawned = thread::Builder::new()
        .name("stoolap-jemalloc-purge".into())
        .spawn(|| {
            loop {
                thread::sleep(arena::DECAY_EPOCH);
                unsafe { arena::decay_tick() };
            }
        });
    if let Err(e) = spawned {
        // Not running, so that a later call may try again
        RUNNING.store(false, Ordering::Release);
        panic!("spawning the background purge thread: {e}");
    }
    true
}

/// Whether the background thread runs in this process
pub fn is_running() -> bool {
    RUNNING.load(Ordering::Acquire)
}

#[cfg(all(unix, not(miri)))]
/// In a child after `fork`: the parent's thread did not come along
pub(crate) fn fork_child() {
    RUNNING.store(false, Ordering::Release);
}
