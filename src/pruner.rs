//! The pruning pipeline.
//!
//! 1. **Scan** (parallel, read-only): every region file is read and each
//!    chunk's `InhabitedTime` and structure data are extracted.
//! 2. **Select** (in memory): the chunks to keep are collected into a
//!    [`Selection`]: the whitelist, chunks that were inhabited long enough,
//!    a radius around those, and every chunk of every structure that touches
//!    a kept chunk.
//! 3. **Prune** (parallel): all chunks outside of the selection are removed
//!    from the `region`, `poi` and `entities` files. Each file is replaced
//!    atomically, so an interruption at any point leaves a loadable world.
//!
//! Nothing is modified before the scan has completed successfully.

use crate::chunk_set::ChunkSet;
use crate::nbt::{ChunkInfo, read_chunk_info};
use crate::pos::{CHUNKS_PER_REGION, ChunkPos, RegionPos, parse_mcc_file_name};
use crate::region::{Decompressor, HEADER_SIZE, Header, Keep, Location, RewriteOutcome, TEMP_SUFFIX, prune_region_file, read_sectors};
use crate::selection::{RegionSelection, Selection, Stats};
use crate::structures::{ChunkRect, Interner, LocalInterner, StructureIndex, StructureKey, StructureStats, chunk_rects};
use crate::world::Dimension;
use anyhow::{Context, Result, anyhow};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub const MIN_RADIUS: u32 = 0;
pub const MAX_RADIUS: u32 = 128;
/// Game ticks per second, to convert durations to `InhabitedTime`.
pub const TICKS_PER_SECOND: u64 = 20;

/// What to prune and how.
#[derive(Clone, Debug, Default)]
pub struct PruneOptions {
    /// Chunks are kept if their `InhabitedTime` (in ticks) is greater.
    pub inhabited_time: i64,
    /// Additionally keep all chunks within this distance (in chunks) of a
    /// chunk that is kept because of its `InhabitedTime`.
    pub radius: u32,
    /// Chunks that are always kept.
    pub whitelist: Selection,
    /// Keep the whitelist (and structures touching it) and nothing else,
    /// ignoring `inhabited_time` and `radius`.
    pub whitelist_only: bool,
    /// Skip files and chunks that cannot be processed instead of aborting.
    /// Anything that could not be read is kept.
    pub continue_on_error: bool,
    /// Report what would be done without modifying anything.
    pub dry_run: bool,
    /// Number of worker threads; 0 uses all available cores.
    pub threads: usize,
}

/// Receives progress updates. Called from worker threads.
pub trait Progress: Sync {
    /// A new stage with `total` steps begins.
    fn begin(&self, message: &str, total: u64);
    /// `steps` more steps of the current stage are done.
    fn advance(&self, steps: u64);
    /// The current stage is over.
    fn finish(&self);
}

/// A [`Progress`] that ignores everything.
pub struct NoProgress;

impl Progress for NoProgress {
    fn begin(&self, _: &str, _: u64) {}
    fn advance(&self, _: u64) {}
    fn finish(&self) {}
}

/// What happened in one of the directories of a dimension.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct DirReport {
    /// `region`, `poi` or `entities`
    pub name: String,
    pub files: u64,
    pub files_deleted: u64,
    pub files_rewritten: u64,
    pub files_untouched: u64,
    pub chunks_kept: u64,
    pub chunks_removed: u64,
    /// Oversized chunk files (`.mcc`) that were removed.
    pub external_removed: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

/// Summary of a pruning run.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Report {
    pub dry_run: bool,
    /// The run was cancelled; the numbers describe what was done until then.
    pub cancelled: bool,
    pub regions_scanned: u64,
    pub chunks_scanned: u64,
    /// Chunks that could not be read and were therefore kept.
    pub chunks_unreadable: u64,
    /// Chunks that have space reserved in a region file but hold no data.
    /// The game treats them as missing, so they are pruned like chunks that
    /// nobody visited.
    pub chunks_empty: u64,
    pub structures: StructureStats,
    /// The final selection of chunks to keep.
    pub selection: Stats,
    /// Non-fatal errors that were skipped because of `continue_on_error`.
    pub errors: u64,
    pub dirs: Vec<DirReport>,
}

