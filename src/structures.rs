//! Bookkeeping for generated structures.
//!
//! A structure (village, stronghold, ...) is stored in the chunk it *starts*
//! in, and every chunk it reaches into holds a *reference* to that start.
//! When a chunk is kept, all structures it is part of have to be kept
//! completely, otherwise the game would later regenerate the pruned half of a
//! structure on top of whatever the new terrain looks like.

use crate::nbt::BlockBox;
use crate::pos::ChunkPos;
use crate::selection::Selection;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::RwLock;

/// Structure pieces spanning more chunks than this are considered corrupt.
/// Real structures stay far below it.
const MAX_SPAN_CHUNKS: i32 = 1024;

/// Identifies one generated structure: its type and the chunk it starts in.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct StructureKey {
    /// Interned structure id.
    pub id: u32,
    /// Packed position of the start chunk.
    pub start: i64,
}

/// A rectangle of chunks, all bounds inclusive.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ChunkRect {
    pub min_x: i32,
    pub min_z: i32,
    pub max_x: i32,
    pub max_z: i32,
}

impl ChunkRect {
    /// The chunks covered by a structure piece, plus a margin of one chunk on
    /// every side (structures may place blocks slightly outside their box).
    /// Returns `None` for boxes that are inverted or implausibly large.
    pub fn around(bb: BlockBox) -> Option<ChunkRect> {
        if bb.max_x < bb.min_x || bb.max_z < bb.min_z {
            return None;
        }
        // Block coordinates shifted to chunks leave plenty of room for the margin.
        let rect = ChunkRect { min_x: (bb.min_x >> 4) - 1, min_z: (bb.min_z >> 4) - 1, max_x: (bb.max_x >> 4) + 1, max_z: (bb.max_z >> 4) + 1 };
        let plausible = rect.max_x - rect.min_x < MAX_SPAN_CHUNKS && rect.max_z - rect.min_z < MAX_SPAN_CHUNKS;
        plausible.then_some(rect)
    }

    fn contains(&self, other: &ChunkRect) -> bool {
        self.min_x <= other.min_x && self.max_x >= other.max_x && self.min_z <= other.min_z && self.max_z >= other.max_z
    }
}

/// Convert the boxes of a structure into chunk rectangles, dropping
/// duplicates and rectangles that lie inside another one. Returns the
/// rectangles and the number of boxes that had to be ignored.
pub fn chunk_rects(boxes: &[BlockBox]) -> (Box<[ChunkRect]>, usize) {
    let mut rects: Vec<ChunkRect> = boxes.iter().filter_map(|&bb| ChunkRect::around(bb)).collect();
    let ignored = boxes.len() - rects.len();
    rects.sort_unstable();
    rects.dedup();
    let mut result: Vec<ChunkRect> = Vec::with_capacity(rects.len());
    for rect in rects {
        if result.iter().any(|kept| kept.contains(&rect)) {
            continue;
        }
        result.retain(|kept| !rect.contains(kept));
        result.push(rect);
    }
    (result.into_boxed_slice(), ignored)
}

/// Thread-safe string interner for structure ids. There are only a few
/// hundred distinct ids, but hundreds of thousands of starts and references.
#[derive(Default)]
pub struct Interner {
    ids: RwLock<FxHashMap<Box<str>, u32>>,
}

impl Interner {
    pub fn intern(&self, name: &str) -> u32 {
        if let Some(&id) = self.ids.read().unwrap_or_else(|e| e.into_inner()).get(name) {
            return id;
        }
        let mut ids = self.ids.write().unwrap_or_else(|e| e.into_inner());
        let next = ids.len() as u32;
        *ids.entry(name.into()).or_insert(next)
    }

    /// All names, indexed by id.
    pub fn names(&self) -> Vec<String> {
        let ids = self.ids.read().unwrap_or_else(|e| e.into_inner());
        let mut names = vec![String::new(); ids.len()];
        for (name, &id) in ids.iter() {
            names[id as usize] = name.to_string();
        }
        names
    }
}

/// A per-thread cache in front of an [`Interner`], to stay off its lock.
pub struct LocalInterner<'a> {
    shared: &'a Interner,
    cache: FxHashMap<Box<str>, u32>,
}

impl<'a> LocalInterner<'a> {
    pub fn new(shared: &'a Interner) -> Self {
        LocalInterner { shared, cache: FxHashMap::default() }
    }

    pub fn intern(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.cache.get(name) {
            return id;
        }
        let id = self.shared.intern(name);
        self.cache.insert(name.into(), id);
        id
    }
}

/// All structure starts found in the world and the structures to keep.
#[derive(Default)]
pub struct StructureIndex {
    starts: FxHashMap<StructureKey, Box<[ChunkRect]>>,
    keep: FxHashSet<StructureKey>,
}

/// Result of [`StructureIndex::select_kept`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct StructureStats {
    /// Structures whose chunks were added to the selection.
    pub kept: u64,
    /// Referenced structures whose start could not be found, e.g. because
    /// the start chunk no longer exists.
    pub missing: u64,
}

impl StructureIndex {
    pub fn add_starts(&mut self, starts: impl IntoIterator<Item = (StructureKey, Box<[ChunkRect]>)>) {
        self.starts.extend(starts);
    }

    pub fn keep(&mut self, keys: impl IntoIterator<Item = StructureKey>) {
        self.keep.extend(keys);
    }

