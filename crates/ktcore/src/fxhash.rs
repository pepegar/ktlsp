//! A fast, non-cryptographic hasher for internal lookup maps.
//!
//! The symbol index and inference caches hash short keys (names, small integer tuples) millions of
//! times per project analysis; the std SipHash default costs more than the lookups it protects.
//! This is the well-known FxHash algorithm (rotate-xor-multiply, as used by rustc): excellent for
//! trusted in-process keys where HashDoS is not a concern.

use std::hash::{BuildHasherDefault, Hasher};

const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

#[derive(Default, Clone)]
pub struct FxHasher {
    hash: u64,
}

impl FxHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(SEED);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for chunk in &mut chunks {
            self.add(u64::from_le_bytes(chunk.try_into().unwrap()));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut word = 0u64;
            for (i, b) in rest.iter().enumerate() {
                word |= (*b as u64) << (i * 8);
            }
            self.add(word);
        }
    }

    #[inline]
    fn write_u8(&mut self, n: u8) {
        self.add(n as u64);
    }

    #[inline]
    fn write_u16(&mut self, n: u16) {
        self.add(n as u64);
    }

    #[inline]
    fn write_u32(&mut self, n: u32) {
        self.add(n as u64);
    }

    #[inline]
    fn write_u64(&mut self, n: u64) {
        self.add(n);
    }

    #[inline]
    fn write_usize(&mut self, n: usize) {
        self.add(n as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

pub type FxBuildHasher = BuildHasherDefault<FxHasher>;
pub type FxHashMap<K, V> = std::collections::HashMap<K, V, FxBuildHasher>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fx_map_lookups_round_trip() {
        let mut map = FxHashMap::default();
        map.insert("Greeter".to_string(), 1);
        map.insert("greet".to_string(), 2);
        assert_eq!(map.get("Greeter"), Some(&1));
        assert_eq!(map.get("greet"), Some(&2));
        assert_eq!(map.get("missing"), None);
    }

    #[test]
    fn fx_hashes_tuple_keys() {
        let mut map = FxHashMap::default();
        map.insert((10usize, 20usize, 3u16), "a");
        map.insert((10usize, 20usize, 4u16), "b");
        assert_eq!(map.get(&(10, 20, 3)), Some(&"a"));
        assert_eq!(map.get(&(10, 20, 4)), Some(&"b"));
    }
}