impl Report {
    pub fn chunks_removed(&self) -> u64 {
        self.dirs.iter().find(|d| d.name == "region").map_or(0, |d| d.chunks_removed)
    }

    pub fn bytes_before(&self) -> u64 {
        self.dirs.iter().map(|d| d.bytes_before).sum()
    }

    pub fn bytes_after(&self) -> u64 {
        self.dirs.iter().map(|d| d.bytes_after).sum()
    }
}

/// Collects errors from worker threads and decides whether to go on.
struct Errors<'a> {
    continue_on_error: bool,
    cancel: &'a AtomicBool,
    abort: AtomicBool,
    skipped: AtomicU64,
    fatal: Mutex<Option<anyhow::Error>>,
}

impl Errors<'_> {
    /// Report a problem. With `continue_on_error` it is logged and counted,
    /// otherwise the first one becomes the result of the run.
    fn report(&self, error: anyhow::Error) {
        if self.continue_on_error {
            log::warn!("{error:#}");
            self.skipped.fetch_add(1, Ordering::Relaxed);
        } else {
            self.abort.store(true, Ordering::Relaxed);
            let mut fatal = self.fatal.lock().unwrap_or_else(|e| e.into_inner());
            if fatal.is_none() {
                *fatal = Some(error);
            }
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// True if workers should not start any more work.
    fn stop(&self) -> bool {
        self.abort.load(Ordering::Relaxed) || self.cancelled()
    }

    fn take_fatal(&self) -> Option<anyhow::Error> {
        self.fatal.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// The region and oversized-chunk files of one directory.
#[derive(Default)]
struct DirIndex {
    regions: Vec<RegionPos>,
    external: FxHashSet<ChunkPos>,
}

/// List a directory. Temporary files that an interrupted earlier run left
/// behind are removed if `clean` is set.
fn index_dir(dir: &Path, clean: bool) -> Result<DirIndex> {
    let mut index = DirIndex::default();
    let entries = fs::read_dir(dir).with_context(|| format!("failed to list {}", dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("failed to list {}", dir.display()))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(region) = RegionPos::from_file_name(name) {
            index.regions.push(region);
        } else if let Some(chunk) = parse_mcc_file_name(name) {
            index.external.insert(chunk);
        } else if clean && name.strip_suffix(TEMP_SUFFIX).is_some_and(|original| RegionPos::from_file_name(original).is_some()) {
            log::info!("removing leftover temporary file {}", entry.path().display());
            let _ = fs::remove_file(entry.path());
        }
    }
    index.regions.sort_unstable();
    Ok(index)
}

/// What the scan found in one region file.
struct RegionScan {
    region: RegionPos,
    /// The file could not be read at all, so all of it is kept.
    failed: bool,
    /// Chunks that are kept on their own merit: because of their
    /// `InhabitedTime`, or in whitelist-only mode because they are whitelisted.
    kept: ChunkSet,
    /// Chunks that could not be read. They are kept to be safe.
    unreadable: ChunkSet,
    chunks: u64,
    /// Chunks without any data, see [`Report::chunks_empty`].
    empty: u64,
    starts: Vec<(StructureKey, Box<[ChunkRect]>)>,
    keep_structures: Vec<StructureKey>,
    ignored_boxes: usize,
}

impl RegionScan {
    fn new(region: RegionPos) -> Self {
        RegionScan {
            region,
            failed: false,
            kept: ChunkSet::new(),
            unreadable: ChunkSet::new(),
            chunks: 0,
            starts: Vec::new(),
            keep_structures: Vec::new(),
            empty: 0,
            ignored_boxes: 0,
        }
    }
}

/// Per-thread buffers, reused across region files.
struct Scratch<'a> {
    /// The raw sectors of the chunk that is being read.
    sectors: Vec<u8>,
    decompressor: Decompressor,
    info: ChunkInfo,
    interner: LocalInterner<'a>,
}

struct Scanner<'a> {
    dir: &'a Path,
    options: &'a PruneOptions,
    errors: &'a Errors<'a>,
}

impl Scanner<'_> {
    /// A chunk is kept on its own merit if this returns true.
    fn keeps(&self, chunk: ChunkPos, info: &ChunkInfo) -> bool {
        if self.options.whitelist_only {
            self.options.whitelist.is_chunk_selected(chunk)
        } else {
            // Chunks without an InhabitedTime are kept to be safe.
            info.inhabited_time.is_none_or(|time| time > self.options.inhabited_time)
        }
    }

    fn scan(&self, region: RegionPos, scratch: &mut Scratch<'_>) -> Option<RegionScan> {
        let path = self.dir.join(region.file_name());
        let Scratch { sectors, decompressor, info, interner } = scratch;
        let mut scan = RegionScan::new(region);

        let opened = File::open(&path).and_then(|mut file| {
            let len = file.metadata()?.len();
            // A file without chunk data has nothing to scan.
            let header = if len > HEADER_SIZE as u64 { Some(Header::read_from(&mut file)?) } else { None };
            Ok((file, header, len))
        });
        let (mut file, header) = match opened {
            Ok((file, Some(header), _)) => (file, header),
            Ok((_, None, len)) => {
                log::debug!("skipped empty region file {} ({len} bytes)", path.display());
                return None;
            }
            Err(e) => {
                self.errors.report(anyhow!(e).context(format!("failed to read {}", path.display())));
                scan.failed = true;
                return Some(scan);
            }
        };

        // Visit the chunks in file order, so the file is read sequentially.
        let mut chunks: Vec<(Location, usize)> = (0..CHUNKS_PER_REGION).map(|index| (header.location(index), index)).filter(|(l, _)| l.exists()).collect();
        chunks.sort_unstable_by_key(|(location, _)| location.offset);
        for (location, index) in chunks {
            if self.errors.stop() {
                return None;
            }
            scan.chunks += 1;
            let chunk = region.chunk(index);
            // `Ok(false)`: the chunk has no data at all
            let read = (|| -> Result<bool> {
                read_sectors(&mut file, location, sectors)?;
                let Some(reader) = decompressor.open(sectors, region, index, self.dir)? else { return Ok(false) };
                read_chunk_info(reader, info)?;
                Ok(true)
            })();
            match read {
                Ok(true) => {}
                Ok(false) => {
                    // Nothing to lose and nothing to inspect: it is neither
                    // kept on its own merit nor an error.
                    log::debug!("chunk {chunk} in {} is allocated but holds no data", path.display());
                    scan.empty += 1;
                    continue;
                }
                Err(e) => {
                    self.errors.report(e.context(format!("failed to read chunk {chunk} in {}", path.display())));
                    scan.unreadable.set(index);
                    continue;
                }
            }

            let keep = self.keeps(chunk, info);
            for start in info.starts.drain(..) {
                let key = StructureKey { id: interner.intern(&start.id), start: chunk.to_long() };
                let (rects, ignored) = chunk_rects(&start.boxes);
                scan.ignored_boxes += ignored;
                scan.starts.push((key, rects));
                if keep {
                    scan.keep_structures.push(key);
                }
            }
            if keep {
                scan.kept.set(index);
                for (id, starts) in &info.references {
                    let id = interner.intern(id);
                    scan.keep_structures.extend(starts.iter().map(|&start| StructureKey { id, start }));
                }
            }
        }
        scan.keep_structures.sort_unstable();
        scan.keep_structures.dedup();
        if scan.ignored_boxes > 0 {
            log::warn!("{}: ignored {} structure pieces with implausible bounding boxes", path.display(), scan.ignored_boxes);
        }
        log::debug!("scanned {}: {} chunks, {} kept, {} unreadable", path.display(), scan.chunks, scan.kept.len(), scan.unreadable.len());
        Some(scan)
    }
}

