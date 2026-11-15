//! A set of chunks, compatible with MCA Selector's selection CSV format.

use crate::chunk_set::ChunkSet;
use crate::pos::{ChunkPos, RegionPos};
use anyhow::{Context, Result, bail};
use rustc_hash::FxHashMap;
use std::collections::hash_map::Entry;
use std::fmt;
use std::io::BufRead;
use std::path::Path;

/// A (possibly inverted) selection of chunks.
///
/// Internally this stores *marked* chunks per region: `None` means the whole
/// region is marked, `Some(set)` marks individual chunks. A set is never
/// empty and never full; those cases are normalised to "no entry" and `None`.
/// When the selection is `inverted`, a chunk is selected iff it is not marked.
#[derive(Clone, Default, Debug)]
pub struct Selection {
    regions: FxHashMap<RegionPos, Option<Box<ChunkSet>>>,
    inverted: bool,
}

/// The selected chunks of a single region.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RegionSelection {
    /// No chunk of the region is selected.
    None,
    /// Every chunk of the region is selected.
    All,
    /// Exactly these chunks are selected (neither empty nor full).
    Some(ChunkSet),
}

impl RegionSelection {
    pub fn contains(&self, index: usize) -> bool {
        match self {
            RegionSelection::None => false,
            RegionSelection::All => true,
            RegionSelection::Some(set) => set.get(index),
        }
    }
}

/// Summary of a selection's stored entries, for logging.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Stats {
    pub inverted: bool,
    pub total_chunks: u64,
    pub chunks_in_partial_regions: u64,
    pub partial_regions: u64,
    pub full_regions: u64,
}

impl fmt::Display for Stats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} chunks in {} full and {} partial regions ({} chunks in partial regions){}",
            self.total_chunks,
            self.full_regions,
            self.partial_regions,
            self.chunks_in_partial_regions,
            if self.inverted { ", inverted" } else { "" }
        )
    }
}

impl Selection {
    pub fn new() -> Self {
        Selection::default()
    }

    pub fn is_inverted(&self) -> bool {
        self.inverted
    }

    /// True if nothing at all is selected.
    pub fn is_empty(&self) -> bool {
        !self.inverted && self.regions.is_empty()
    }

    // ---- raw "marked" storage -------------------------------------------

    /// Mark or unmark the chunks in `mask` of row `z` in `region`.
    fn apply_row(&mut self, region: RegionPos, z: usize, mask: u32, mark: bool) {
        if mask == 0 {
            return;
        }
        match self.regions.entry(region) {
            Entry::Occupied(mut entry) => {
                if mark {
                    if let Some(set) = entry.get_mut() {
                        *set.row_mut(z) |= mask;
                        if set.is_full() {
                            *entry.get_mut() = None;
                        }
                    }
                } else {
                    let slot = entry.get_mut();
                    let set = slot.get_or_insert_with(|| Box::new(ChunkSet::full()));
                    *set.row_mut(z) &= !mask;
                    if set.is_empty() {
                        entry.remove();
                    }
                }
            }
            Entry::Vacant(entry) => {
                if mark {
                    // A single row can never fill a region, so this is partial.
                    let mut set = Box::new(ChunkSet::new());
                    *set.row_mut(z) = mask;
                    entry.insert(Some(set));
                }
            }
        }
    }

    fn mark_region(&mut self, region: RegionPos, mark: bool) {
        if mark {
            self.regions.insert(region, None);
        } else {
            self.regions.remove(&region);
        }
    }

    fn is_marked(&self, chunk: ChunkPos) -> bool {
        match self.regions.get(&chunk.region()) {
            None => false,
            Some(None) => true,
            Some(Some(set)) => set.get(chunk.index()),
        }
    }

    // ---- selection API ---------------------------------------------------

    pub fn is_chunk_selected(&self, chunk: ChunkPos) -> bool {
        self.is_marked(chunk) != self.inverted
    }