    /// Add every chunk of every structure that has to be kept to `selection`.
    pub fn select_kept(&self, selection: &mut Selection, names: &[String]) -> StructureStats {
        let mut stats = StructureStats::default();
        for key in &self.keep {
            match self.starts.get(key) {
                Some(rects) => {
                    stats.kept += 1;
                    for rect in rects.iter() {
                        selection.add_rect(ChunkPos::new(rect.min_x, rect.min_z), ChunkPos::new(rect.max_x, rect.max_z));
                    }
                }
                None => {
                    stats.missing += 1;
                    log::debug!(
                        "could not find the start of structure {} in chunk {}",
                        names.get(key.id as usize).map_or("?", String::as_str),
                        ChunkPos::from_long(key.start)
                    );
                }
            }
        }
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bb(min_x: i32, min_z: i32, max_x: i32, max_z: i32) -> BlockBox {
        BlockBox { min_x, min_z, max_x, max_z }
    }

    #[test]
    fn rect_has_one_chunk_margin() {
        assert_eq!(ChunkRect::around(bb(0, 0, 15, 15)), Some(ChunkRect { min_x: -1, min_z: -1, max_x: 1, max_z: 1 }));
        assert_eq!(ChunkRect::around(bb(15, -1, 16, 0)), Some(ChunkRect { min_x: -1, min_z: -2, max_x: 2, max_z: 1 }));
        assert_eq!(ChunkRect::around(bb(-17, 32, -17, 32)), Some(ChunkRect { min_x: -3, min_z: 1, max_x: -1, max_z: 3 }));
    }

    #[test]
    fn rect_is_a_superset_of_the_original_tool() {
        // The Java version stepped from min-16 to max+16 in steps of 16 blocks.
        for (min, max) in [(0, 15), (15, 16), (0, 16), (-33, 47), (7, 7), (-1, 0), (100, 355)] {
            let rect = ChunkRect::around(bb(min, 0, max, 0)).unwrap();
            let mut x = min - 16;
            while x <= max + 16 {
                assert!((rect.min_x..=rect.max_x).contains(&(x >> 4)), "{min}..{max}: chunk {}", x >> 4);
                x += 16;
            }
        }
    }

    #[test]
    fn rect_rejects_garbage() {
        assert_eq!(ChunkRect::around(bb(10, 0, 9, 0)), None);
        assert_eq!(ChunkRect::around(bb(0, 10, 0, 9)), None);
        assert_eq!(ChunkRect::around(bb(i32::MIN, 0, i32::MAX, 0)), None);
        assert_eq!(ChunkRect::around(bb(0, 0, 0, 16 * 2000)), None);
        assert!(ChunkRect::around(bb(i32::MAX - 5, i32::MIN, i32::MAX, i32::MIN + 5)).is_some());
    }

    #[test]
    fn rects_are_deduplicated() {
        let (rects, ignored) = chunk_rects(&[
            bb(0, 0, 15, 15),
            bb(1, 1, 14, 14),   // same chunks
            bb(0, 0, 100, 100), // contains both
            bb(40, 40, 50, 50), // inside the big one
            bb(200, 0, 210, 5), // separate
            bb(5, 5, 4, 4),     // inverted
        ]);
        assert_eq!(ignored, 1);
        assert_eq!(rects.as_ref(), &[ChunkRect { min_x: -1, min_z: -1, max_x: 7, max_z: 7 }, ChunkRect { min_x: 11, min_z: -1, max_x: 14, max_z: 1 }]);
        assert!(chunk_rects(&[]).0.is_empty());
    }

    #[test]
    fn interner_is_consistent() {
        let shared = Interner::default();
        let mut a = LocalInterner::new(&shared);
        let mut b = LocalInterner::new(&shared);
        let village = a.intern("minecraft:village");
        let fort = b.intern("minecraft:fortress");
        assert_ne!(village, fort);
        assert_eq!(b.intern("minecraft:village"), village);
        assert_eq!(a.intern("minecraft:fortress"), fort);
        assert_eq!(a.intern("minecraft:village"), village);
        let names = shared.names();
        assert_eq!(names[village as usize], "minecraft:village");
        assert_eq!(names[fort as usize], "minecraft:fortress");
    }

    #[test]
    fn index_selects_kept_structures() {
        let mut index = StructureIndex::default();
        let a = StructureKey { id: 0, start: ChunkPos::new(0, 0).to_long() };
        let b = StructureKey { id: 0, start: ChunkPos::new(50, 50).to_long() };
        let c = StructureKey { id: 1, start: ChunkPos::new(0, 0).to_long() };
        index.add_starts([(a, chunk_rects(&[bb(0, 0, 40, 10)]).0), (b, chunk_rects(&[bb(800, 800, 810, 810)]).0)]);
        index.keep([a, c, a]);
        let mut selection = Selection::new();
        let stats = index.select_kept(&mut selection, &["x".into(), "y".into()]);
        assert_eq!(stats, StructureStats { kept: 1, missing: 1 });
        assert_eq!(selection.stats().total_chunks, 5 * 3);
        assert!(selection.is_chunk_selected(ChunkPos::new(-1, -1)));
        assert!(selection.is_chunk_selected(ChunkPos::new(3, 1)));
        assert!(!selection.is_chunk_selected(ChunkPos::new(4, 1)));
        assert!(!selection.is_chunk_selected(ChunkPos::new(50, 50)));
    }
}