/// Add every chunk within `radius` chunks of a chunk in `kept` to
/// `selection`.
///
/// Only chunks on the border of the kept area are expanded: the disc around
/// an interior chunk is always covered by the kept area itself plus the discs
/// around border chunks. Each run of border chunks then turns into one range
/// per row, which keeps this fast even for the maximum radius.
fn apply_radius(selection: &mut Selection, kept: &FxHashMap<RegionPos, ChunkSet>, radius: u32) {
    if radius == 0 {
        return;
    }
    let radius = radius.min(MAX_RADIUS) as i32;
    // half-width of the disc in each row
    let widths: Vec<(i32, i32)> = (-radius..=radius).map(|dz| (dz, ((radius * radius - dz * dz) as u32).isqrt() as i32)).collect();
    let empty = ChunkSet::new();
    let neighbour = |x: i32, z: i32| kept.get(&RegionPos::new(x, z)).unwrap_or(&empty);

    for (&region, set) in kept {
        let (west, east) = (neighbour(region.x - 1, region.z), neighbour(region.x + 1, region.z));
        let (north, south) = (neighbour(region.x, region.z - 1), neighbour(region.x, region.z + 1));
        let origin = region.chunk(0);
        for z in 0..32 {
            let row = set.row(z);
            if row == 0 {
                continue;
            }
            let has_west = (row << 1) | (west.row(z) >> 31);
            let has_east = (row >> 1) | (east.row(z) << 31);
            let has_north = if z > 0 { set.row(z - 1) } else { north.row(31) };
            let has_south = if z < 31 { set.row(z + 1) } else { south.row(0) };
            let mut border = row & !(has_west & has_east & has_north & has_south);
            let chunk_z = origin.z + z as i32;
            while border != 0 {
                let first = border.trailing_zeros();
                let len = (border >> first).trailing_ones();
                border &= !(u32::MAX >> (32 - len) << first);
                let x0 = origin.x + first as i32;
                let x1 = x0 + len as i32 - 1;
                for &(dz, width) in &widths {
                    selection.add_row_range(chunk_z.saturating_add(dz), x0.saturating_sub(width), x1.saturating_add(width));
                }
            }
        }
    }
}

