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
use core::sync::atomic::{AtomicU64, Ordering};

/// Registration: 0 before it, `DONE` after it, and the registering
/// process's id in between
static STATE: AtomicU64 = AtomicU64::new(0);
const DONE: u64 = u64::MAX;

/// Registers the handlers once; called whenever a thread cache is made, so
/// before any thread of the allocator's can fork. Threads that come while
/// another registers wait for it: a fork in between would run without the
/// handlers.
pub(crate) fn register() {
    register_with(|| {});
}

/// `registered` runs between the registration and its record, where tests
/// fork
#[inline(always)]
fn register_with(registered: impl FnOnce()) {
    if STATE.load(Ordering::Acquire) == DONE {
        return;
    }
    let me = u64::from(std::process::id());
    let mut waiting_since = None;
    loop {
        match STATE.load(Ordering::Acquire) {
            DONE => return,
            // Forked while the parent registered: the registering thread
            // is not in this process, so this one registers instead
            s if s == 0 || s != me => {
                if STATE
                    .compare_exchange(s, me, Ordering::Acquire, Ordering::Acquire)
                    .is_ok()
                {
                    let ok =
                        unsafe { libc::pthread_atfork(Some(prepare), Some(parent), Some(child)) }
                            == 0;
                    if ok {
                        registered();
                    }
                    // Without handlers the next thread cache tries again
                    STATE.store(if ok { DONE } else { 0 }, Ordering::Release);
                    return;
                }
            }
            // Another thread of this process registers. One that does
            // for over a second is gone: a fork came in its window and
            // this process got the forking parent's id after it exited.
            // Its claim is dropped, and the loop registers again.
            _ => {
                let since = *waiting_since.get_or_insert_with(std::time::Instant::now);
                if since.elapsed() > std::time::Duration::from_secs(1) {
                    let _ = STATE.compare_exchange(me, 0, Ordering::AcqRel, Ordering::Acquire);
                }
                std::thread::yield_now();
            }
        }
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
    // The handlers ran, so they are registered in this process too, though
    // the fork may have come before the parent recorded it
    STATE.store(DONE, Ordering::Release);
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

#[cfg(test)]
mod tests {
    use super::{STATE, register, register_with};
    use core::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    /// Exits with 0 if `pid` exits with 0 within 10 seconds; kills it and
    /// exits with 2 if not
    fn wait(pid: libc::pid_t) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut status = 0;
        while unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) } != pid {
            if Instant::now() > deadline {
                unsafe { libc::kill(pid, libc::SIGKILL) };
                unsafe { libc::waitpid(pid, &raw mut status, 0) };
                return 2;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else {
            3
        }
    }

    /// A claim left by a registering thread that is in no process any
    /// more, with this process's id, is taken over after a second
    #[test]
    fn stale_claim_is_taken_over() {
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            STATE.store(u64::from(std::process::id()), Ordering::Release);
            register();
            let done = STATE.load(Ordering::Acquire) == super::DONE;
            unsafe { libc::_exit(i32::from(!done)) };
        }
        assert_eq!(wait(pid), 0, "register() waited on the stale claim");
    }

    /// A fork after the handlers were registered but before that was
    /// recorded: the child must not register them again, or its next fork
    /// would run them twice and wait on a lock that the first run holds
    #[test]
    fn fork_between_registration_and_its_record() {
        assert_eq!(STATE.load(Ordering::Acquire), 0, "nothing registered yet");
        let mut code = -1;
        register_with(|| {
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0);
            if pid == 0 {
                // A new thread cache in the child registers again
                register();
                let pid = unsafe { libc::fork() };
                if pid == 0 {
                    unsafe { libc::_exit(0) };
                }
                unsafe { libc::_exit(wait(pid)) };
            }
            code = wait(pid);
        });
        assert_eq!(code, 0, "the child's next fork did not finish");
    }
}
