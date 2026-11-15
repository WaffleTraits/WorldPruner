//! Chunk and region coordinates.

use std::fmt;

/// Number of chunks along one side of a region.
pub const REGION_SIZE: i32 = 32;
/// Number of chunks in a region file.
pub const CHUNKS_PER_REGION: usize = 1024;

/// Absolute chunk coordinates.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ChunkPos {
    pub x: i32,
    pub z: i32,
}

/// Region file coordinates (`r.<x>.<z>.mca`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct RegionPos {
    pub x: i32,
    pub z: i32,
}

impl ChunkPos {
    pub const fn new(x: i32, z: i32) -> Self {
        ChunkPos { x, z }
    }

    /// Decode Minecraft's packed chunk position (`x` in the low 32 bits, `z`
    /// in the high 32 bits), as used by structure references.
    pub const fn from_long(packed: i64) -> Self {
        ChunkPos { x: packed as i32, z: (packed >> 32) as i32 }
    }

    pub const fn to_long(self) -> i64 {
        ((self.z as i64) << 32) | (self.x as u32 as i64)
    }

    /// The region this chunk is stored in.
    pub const fn region(self) -> RegionPos {
        RegionPos { x: self.x >> 5, z: self.z >> 5 }
    }

    /// Slot of this chunk in its region file header (`0..1024`).
    pub const fn index(self) -> usize {
        (((self.z & 31) << 5) | (self.x & 31)) as usize
    }
}

impl RegionPos {
    pub const fn new(x: i32, z: i32) -> Self {
        RegionPos { x, z }
    }

    /// Absolute position of the chunk stored in header slot `index`.
    ///
    /// Uses wrapping arithmetic so absurd region coordinates in a file name
    /// cannot cause an overflow panic.
    pub const fn chunk(self, index: usize) -> ChunkPos {
        ChunkPos { x: self.x.wrapping_shl(5).wrapping_add((index & 31) as i32), z: self.z.wrapping_shl(5).wrapping_add(((index >> 5) & 31) as i32) }
    }

    /// `r.<x>.<z>.mca`
    pub fn file_name(self) -> String {
        format!("r.{}.{}.mca", self.x, self.z)
    }

    /// Parse a region file name. Only the canonical spelling is accepted, so
    /// `r.01.0.mca` or temporary files never match.
    pub fn from_file_name(name: &str) -> Option<Self> {
        let rest = name.strip_prefix("r.")?.strip_suffix(".mca")?;
        let (x, z) = rest.split_once('.')?;
        let pos = RegionPos { x: parse_coord(x)?, z: parse_coord(z)? };
        // Regions this far out cannot be addressed by 32-bit chunk coordinates.
        const LIMIT: i32 = i32::MAX >> 5;
        (pos.x.abs() <= LIMIT && pos.z.abs() <= LIMIT).then_some(pos)
    }
}

/// Parse an oversized-chunk file name (`c.<x>.<z>.mcc`).
pub fn parse_mcc_file_name(name: &str) -> Option<ChunkPos> {
    let rest = name.strip_prefix("c.")?.strip_suffix(".mcc")?;
    let (x, z) = rest.split_once('.')?;
    Some(ChunkPos { x: parse_coord(x)?, z: parse_coord(z)? })
}

/// `c.<x>.<z>.mcc`
pub fn mcc_file_name(pos: ChunkPos) -> String {
    format!("c.{}.{}.mcc", pos.x, pos.z)
}

fn parse_coord(s: &str) -> Option<i32> {
    let digits = s.strip_prefix('-').unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

impl fmt::Display for ChunkPos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}|{}", self.x, self.z)
    }
}

impl fmt::Display for RegionPos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}|{}", self.x, self.z)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_round_trip() {
        for (x, z) in [(0, 0), (1, -1), (-1, 1), (i32::MAX, i32::MIN), (-123456, 654321)] {
            let p = ChunkPos::new(x, z);
            assert_eq!(ChunkPos::from_long(p.to_long()), p);
        }
        assert_eq!(ChunkPos::new(-1, 0).to_long(), 0xFFFF_FFFF);
        assert_eq!(ChunkPos::new(0, 1).to_long(), 1 << 32);
    }

    #[test]
    fn chunk_to_region_and_index() {
        assert_eq!(ChunkPos::new(0, 0).region(), RegionPos::new(0, 0));
        assert_eq!(ChunkPos::new(31, 31).region(), RegionPos::new(0, 0));
        assert_eq!(ChunkPos::new(32, -1).region(), RegionPos::new(1, -1));
        assert_eq!(ChunkPos::new(-33, -32).region(), RegionPos::new(-2, -1));
        assert_eq!(ChunkPos::new(5, 8).index(), 8 * 32 + 5);
        assert_eq!(ChunkPos::new(-1, -1).index(), 1023);
        for index in [0, 1, 31, 32, 517, 1023] {
            let region = RegionPos::new(-3, 7);
            let chunk = region.chunk(index);
            assert_eq!(chunk.region(), region);
            assert_eq!(chunk.index(), index);
        }
    }

    #[test]
    fn file_names() {
        assert_eq!(RegionPos::from_file_name("r.0.0.mca"), Some(RegionPos::new(0, 0)));
        assert_eq!(RegionPos::from_file_name("r.-12.34.mca"), Some(RegionPos::new(-12, 34)));
        assert_eq!(RegionPos::new(-12, 34).file_name(), "r.-12.34.mca");
        for bad in [
            "r.0.0.mcr",
            "r.0.mca",
            "r.0.0.0.mca",
            "r.a.0.mca",
            "r.+1.0.mca",
            "r.0.0.mca.tmp",
            "x.0.0.mca",
            "r..0.mca",
            "r.99999999999.0.mca",
            "r.0.2147483647.mca",
        ] {
            assert_eq!(RegionPos::from_file_name(bad), None, "{bad}");
        }
        assert_eq!(parse_mcc_file_name("c.-5.70.mcc"), Some(ChunkPos::new(-5, 70)));
        assert_eq!(parse_mcc_file_name("c.-5.mcc"), None);
        assert_eq!(mcc_file_name(ChunkPos::new(-5, 70)), "c.-5.70.mcc");
    }
}