    /// True if every chunk of the region is selected.
    pub fn is_region_selected(&self, region: RegionPos) -> bool {
        match self.regions.get(&region) {
            None => self.inverted,
            Some(None) => !self.inverted,
            Some(Some(_)) => false,
        }
    }

    /// True if at least one chunk of the region is selected.
    pub fn is_any_chunk_in_region_selected(&self, region: RegionPos) -> bool {
        match self.regions.get(&region) {
            None => self.inverted,
            Some(None) => !self.inverted,
            Some(Some(_)) => true,
        }
    }

    /// The selected chunks of one region.
    pub fn selected_chunks(&self, region: RegionPos) -> RegionSelection {
        match (self.regions.get(&region), self.inverted) {
            (None, false) | (Some(None), true) => RegionSelection::None,
            (None, true) | (Some(None), false) => RegionSelection::All,
            (Some(Some(set)), false) => RegionSelection::Some((**set).clone()),
            (Some(Some(set)), true) => RegionSelection::Some(set.complement()),
        }
    }

    /// Select a single chunk.
    pub fn add_chunk(&mut self, chunk: ChunkPos) {
        let index = chunk.index();
        self.apply_row(chunk.region(), index >> 5, 1 << (index & 31), !self.inverted);
    }

    /// Select a whole region.
    pub fn add_region(&mut self, region: RegionPos) {
        self.mark_region(region, !self.inverted);
    }

    /// Select several chunks of one region at once.
    pub fn add_chunk_set(&mut self, region: RegionPos, chunks: &ChunkSet) {
        if chunks.is_full() {
            self.add_region(region);
            return;
        }
        for (z, &mask) in chunks.rows().iter().enumerate() {
            self.apply_row(region, z, mask, !self.inverted);
        }
    }

    /// Select the chunks `x0..=x1` of the absolute chunk row `z`.
    pub fn add_row_range(&mut self, z: i32, x0: i32, x1: i32) {
        if x0 > x1 {
            return;
        }
        let mark = !self.inverted;
        let row = (z & 31) as usize;
        for rx in (x0 >> 5)..=(x1 >> 5) {
            let base = i64::from(rx) << 5;
            let lo = (i64::from(x0) - base).max(0) as u32;
            let hi = (i64::from(x1) - base).min(31) as u32;
            let mask = (u32::MAX >> (31 - hi)) & (u32::MAX << lo);
            self.apply_row(RegionPos::new(rx, z >> 5), row, mask, mark);
        }
    }

    /// Select an axis-aligned rectangle of chunks (all bounds inclusive).
    pub fn add_rect(&mut self, min: ChunkPos, max: ChunkPos) {
        for z in min.z..=max.z {
            self.add_row_range(z, min.x, max.x);
        }
    }

    pub fn stats(&self) -> Stats {
        let mut stats = Stats { inverted: self.inverted, ..Stats::default() };
        for set in self.regions.values() {
            match set {
                None => {
                    stats.total_chunks += 1024;
                    stats.full_regions += 1;
                }
                Some(set) => {
                    let size = set.len() as u64;
                    stats.total_chunks += size;
                    stats.chunks_in_partial_regions += size;
                    stats.partial_regions += 1;
                }
            }
        }
        stats
    }

    // ---- CSV -------------------------------------------------------------

    /// Read a selection file exported by MCA Selector.
    pub fn from_csv_file(path: &Path) -> Result<Selection> {
        let file = std::fs::File::open(path).with_context(|| format!("failed to open whitelist {}", path.display()))?;
        Selection::from_csv(std::io::BufReader::new(file)).with_context(|| format!("failed to read whitelist {}", path.display()))
    }

