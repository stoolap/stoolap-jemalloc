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

//! The process's executable mappings, so that pprof can symbolize
//! addresses against the binaries on disk

pub struct Mapping {
    pub start: u64,
    pub limit: u64,
    pub offset: u64,
    pub path: String,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn mappings() -> Vec<Mapping> {
    let Ok(maps) = std::fs::read_to_string("/proc/self/maps") else {
        return Vec::new();
    };
    maps.lines().filter_map(parse_maps_line).collect()
}

/// An executable, file-backed line of `/proc/self/maps`:
/// `start-end perms offset dev inode path`, where the path runs to the end
/// of the line and may hold spaces
#[cfg(any(target_os = "linux", target_os = "android", test))]
fn parse_maps_line(line: &str) -> Option<Mapping> {
    let mut rest = line;
    let mut fields = [""; 5];
    for field in &mut fields {
        rest = rest.trim_start();
        let end = rest.find(' ').unwrap_or(rest.len());
        *field = &rest[..end];
        rest = &rest[end..];
    }
    let [range, perms, offset, _dev, _inode] = fields;
    let path = rest.trim_start();
    if !perms.contains('x') || path.is_empty() || path.starts_with('[') {
        return None;
    }
    let (start, end) = range.split_once('-')?;
    let hex = |s: &str| u64::from_str_radix(s, 16).ok();
    Some(Mapping {
        start: hex(start)?,
        limit: hex(end)?,
        offset: hex(offset)?,
        path: path.to_string(),
    })
}

/// The images that dyld has loaded. Another thread may unload one at any
/// time, so its header and name are copied with `vm_read_overwrite`, which
/// fails on unmapped memory where a plain read would crash.
#[cfg(target_vendor = "apple")]
pub fn mappings() -> Vec<Mapping> {
    use core::ffi::c_char;

    unsafe extern "C" {
        fn _dyld_image_count() -> u32;
        fn _dyld_get_image_header(i: u32) -> *const u8;
        fn _dyld_get_image_vmaddr_slide(i: u32) -> isize;
        fn _dyld_get_image_name(i: u32) -> *const c_char;
    }

    const MH_MAGIC_64: u32 = 0xfeed_facf;
    const LC_SEGMENT_64: u32 = 0x19;
    const HEADER: usize = 32;

    let read_u32 = |b: &[u8], at: usize| {
        b.get(at..at + 4)
            .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
    };
    let read_u64 = |b: &[u8], at: usize| {
        b.get(at..at + 8)
            .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
    };

    let mut out = Vec::new();
    for i in 0..unsafe { _dyld_image_count() } {
        let (header, name, slide) = unsafe {
            (
                _dyld_get_image_header(i),
                _dyld_get_image_name(i),
                _dyld_get_image_vmaddr_slide(i) as u64,
            )
        };
        if header.is_null() || name.is_null() {
            continue;
        }
        let mut head = [0u8; HEADER];
        if !copy_from_process(header.addr(), &mut head) || read_u32(&head, 0) != Some(MH_MAGIC_64) {
            continue;
        }
        let ncmds = read_u32(&head, 16).unwrap_or(0);
        let size = read_u32(&head, 20).unwrap_or(0) as usize;
        let mut cmds = vec![0u8; size.min(1 << 20)];
        if !copy_from_process(header.addr() + HEADER, &mut cmds) {
            continue;
        }
        let mut at = 0;
        for _ in 0..ncmds {
            let (Some(kind), Some(len)) = (read_u32(&cmds, at), read_u32(&cmds, at + 4)) else {
                break;
            };
            let segname = cmds.get(at + 8..at + 24).unwrap_or_default();
            if kind == LC_SEGMENT_64 && segname.starts_with(b"__TEXT\0") {
                let (Some(vmaddr), Some(vmsize), Some(fileoff)) = (
                    read_u64(&cmds, at + 24),
                    read_u64(&cmds, at + 32),
                    read_u64(&cmds, at + 40),
                ) else {
                    break;
                };
                // An unload meanwhile may have moved another image to `i`
                let same = unsafe { _dyld_get_image_header(i) } == header;
                if let (true, Some(path)) = (same, copy_c_string(name.addr())) {
                    out.push(Mapping {
                        start: vmaddr.wrapping_add(slide),
                        limit: vmaddr.wrapping_add(slide).wrapping_add(vmsize),
                        offset: fileoff,
                        path,
                    });
                }
                break;
            }
            if len == 0 {
                break;
            }
            at += len as usize;
        }
    }
    out
}

/// Copies `buf.len()` bytes at `addr` of this process; false when any of
/// them is not mapped
#[cfg(target_vendor = "apple")]
fn copy_from_process(addr: usize, buf: &mut [u8]) -> bool {
    unsafe extern "C" {
        static mach_task_self_: u32;
        fn vm_read_overwrite(
            task: u32,
            address: usize,
            size: usize,
            data: usize,
            out_size: *mut usize,
        ) -> i32;
    }
    if buf.is_empty() {
        return true;
    }
    let mut copied = 0;
    // The kernel writes into the buffer through its address
    let data = buf.as_mut_ptr().expose_provenance();
    let ok = unsafe { vm_read_overwrite(mach_task_self_, addr, buf.len(), data, &raw mut copied) };
    ok == 0 && copied == buf.len()
}

/// A NUL-terminated string at `addr`, read a page at a time so that the
/// read never runs into a page past its end
#[cfg(target_vendor = "apple")]
fn copy_c_string(addr: usize) -> Option<String> {
    const MAX: usize = 4096;
    let page = crate::os::page_size();
    let mut bytes = Vec::new();
    let mut at = addr;
    while bytes.len() < MAX {
        let mut chunk = vec![0u8; page - at % page];
        if !copy_from_process(at, &mut chunk) {
            return None;
        }
        if let Some(end) = chunk.iter().position(|&b| b == 0) {
            bytes.extend_from_slice(&chunk[..end]);
            return Some(String::from_utf8_lossy(&bytes).into_owned());
        }
        at += chunk.len();
        bytes.extend_from_slice(&chunk);
    }
    None
}

#[cfg(windows)]
pub fn mappings() -> Vec<Mapping> {
    use windows_sys::Win32::Foundation::HMODULE;
    use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW;
    use windows_sys::Win32::System::ProcessStatus::{
        K32EnumProcessModules, K32GetModuleInformation, MODULEINFO,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let mut out = Vec::new();
    unsafe {
        let process = GetCurrentProcess();
        let mut modules: Vec<HMODULE> = vec![core::ptr::null_mut(); 1024];
        let mut needed = 0u32;
        let bytes = (modules.len() * size_of::<HMODULE>()) as u32;
        if K32EnumProcessModules(process, modules.as_mut_ptr(), bytes, &raw mut needed) == 0 {
            return out;
        }
        modules.truncate((needed as usize / size_of::<HMODULE>()).min(modules.len()));
        for module in modules {
            let mut info: MODULEINFO = core::mem::zeroed();
            if K32GetModuleInformation(
                process,
                module,
                &raw mut info,
                size_of::<MODULEINFO>() as u32,
            ) == 0
            {
                continue;
            }
            let mut name = [0u16; 1024];
            let len = GetModuleFileNameW(module, name.as_mut_ptr(), name.len() as u32) as usize;
            let start = info.lpBaseOfDll as u64;
            out.push(Mapping {
                start,
                limit: start + u64::from(info.SizeOfImage),
                offset: 0,
                path: String::from_utf16_lossy(&name[..len]),
            });
        }
    }
    out
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    windows
)))]
pub fn mappings() -> Vec<Mapping> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::parse_maps_line;

    #[test]
    fn maps_lines() {
        let m = parse_maps_line(
            "7f12a000-7f12c000 r-xp 00001000 fd:01 1234                       /work/map check/bin (deleted)",
        )
        .unwrap();
        assert_eq!(
            (m.start, m.limit, m.offset),
            (0x7f12_a000, 0x7f12_c000, 0x1000)
        );
        assert_eq!(m.path, "/work/map check/bin (deleted)");
        assert!(parse_maps_line("7f12a000-7f12c000 rw-p 00000000 fd:01 1234 /lib/x.so").is_none());
        assert!(parse_maps_line("7ffd000-7ffe000 r-xp 00000000 00:00 0 [vdso]").is_none());
        assert!(parse_maps_line("7ffd000-7ffe000 r-xp 00000000 00:00 0").is_none());
    }
}
