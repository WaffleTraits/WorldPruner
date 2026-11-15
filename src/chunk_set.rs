//! A fixed-size bit set holding the 1024 chunks of one region.

use crate::pos::CHUNKS_PER_REGION;

/// One bit per chunk of a region. Row `z` is a `u32` whose bit `x` is the
/// chunk at region-relative position `x|z`, so the bit number of a chunk in
/// the whole set equals its slot in the region file header.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct ChunkSet {
    rows: [u32; 32],
}

impl ChunkSet {
    pub const fn new() -> Self {
        ChunkSet { rows: [0; 32] }
    }

    pub const fn full() -> Self {
        ChunkSet { rows: [u32::MAX; 32] }
    }

    #[inline]
    pub fn get(&self, index: usize) -> bool {
        debug_assert!(index < CHUNKS_PER_REGION);
        self.rows[(index >> 5) & 31] & (1 << (index & 31)) != 0
    }

    #[inline]
    pub fn set(&mut self, index: usize) {
        debug_assert!(index < CHUNKS_PER_REGION);
        self.rows[(index >> 5) & 31] |= 1 << (index & 31);
    }

    #[inline]
    pub fn clear(&mut self, index: usize) {
        debug_assert!(index < CHUNKS_PER_REGION);
        self.rows[(index >> 5) & 31] &= !(1 << (index & 31));
    }

    /// Bit mask of row `z` (`0..32`).
    #[inline]
    pub fn row(&self, z: usize) -> u32 {
        self.rows[z & 31]
    }

    #[inline]
    pub fn row_mut(&mut self, z: usize) -> &mut u32 {
        &mut self.rows[z & 31]
    }

    pub fn rows(&self) -> &[u32; 32] {
        &self.rows
    }

    /// Number of chunks in the set.
    pub fn len(&self) -> usize {
        self.rows.iter().map(|r| r.count_ones() as usize).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.iter().all(|&r| r == 0)
    }

    pub fn is_full(&self) -> bool {
        self.rows.iter().all(|&r| r == u32::MAX)
    }

    pub fn union_with(&mut self, other: &ChunkSet) {
        for (a, b) in self.rows.iter_mut().zip(&other.rows) {
            *a |= b;
        }
    }

    pub fn subtract(&mut self, other: &ChunkSet) {
        for (a, b) in self.rows.iter_mut().zip(&other.rows) {
            *a &= !b;
        }
    }

    /// The set of all chunks that are *not* in this set.
    pub fn complement(&self) -> ChunkSet {
        let mut rows = self.rows;
        for r in &mut rows {
            *r = !*r;
        }
        ChunkSet { rows }
    }

    /// Indices of all chunks in the set, in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.rows.iter().enumerate().flat_map(|(z, &row)| {
            let mut bits = row;
            std::iter::from_fn(move || {
                if bits == 0 {
                    return None;
                }
                let x = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                Some((z << 5) | x)
            })
        })
    }
}

impl std::fmt::Debug for ChunkSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ChunkSet({} chunks)", self.len())
    }
}

impl FromIterator<usize> for ChunkSet {
    fn from_iter<I: IntoIterator<Item = usize>>(iter: I) -> Self {
        let mut set = ChunkSet::new();
        for index in iter {
            set.set(index);
        }
        set
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_clear() {
        let mut s = ChunkSet::new();
        assert!(s.is_empty());
        for i in [0, 31, 32, 63, 64, 1023] {
            assert!(!s.get(i));
            s.set(i);
            assert!(s.get(i));
        }
        assert_eq!(s.len(), 6);
        s.set(0);
        assert_eq!(s.len(), 6);
        s.clear(0);
        s.clear(0);
        assert!(!s.get(0));
        assert_eq!(s.len(), 5);
        assert_eq!(s.iter().collect::<Vec<_>>(), vec![31, 32, 63, 64, 1023]);
    }

    #[test]
    fn full_and_complement() {
        let full = ChunkSet::full();
        assert!(full.is_full());
        assert_eq!(full.len(), 1024);
        assert!(full.complement().is_empty());
        let mut s = ChunkSet::new();
        s.set(5);
        let c = s.complement();
        assert_eq!(c.len(), 1023);
        assert!(!c.get(5));
        let mut u = c.clone();
        u.union_with(&s);
        assert!(u.is_full());
        u.subtract(&c);
        assert_eq!(u, s);
    }

    #[test]
    fn row_layout_matches_header_index() {
        let mut s = ChunkSet::new();
        s.set(8 * 32 + 5);
        assert_eq!(s.row(8), 1 << 5);
        *s.row_mut(31) = 1 << 31;
        assert!(s.get(1023));
    }
}