    /// Parse MCA Selector's selection format: an optional first line
    /// `inverted`, then one line per region (`rx;rz`) or chunk
    /// (`rx;rz;cx;cz`, with absolute chunk coordinates).
    pub fn from_csv(reader: impl BufRead) -> Result<Selection> {
        let mut selection = Selection::new();
        let mut inverted = false;
        for (i, line) in reader.lines().enumerate() {
            let num = i + 1;
            let line = line.with_context(|| format!("failed to read line {num}"))?;
            let line = line.trim().trim_start_matches('\u{feff}');
            if line.is_empty() {
                continue;
            }
            if num == 1 && line == "inverted" {
                inverted = true;
                continue;
            }
            let fields: Vec<&str> = line.split(';').map(str::trim).collect();
            if fields.len() != 2 && fields.len() != 4 {
                bail!("invalid region or chunk coordinate format in line {num}");
            }
            let parse = |s: &str, what: &str| -> Result<i32> { s.parse().map_err(|_| anyhow::anyhow!("failed to read {what} coordinates in line {num}")) };
            let region = RegionPos::new(parse(fields[0], "region")?, parse(fields[1], "region")?);
            if fields.len() == 4 {
                let chunk = ChunkPos::new(parse(fields[2], "chunk")?, parse(fields[3], "chunk")?);
                if chunk.region() != region {
                    bail!("chunk {chunk} is not in region {region} in line {num}");
                }
                // Entries are marks; `inverted` is applied once everything is read.
                selection.add_chunk(chunk);
            } else {
                selection.add_region(region);
            }
        }
        selection.inverted = inverted;
        Ok(selection)
    }

