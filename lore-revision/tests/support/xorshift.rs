// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A seeded xorshift and the incompressible bytes it makes, for the tests of every platform.
#![allow(dead_code)]

/// A seeded xorshift64. A zero state stays zero, so seeds are made odd where they may be zero.
pub struct XorShift(pub u64);

impl XorShift {
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// `len` incompressible bytes, distinct per `seed`: the low byte of each xorshift step.
pub fn incompressible(len: usize, seed: u64) -> Vec<u8> {
    let mut rng = XorShift(seed | 1);
    (0..len).map(|_| rng.next_u64() as u8).collect()
}
