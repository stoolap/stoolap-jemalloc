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

//! `fork` handlers. A child starts with only the forking thread: a lock
//! that another thread held at the fork, or a structure it was changing,
//! would stay that way in the child. So every allocator lock is taken
//! before `fork`, in the order the allocator nests them, and released
//! after it in both processes.

use crate::{arena, base, huge, prof, tcache};
use core::sync::atomic::{AtomicBool, Ordering};

/// Registers the handlers once; called when the first thread cache is
/// made, which comes before any fork
pub(crate) fn register() {
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::Relaxed) {
        return;
    }
    unsafe {
        libc::pthread_atfork(Some(prepare), Some(parent), Some(child));
    }
}

unsafe extern "C" fn prepare() {
    prof::fork_lock_dump();
    arena::fork_lock();
    huge::fork_lock();
    prof::fork_lock();
    tcache::fork_lock();
    base::fork_lock();
}

unsafe fn unlock_all() {
    unsafe {
        base::fork_unlock();
        tcache::fork_unlock();
        prof::fork_unlock();
        huge::fork_unlock();
        arena::fork_unlock();
        prof::fork_unlock_dump();
    }
}

unsafe extern "C" fn parent() {
    unsafe { unlock_all() };
}

unsafe extern "C" fn child() {
    unsafe {
        unlock_all();
        let t = tcache::current();
        arena::fork_child(if t.is_null() {
            core::ptr::null_mut()
        } else {
            (*t).arena
        });
    }
    crate::background::fork_child();
}