    /// Serialise in MCA Selector's selection format (sorted, so the output is
    /// deterministic).
    pub fn to_csv(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        if self.inverted {
            out.push_str("inverted\n");
        }
        let mut regions: Vec<_> = self.regions.iter().collect();
        regions.sort_by_key(|(pos, _)| **pos);
        for (region, set) in regions {
            match set {
                None => {
                    let _ = writeln!(out, "{};{}", region.x, region.z);
                }
                Some(set) => {
                    for index in set.iter() {
                        let chunk = region.chunk(index);
                        let _ = writeln!(out, "{};{};{};{}", region.x, region.z, chunk.x, chunk.z);
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(x: i32, z: i32) -> ChunkPos {
        ChunkPos::new(x, z)
    }

    fn inverted(mut s: Selection) -> Selection {
        s.inverted = true;
        s
    }

    fn assert_all_selected(s: &Selection, region: RegionPos, expected: bool) {
        for index in 0..1024 {
            let chunk = region.chunk(index);
            assert_eq!(s.is_chunk_selected(chunk), expected, "chunk {chunk} in region {region}");
        }
    }

    // ---- ported from the original TestSelection ----

    #[test]
    fn is_chunk_selected_full_region() {
        let mut s = Selection::new();
        s.add_region(RegionPos::new(0, 0));
        assert!(s.is_chunk_selected(c(0, 0)));
        assert!(s.is_chunk_selected(c(31, 31)));
        assert!(!s.is_chunk_selected(c(32, 32)));
        assert!(!s.is_chunk_selected(c(-1, -1)));

        let s = inverted(s);
        assert!(!s.is_chunk_selected(c(0, 0)));
        assert!(!s.is_chunk_selected(c(31, 31)));
        assert!(s.is_chunk_selected(c(32, 32)));
        assert!(s.is_chunk_selected(c(-1, -1)));
    }

    #[test]
    fn is_chunk_selected_individual_chunks() {
        let mut s = Selection::new();
        s.add_chunk(c(0, 0));
        s.add_chunk(c(5, 8));
        let selected = [c(0, 0), c(5, 8)];
        let unselected = [c(5, 7), c(5, 9), c(4, 8), c(6, 8), c(31, 31), c(32, 32), c(-1, -1)];
        for p in selected {
            assert!(s.is_chunk_selected(p));
        }
        for p in unselected {
            assert!(!s.is_chunk_selected(p));
        }
        let s = inverted(s);
        for p in selected {
            assert!(!s.is_chunk_selected(p));
        }
        for p in unselected {
            assert!(s.is_chunk_selected(p));
        }
    }

    #[test]
    fn add_chunk_filling_region_collapses_to_full() {
        let mut s = Selection::new();
        for x in 0..32 {
            for z in 0..32 {
                if x != 31 || z != 31 {
                    s.add_chunk(c(x, z));
                }
            }
        }
        assert_eq!(s.regions.len(), 1);
        assert_eq!(s.regions[&RegionPos::new(0, 0)].as_ref().unwrap().len(), 1023);
        assert!(!s.is_region_selected(RegionPos::new(0, 0)));

        s.add_chunk(c(31, 31));
        assert_eq!(s.regions.len(), 1);
        assert!(s.regions[&RegionPos::new(0, 0)].is_none());
        assert!(s.is_region_selected(RegionPos::new(0, 0)));
    }

    #[test]
    fn add_chunk_inverted() {
        let mut s = Selection::new();
        s.add_chunk(c(0, 0));
        let mut s = inverted(s);
        assert!(!s.is_chunk_selected(c(0, 0)));
        for index in 1..1024 {
            assert!(s.is_chunk_selected(RegionPos::new(0, 0).chunk(index)));
        }
        // selecting the only unselected chunk selects everything
        s.add_chunk(c(0, 0));
        assert!(s.regions.is_empty());
        assert_all_selected(&s, RegionPos::new(0, 0), true);
    }

    // ---- additional coverage ----

    #[test]
    fn add_chunk_to_fully_unselected_region_of_inverted_selection() {
        let mut s = Selection::new();
        s.add_region(RegionPos::new(0, 0));
        let mut s = inverted(s);
        assert_all_selected(&s, RegionPos::new(0, 0), false);
        s.add_chunk(c(3, 4));
        assert!(s.is_chunk_selected(c(3, 4)));
        assert!(!s.is_chunk_selected(c(4, 4)));
        assert_eq!(s.selected_chunks(RegionPos::new(0, 0)), RegionSelection::Some([4 * 32 + 3].into_iter().collect()));
    }

    #[test]
    fn region_queries() {
        let r = RegionPos::new(2, -3);
        let mut s = Selection::new();
        assert!(s.is_empty());
        assert!(!s.is_region_selected(r));
        assert!(!s.is_any_chunk_in_region_selected(r));
        assert_eq!(s.selected_chunks(r), RegionSelection::None);

        s.add_chunk(r.chunk(7));
        assert!(!s.is_region_selected(r));
        assert!(s.is_any_chunk_in_region_selected(r));
        assert!(s.selected_chunks(r).contains(7));
        assert!(!s.selected_chunks(r).contains(8));

        s.add_region(r);
        assert!(s.is_region_selected(r));
        assert_eq!(s.selected_chunks(r), RegionSelection::All);

        let mut s = inverted(s);
        assert!(!s.is_region_selected(r));
        assert!(!s.is_any_chunk_in_region_selected(r));
        assert_eq!(s.selected_chunks(r), RegionSelection::None);
        assert!(s.is_region_selected(RegionPos::new(9, 9)));
        assert_eq!(s.selected_chunks(RegionPos::new(9, 9)), RegionSelection::All);
        s.add_region(r);
        assert!(s.is_region_selected(r));
    }

    #[test]
    fn add_row_range_spans_regions() {
        let mut s = Selection::new();
        s.add_row_range(-1, -40, 70);
        for x in -100..100 {
            assert_eq!(s.is_chunk_selected(c(x, -1)), (-40..=70).contains(&x), "x={x}");
            assert!(!s.is_chunk_selected(c(x, 0)));
            assert!(!s.is_chunk_selected(c(x, -2)));
        }
        s.add_row_range(5, 3, 3);
        assert!(s.is_chunk_selected(c(3, 5)));
        assert!(!s.is_chunk_selected(c(2, 5)));
        assert!(!s.is_chunk_selected(c(4, 5)));
        s.add_row_range(5, 4, 3); // empty range
        assert!(!s.is_chunk_selected(c(4, 5)));
    }

    #[test]
    fn add_row_range_matches_add_chunk() {
        let mut a = Selection::new();
        let mut b = Selection::new();
        let mut seed = 12345u32;
        let mut next = move || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 8) as i32
        };
        for _ in 0..200 {
            let z = next() % 80 - 40;
            let x0 = next() % 200 - 100;
            let x1 = x0 + next() % 90;
            a.add_row_range(z, x0, x1);
            for x in x0..=x1 {
                b.add_chunk(c(x, z));
            }
        }
        for x in -110..200 {
            for z in -45..45 {
                assert_eq!(a.is_chunk_selected(c(x, z)), b.is_chunk_selected(c(x, z)));
            }
        }
        assert_eq!(a.stats(), b.stats());
    }

    #[test]
    fn add_rect_and_chunk_set() {
        let mut s = Selection::new();
        s.add_rect(c(0, 0), c(31, 31));
        assert!(s.is_region_selected(RegionPos::new(0, 0)));
        s.add_rect(c(30, 30), c(33, 33));
        assert!(s.is_chunk_selected(c(33, 33)));
        assert!(!s.is_chunk_selected(c(34, 33)));

        let set: ChunkSet = [1, 2, 3].into_iter().collect();
        s.add_chunk_set(RegionPos::new(5, 5), &set);
        assert_eq!(s.selected_chunks(RegionPos::new(5, 5)), RegionSelection::Some(set));
        s.add_chunk_set(RegionPos::new(6, 5), &ChunkSet::full());
        assert!(s.is_region_selected(RegionPos::new(6, 5)));
    }

    #[test]
    fn stats() {
        let mut s = Selection::new();
        s.add_region(RegionPos::new(0, 0));
        s.add_chunk(c(40, 0));
        s.add_chunk(c(41, 0));
        let stats = s.stats();
        assert_eq!(stats.total_chunks, 1026);
        assert_eq!(stats.full_regions, 1);
        assert_eq!(stats.partial_regions, 1);
        assert_eq!(stats.chunks_in_partial_regions, 2);
    }

    #[test]
    fn csv_round_trip() {
        let csv = "1;-1\n0;0;5;8\n0;0;0;0\n-1;-1;-1;-1\n";
        let s = Selection::from_csv(csv.as_bytes()).unwrap();
        assert!(!s.is_inverted());
        assert!(s.is_region_selected(RegionPos::new(1, -1)));
        assert!(s.is_chunk_selected(c(5, 8)));
        assert!(s.is_chunk_selected(c(0, 0)));
        assert!(s.is_chunk_selected(c(-1, -1)));
        assert!(!s.is_chunk_selected(c(1, 0)));
        assert_eq!(s.to_csv(), "-1;-1;-1;-1\n0;0;0;0\n0;0;5;8\n1;-1\n");
        let again = Selection::from_csv(s.to_csv().as_bytes()).unwrap();
        assert_eq!(again.to_csv(), s.to_csv());
    }

    #[test]
    fn csv_inverted_lists_unselected_chunks() {
        let s = Selection::from_csv("inverted\r\n0;0\r\n1;0;32;0\r\n\r\n".as_bytes()).unwrap();
        assert!(s.is_inverted());
        assert_all_selected(&s, RegionPos::new(0, 0), false);
        assert!(!s.is_chunk_selected(c(32, 0)));
        assert!(s.is_chunk_selected(c(33, 0)));
        assert!(s.is_chunk_selected(c(1000, 1000)));
        assert_eq!(s.to_csv(), "inverted\n0;0\n1;0;32;0\n");
    }

    #[test]
    fn csv_errors() {
        for (bad, needle) in [
            ("1;2;3\n", "line 1"),
            ("0;0\nx;0\n", "line 2"),
            ("0;0;32;0\n", "not in region"),
            ("0;0;a;0\n", "chunk coordinates"),
            ("0;0\ninverted\n", "line 2"),
        ] {
            let err = Selection::from_csv(bad.as_bytes()).unwrap_err().to_string();
            assert!(err.contains(needle), "{bad:?} -> {err}");
        }
        assert!(Selection::from_csv("".as_bytes()).unwrap().is_empty());
    }
}
