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

//! A type with a method named like another trait's method on it, in a file
//! of its own, so that the profile can tell the two apart

use std::hint::black_box;

pub struct Twice;

pub trait Second {
    fn allocate(&self) -> Vec<u8>;
}

impl Second for Twice {
    #[inline(never)]
    fn allocate(&self) -> Vec<u8> {
        black_box(vec![2u8; 8192])
    }
}