/// Prune one dimension.
///
/// `cancel` can be set from another thread to stop early. Returns an error
/// if something went wrong and `continue_on_error` is off; in that case
/// files processed so far stay pruned, everything else is untouched, and the
/// world remains consistent.
pub fn prune(dimension: &Dimension, options: &PruneOptions, progress: &dyn Progress, cancel: &AtomicBool) -> Result<Report> {
    let mut builder = rayon::ThreadPoolBuilder::new().thread_name(|i| format!("pruner-{i}"));
    if options.threads > 0 {
        builder = builder.num_threads(options.threads);
    }
    let pool = builder.build().context("failed to start worker threads")?;
    pool.install(|| prune_in_pool(dimension, options, progress, cancel))
}

fn prune_in_pool(dimension: &Dimension, options: &PruneOptions, progress: &dyn Progress, cancel: &AtomicBool) -> Result<Report> {
    let errors =
        Errors { continue_on_error: options.continue_on_error, cancel, abort: AtomicBool::new(false), skipped: AtomicU64::new(0), fatal: Mutex::new(None) };
    let mut report = Report { dry_run: options.dry_run, ..Report::default() };
    log::info!(
        "pruning {}: inhabited_time={} radius={} whitelist=[{}] whitelist_only={} continue_on_error={} dry_run={} threads={}",
        dimension.region.display(),
        options.inhabited_time,
        options.radius,
        options.whitelist.stats(),
        options.whitelist_only,
        options.continue_on_error,
        options.dry_run,
        rayon::current_num_threads()
    );

    // ---- index ----
    let mut dirs: Vec<(&str, &PathBuf, DirIndex)> = Vec::new();
    for (name, dir) in [("region", Some(&dimension.region)), ("poi", dimension.poi.as_ref()), ("entities", dimension.entities.as_ref())] {
        if let Some(dir) = dir {
            dirs.push((name, dir, index_dir(dir, !options.dry_run)?));
        }
    }

    // ---- scan ----
    let whitelist = &options.whitelist;
    let to_scan: Vec<RegionPos> = dirs[0]
        .2
        .regions
        .iter()
        .copied()
        // In whitelist-only mode, regions without whitelisted chunks are
        // removed entirely and do not need to be looked at.
        .filter(|&region| !options.whitelist_only || whitelist.is_any_chunk_in_region_selected(region))
        .collect();
    progress.begin("Scanning chunks", to_scan.len() as u64);
    let interner = Interner::default();
    let scanner = Scanner { dir: &dimension.region, options, errors: &errors };
    let scans: Vec<RegionScan> = to_scan
        .par_iter()
        .map_init(
            || Scratch { sectors: Vec::new(), decompressor: Decompressor::default(), info: ChunkInfo::default(), interner: LocalInterner::new(&interner) },
            |scratch, &region| {
                let scan = if errors.stop() { None } else { scanner.scan(region, scratch) };
                progress.advance(1);
                scan
            },
        )
        .flatten()
        .collect();
    progress.finish();
    if let Some(error) = errors.take_fatal() {
        return Err(error.context("nothing was changed"));
    }
    if errors.cancelled() {
        report.cancelled = true;
        return Ok(report);
    }

    // ---- select ----
    let mut selection = whitelist.clone();
    let mut structures = StructureIndex::default();
    let mut kept_by_time: FxHashMap<RegionPos, ChunkSet> = FxHashMap::default();
    for scan in scans {
        report.regions_scanned += 1;
        report.chunks_scanned += scan.chunks;
        report.chunks_empty += scan.empty;
        report.chunks_unreadable += scan.unreadable.len() as u64;
        if scan.failed {
            selection.add_region(scan.region);
        }
        selection.add_chunk_set(scan.region, &scan.unreadable);
        structures.add_starts(scan.starts);
        structures.keep(scan.keep_structures);
        if !options.whitelist_only && !scan.kept.is_empty() {
            selection.add_chunk_set(scan.region, &scan.kept);
            if options.radius > 0 {
                kept_by_time.insert(scan.region, scan.kept);
            }
        }
    }
    apply_radius(&mut selection, &kept_by_time, options.radius);
    drop(kept_by_time);
    report.structures = structures.select_kept(&mut selection, &interner.names());
    drop(structures);
    report.selection = selection.stats();
    log::info!("keeping {} structures ({} referenced structures have no start)", report.structures.kept, report.structures.missing);
    log::info!("selection of chunks to keep: {}", report.selection);

    // ---- prune ----
    let tasks: Vec<(usize, RegionPos)> = dirs.iter().enumerate().flat_map(|(i, (_, _, index))| index.regions.iter().map(move |&r| (i, r))).collect();
    progress.begin(if options.dry_run { "Checking files" } else { "Pruning files" }, tasks.len() as u64);
    let outcomes: Vec<(usize, RewriteOutcome)> = tasks
        .par_iter()
        .filter_map(|&(dir_index, region)| {
            let outcome = if errors.stop() { None } else { prune_file(&dirs[dir_index], region, &selection, options.dry_run, &errors) };
            progress.advance(1);
            outcome.map(|o| (dir_index, o))
        })
        .collect();
    progress.finish();

    report.dirs = dirs.iter().map(|(name, _, index)| DirReport { name: name.to_string(), files: index.regions.len() as u64, ..DirReport::default() }).collect();
    for (dir_index, outcome) in outcomes {
        let dir = &mut report.dirs[dir_index];
        dir.files_deleted += u64::from(outcome.deleted_file);
        dir.files_untouched += u64::from(outcome.untouched);
        dir.files_rewritten += u64::from(!outcome.deleted_file && !outcome.untouched);
        dir.chunks_kept += u64::from(outcome.kept_chunks);
        dir.chunks_removed += u64::from(outcome.removed_chunks);
        dir.external_removed += u64::from(outcome.removed_external);
        dir.bytes_before += outcome.bytes_before;
        dir.bytes_after += outcome.bytes_after;
    }
    report.errors = errors.skipped.load(Ordering::Relaxed);
    report.cancelled = errors.cancelled();
    for dir in &report.dirs {
        log::info!("{}: {dir:?}", dimension.region.with_file_name(&dir.name).display());
    }
    if let Some(error) = errors.take_fatal() {
        return Err(error.context("pruning was aborted; files that were already processed stay pruned"));
    }
    Ok(report)
}

