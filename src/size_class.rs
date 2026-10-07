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

//! Size classes: 8-byte steps up to 128 bytes (Rust needs no 16-byte
//! quantum), eight classes per doubling up to 1 KiB, where Rust's structs
//! and nodes cluster, then four per doubling as in jemalloc. Padding stays
//! under 12.5% of allocations from 128 bytes to 1 KiB, and under 25% above.

pub const PAGE_SHIFT: usize = 12;
/// The allocator's page; slabs and large runs are made of these
pub const PAGE: usize = 1 << PAGE_SHIFT;

/// Doublings below `1 << FINE_LG` get eight classes, the rest four
const FINE_LG: usize = 10;
/// Largest class served from slabs
pub const SMALL_MAX: usize = 14336;
/// Largest class kept in the thread cache
pub const TCACHE_MAX: usize = 32768;
/// Largest class served from page runs inside chunks; above it is huge
pub const LARGE_MAX: usize = 1 << 20;

const fn classes_per_doubling(lg: usize) -> usize {
    if lg < FINE_LG { 8 } else { 4 }
}

/// Size of class `i` while generating, or 0 past the last class
const fn gen_class(i: usize) -> usize {
    if i < 16 {
        return (i + 1) * 8;
    }
    let mut i = i - 16;
    let mut lg = 7;
    loop {
        let n = classes_per_doubling(lg);
        if i < n {
            let size = (1 << lg) + (i + 1) * ((1 << lg) / n);
            return if size > LARGE_MAX { 0 } else { size };
        }
        i -= n;
        lg += 1;
    }
}

const fn count_up_to(max: usize) -> usize {
    let mut i = 0;
    while gen_class(i) != 0 && gen_class(i) <= max {
        i += 1;
    }
    i
}

pub const NCLASSES: usize = count_up_to(LARGE_MAX);
pub const NSMALL: usize = count_up_to(SMALL_MAX);
pub const NCACHED: usize = count_up_to(TCACHE_MAX);

pub static CLASS_SIZE: [u32; NCLASSES] = CLASS_SIZES;

const CLASS_SIZES: [u32; NCLASSES] = {
    let mut s = [0u32; NCLASSES];
    let mut i = 0;
    while i < NCLASSES {
        s[i] = gen_class(i) as u32;
        i += 1;
    }
    s
};

const _: () = {
    assert!(CLASS_SIZES[NSMALL - 1] as usize == SMALL_MAX);
    assert!(CLASS_SIZES[NCACHED - 1] as usize == TCACHE_MAX);
    assert!(CLASS_SIZES[NCLASSES - 1] as usize == LARGE_MAX);
    assert!(NCLASSES <= 256);
};

/// Pages per slab: the fewest that waste at most 1/16 of the slab
pub static SLAB_PAGES: [u8; NSMALL] = {
    let mut out = [0u8; NSMALL];
    let mut c = 0;
    while c < NSMALL {
        let size = CLASS_SIZES[c] as usize;
        // Otherwise the least wasteful ratio up to 16 pages
        let (mut best, mut best_waste, mut best_bytes) = (0, 1, 0);
        let mut p = 1;
        while p <= 16 {
            let bytes = p * PAGE;
            if bytes >= size {
                let waste = bytes % size;
                if waste * 16 <= bytes {
                    best = p;
                    break;
                }
                if best == 0 || waste * best_bytes < best_waste * bytes {
                    (best, best_waste, best_bytes) = (p, waste, bytes);
                }
            }
            p += 1;
        }
        out[c] = best as u8;
        c += 1;
    }
    out
};

pub static SLAB_OBJS: [u16; NSMALL] = {
    let mut out = [0u16; NSMALL];
    let mut c = 0;
    while c < NSMALL {
        out[c] = ((SLAB_PAGES[c] as usize * PAGE) / CLASS_SIZES[c] as usize) as u16;
        c += 1;
    }
    out
};

/// Thread cache slots per class: up to 200 objects or 16 KiB, at least 8
pub static CACHE_CAP: [u16; NCACHED] = CACHE_CAPS;

const CACHE_CAPS: [u16; NCACHED] = {
    let mut out = [0u16; NCACHED];
    let mut c = 0;
    while c < NCACHED {
        let size = CLASS_SIZES[c] as usize;
        let cap = if c >= NSMALL {
            4
        } else {
            let n = 16384 / size;
            if n > 200 {
                200
            } else if n < 8 {
                8
            } else {
                n
            }
        };
        out[c] = cap as u16;
        c += 1;
    }
    out
};

