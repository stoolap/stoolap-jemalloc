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

//! Stack capture that does not allocate: the system unwinder on unix,
//! `RtlCaptureStackBackTrace` on Windows. Frames are return addresses minus
//! one, so that they point into the calling instruction.

#[cfg(all(unix, not(target_arch = "arm")))]
#[inline(never)]
pub fn capture(frames: &mut [usize], skip: usize) -> usize {
    use core::ffi::{c_int, c_void};

    // _Unwind_Reason_Code values
    const NO_REASON: c_int = 0;
    const END_OF_STACK: c_int = 5;

    unsafe extern "C" {
        fn _Unwind_Backtrace(
            trace: extern "C" fn(*mut c_void, *mut c_void) -> c_int,
            arg: *mut c_void,
        ) -> c_int;
        fn _Unwind_GetIP(ctx: *mut c_void) -> usize;
    }

    struct State<'a> {
        frames: &'a mut [usize],
        len: usize,
        skip: usize,
    }

    extern "C" fn step(ctx: *mut c_void, arg: *mut c_void) -> c_int {
        let st = unsafe { &mut *(arg as *mut State) };
        let ip = unsafe { _Unwind_GetIP(ctx) };
        if ip == 0 {
            return END_OF_STACK;
        }
        if st.skip > 0 {
            st.skip -= 1;
            return NO_REASON;
        }
        st.frames[st.len] = ip - 1;
        st.len += 1;
        if st.len == st.frames.len() {
            END_OF_STACK
        } else {
            NO_REASON
        }
    }

    // The first frame is this function's own
    let mut st = State {
        frames,
        len: 0,
        skip: skip + 1,
    };
    unsafe { _Unwind_Backtrace(step, &raw mut st as *mut c_void) };
    st.len
}

#[cfg(windows)]
#[inline(never)]
pub fn capture(frames: &mut [usize], skip: usize) -> usize {
    use windows_sys::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace;
    let max = frames.len().min(u16::MAX as usize) as u32;
    // Skip this function too
    let n = unsafe {
        RtlCaptureStackBackTrace(
            (skip + 1) as u32,
            max,
            frames.as_mut_ptr() as *mut *mut core::ffi::c_void,
            core::ptr::null_mut(),
        )
    } as usize;
    for f in &mut frames[..n] {
        *f -= 1;
    }
    n
}

#[cfg(not(any(all(unix, not(target_arch = "arm")), windows)))]
pub fn capture(_frames: &mut [usize], _skip: usize) -> usize {
    0
}