fn prune_file(dir: &(&str, &PathBuf, DirIndex), region: RegionPos, selection: &Selection, dry_run: bool, errors: &Errors<'_>) -> Option<RewriteOutcome> {
    let (_, dir, index) = dir;
    let path = dir.join(region.file_name());
    let has_external = |i: usize| index.external.contains(&region.chunk(i));
    let result = match selection.selected_chunks(region) {
        RegionSelection::All => {
            // Fully kept regions are not even opened.
            let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            return Some(RewriteOutcome { untouched: true, bytes_before: size, bytes_after: size, ..RewriteOutcome::default() });
        }
        RegionSelection::None => prune_region_file(&path, region, Keep::Nothing, &has_external, dry_run),
        RegionSelection::Some(keep) => prune_region_file(&path, region, Keep::Only(&keep), &has_external, dry_run),
    };
    match result {
        Ok(outcome) => {
            log::debug!("pruned {}: {outcome:?}", path.display());
            Some(outcome)
        }
        Err(e) => {
            errors.report(e);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Straightforward reference implementation: a disc around every chunk.
    fn naive_radius(selection: &mut Selection, kept: &FxHashMap<RegionPos, ChunkSet>, radius: i32) {
        for (region, set) in kept {
            for index in set.iter() {
                let c = region.chunk(index);
                for dx in -radius..=radius {
                    for dz in -radius..=radius {
                        if dx * dx + dz * dz <= radius * radius {
                            selection.add_chunk(ChunkPos::new(c.x + dx, c.z + dz));
                        }
                    }
                }
            }
        }
    }

    fn kept_from(chunks: &[ChunkPos]) -> FxHashMap<RegionPos, ChunkSet> {
        let mut kept: FxHashMap<RegionPos, ChunkSet> = FxHashMap::default();
        for c in chunks {
            kept.entry(c.region()).or_default().set(c.index());
        }
        kept
    }

    fn assert_same(kept: &FxHashMap<RegionPos, ChunkSet>, radius: u32) {
        let mut fast = Selection::new();
        let mut naive = Selection::new();
        for (region, set) in kept {
            fast.add_chunk_set(*region, set);
            naive.add_chunk_set(*region, set);
        }
        apply_radius(&mut fast, kept, radius);
        naive_radius(&mut naive, kept, radius as i32);
        assert_eq!(fast.to_csv(), naive.to_csv(), "radius {radius}");
    }

    #[test]
    fn radius_around_single_chunks() {
        for radius in [0, 1, 2, 3, 7, 16, 40] {
            assert_same(&kept_from(&[ChunkPos::new(0, 0)]), radius);
            assert_same(&kept_from(&[ChunkPos::new(-1, -1), ChunkPos::new(31, 32), ChunkPos::new(100, -70)]), radius);
        }
    }

    #[test]
    fn radius_around_blobs_matches_naive_implementation() {
        let mut seed = 99u32;
        let mut next = move |n: i32| {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            ((seed >> 8) as i32).rem_euclid(n)
        };
        for radius in [1, 2, 5, 9] {
            let mut chunks = Vec::new();
            // solid rectangles crossing region borders (lots of interior chunks) ...
            for _ in 0..6 {
                let (x, z) = (next(120) - 60, next(120) - 60);
                let (w, h) = (next(45) + 1, next(45) + 1);
                for dx in 0..w {
                    for dz in 0..h {
                        chunks.push(ChunkPos::new(x + dx, z + dz));
                    }
                }
            }
            // ... and some scattered single chunks
            for _ in 0..40 {
                chunks.push(ChunkPos::new(next(200) - 100, next(200) - 100));
            }
            assert_same(&kept_from(&chunks), radius);
        }
    }

    #[test]
    fn radius_of_full_regions() {
        let mut kept = FxHashMap::default();
        kept.insert(RegionPos::new(0, 0), ChunkSet::full());
        kept.insert(RegionPos::new(1, 0), ChunkSet::full());
        kept.insert(RegionPos::new(0, -1), ChunkSet::full());
        assert_same(&kept, 3);
    }
}