pub const TOTAL_SLOTS: usize = {
    let mut n = 0;
    let mut c = 0;
    while c < NCACHED {
        n += CACHE_CAPS[c] as usize;
        c += 1;
    }
    n
};

const LOOKUP_MAX: usize = 4096;

/// Classes of sizes up to `LOOKUP_MAX`, by size in 8-byte steps
static LOOKUP: [u8; LOOKUP_MAX / 8 + 1] = {
    let mut t = [0u8; LOOKUP_MAX / 8 + 1];
    let mut i = 0;
    let mut c = 0;
    while i < t.len() {
        while (CLASS_SIZES[c] as usize) < i * 8 {
            c += 1;
        }
        t[i] = c as u8;
        i += 1;
    }
    t
};

/// The class of `LOOKUP_MAX`, the last of a four-class doubling
const LOOKUP_MAX_CLASS: usize = count_up_to(LOOKUP_MAX) - 1;

/// The class for `size` above `LOOKUP_MAX`, four classes per doubling
const fn compute_class(size: usize) -> usize {
    let lg = (usize::BITS - 1 - (size - 1).leading_zeros()) as usize;
    let step_lg = lg - 2;
    let k = (size - (1 << lg) + (1 << step_lg) - 1) >> step_lg;
    LOOKUP_MAX_CLASS + (lg - LOOKUP_MAX.trailing_zeros() as usize) * 4 + k
}

/// The class for `size` (at most `LARGE_MAX`) with alignment of at most 8
#[inline(always)]
pub fn class_of(size: usize) -> usize {
    if size <= LOOKUP_MAX {
        unsafe { *LOOKUP.get_unchecked((size + 7) >> 3) as usize }
    } else {
        compute_class(size)
    }
}

/// The class for `size` and `align` (at most PAGE), or `None` when the
/// allocation is huge. Every class that is a multiple of `align` keeps
/// its objects aligned, since slabs and runs start on page boundaries.
#[inline]
pub fn class_for(size: usize, align: usize) -> Option<usize> {
    if align <= 8 {
        return if size <= LARGE_MAX {
            Some(class_of(size))
        } else {
            None
        };
    }
    let rounded = (size + align - 1) & !(align - 1);
    if rounded > LARGE_MAX {
        return None;
    }
    let mut c = class_of(rounded);
    while CLASS_SIZE[c] as usize & (align - 1) != 0 {
        c += 1;
    }
    Some(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_of_is_smallest_fit() {
        for size in 0..=LARGE_MAX {
            let c = class_of(size);
            assert!(CLASS_SIZE[c] as usize >= size, "size {size}");
            if c > 0 {
                assert!((CLASS_SIZE[c - 1] as usize) < size, "size {size}");
            }
        }
    }

    #[test]
    fn class_for_respects_alignment() {
        let mut align = 16;
        while align <= PAGE {
            for size in (0..LARGE_MAX).step_by(97) {
                if let Some(c) = class_for(size, align) {
                    let cs = CLASS_SIZE[c] as usize;
                    assert!(
                        cs >= size && cs.is_multiple_of(align),
                        "size {size} align {align}"
                    );
                }
            }
            align *= 2;
        }
    }

    #[test]
    fn padding_is_bounded() {
        for size in 129..=LARGE_MAX {
            let pad = CLASS_SIZE[class_of(size)] as usize - size;
            if size <= 1024 {
                assert!(pad * 8 < size, "size {size}");
            } else {
                assert!(pad * 4 < size, "size {size}");
            }
        }
    }

    #[test]
    fn generated_classes_match_the_table() {
        for (c, &size) in CLASS_SIZE.iter().enumerate() {
            assert_eq!(gen_class(c), size as usize);
        }
        assert_eq!(gen_class(NCLASSES), 0);
        assert_eq!(count_up_to(LARGE_MAX), NCLASSES);
        assert_eq!(count_up_to(SMALL_MAX), NSMALL);
        assert_eq!(count_up_to(TCACHE_MAX), NCACHED);
    }

    #[test]
    fn slabs_hold_objects() {
        for c in 0..NSMALL {
            assert!(SLAB_PAGES[c] >= 1 && SLAB_OBJS[c] >= 1, "class {c}");
        }
    }
}
