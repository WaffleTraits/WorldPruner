//! End-to-end tests on synthetic worlds.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use worldpruner::nbt::write::Compound;
use worldpruner::pos::{ChunkPos, RegionPos, mcc_file_name};
use worldpruner::pruner::{NoProgress, PruneOptions, Report, prune};
use worldpruner::region::testing::{Storage, TempDir, read_chunk, write_region};
use worldpruner::region::{HEADER_SIZE, Header};
use worldpruner::selection::Selection;
use worldpruner::world::{Dimension, resolve_dimensions};

/// A structure: id, start chunk and block-coordinate boxes `[x0, z0, x1, z1]`.
#[derive(Clone, Debug)]
struct Structure {
    id: &'static str,
    start: ChunkPos,
    boxes: Vec<[i32; 4]>,
}

#[derive(Clone, Debug, Default)]
struct TestChunk {
    inhabited: Option<i64>,
    /// structures starting here
    starts: Vec<Structure>,
    /// structures reaching into this chunk
    references: Vec<(&'static str, ChunkPos)>,
    legacy: bool,
    storage: Option<Storage>,
}

fn chunk(inhabited: i64) -> TestChunk {
    TestChunk { inhabited: Some(inhabited), ..TestChunk::default() }
}

impl TestChunk {
    fn nbt(&self, pos: ChunkPos) -> Vec<u8> {
        let mut starts = Compound::new();
        for s in &self.starts {
            let children = s
                .boxes
                .iter()
                .map(|b| Compound::new().string("id", "minecraft:piece").int_array("BB", &[b[0], 60, b[1], b[2], 80, b[3]]).int("O", 0))
                .collect();
            starts = starts.compound(s.id, Compound::new().string("id", s.id).int("ChunkX", pos.x).int("ChunkZ", pos.z).compound_list("Children", children));
        }
        // the game also stores placeholders for structures that do not start here
        starts = starts.compound("minecraft:placeholder", Compound::new().string("id", "INVALID"));
        let mut references: BTreeMap<&str, Vec<i64>> = BTreeMap::new();
        for (id, start) in &self.references {
            references.entry(id).or_default().push(start.to_long());
        }
        let mut refs = Compound::new();
        for (id, starts) in references {
            refs = refs.long_array(id, &starts);
        }
        // some bulk, so chunks span a varying number of sectors
        let filler: Vec<u8> = (0..(pos.x.rem_euclid(7) * 1500 + 300) as usize).map(|i| (i as i32 * 7 + pos.z) as u8).collect();
        if self.legacy {
            let mut level = Compound::new().int("xPos", pos.x).int("zPos", pos.z).byte_array("Biomes", &filler);
            if let Some(t) = self.inhabited {
                level = level.long("InhabitedTime", t);
            }
            level = level.compound("Structures", Compound::new().compound("Starts", starts).compound("References", refs));
            Compound::new().int("DataVersion", 2586).compound("Level", level).into_root()
        } else {
            let mut root = Compound::new().int("DataVersion", 4790).int("xPos", pos.x).int("zPos", pos.z).string("Status", "minecraft:full");
            root = root.compound("structures", Compound::new().compound("starts", starts).compound("References", refs));
            root = root.byte_array("filler", &filler);
            if let Some(t) = self.inhabited {
                root = root.long("InhabitedTime", t);
            }
            root.into_root()
        }
    }
}

/// A synthetic dimension on disk.
struct World {
    _tmp: TempDir,
    root: PathBuf,
    chunks: BTreeMap<ChunkPos, TestChunk>,
}

impl World {
    /// Write `chunks` to `region/`, and a small entry for each chunk to
    /// `poi/` and `entities/`.
    fn create(label: &str, chunks: BTreeMap<ChunkPos, TestChunk>) -> World {
        let tmp = TempDir::new(label);
        let root = tmp.path().to_path_buf();
        let mut by_region: BTreeMap<RegionPos, Vec<(usize, Vec<u8>, Storage)>> = BTreeMap::new();
        let mut side: BTreeMap<RegionPos, Vec<(usize, Vec<u8>, Storage)>> = BTreeMap::new();
        for (pos, c) in &chunks {
            by_region.entry(pos.region()).or_default().push((pos.index(), c.nbt(*pos), c.storage.unwrap_or(Storage::Zlib)));
            let marker = Compound::new().int("x", pos.x).int("z", pos.z).into_root();
            side.entry(pos.region()).or_default().push((pos.index(), marker, Storage::Zlib));
        }
        for (region, list) in &by_region {
            write_region(&root.join("region"), *region, list);
        }
        for dir in ["poi", "entities"] {
            for (region, list) in &side {
                write_region(&root.join(dir), *region, list);
            }
        }
        World { _tmp: tmp, root, chunks }
    }

    fn dimension(&self) -> Dimension {
        Dimension::from_dir("test", &self.root).unwrap()
    }

    fn prune(&self, options: &PruneOptions) -> anyhow::Result<Report> {
        prune(&self.dimension(), options, &NoProgress, &AtomicBool::new(false))
    }

    /// The chunks present in a directory, checking that each still has its
    /// original content.
    fn present(&self, dir: &str) -> BTreeSet<ChunkPos> {
        let mut present = BTreeSet::new();
        let Ok(entries) = fs::read_dir(self.root.join(dir)) else { return present };
        for entry in entries.flatten() {
            let name = entry.file_name().into_string().unwrap();
            let Some(region) = RegionPos::from_file_name(&name) else {
                assert!(name.ends_with(".mcc"), "unexpected file {name} left behind");
                continue;
            };
            let bytes = fs::read(entry.path()).unwrap();
            assert!(bytes.len() > HEADER_SIZE, "{name} should have been deleted");
            assert_eq!(bytes.len() % 4096, 0);
            let header = Header::parse(&bytes).unwrap();
            for index in header.present().iter() {
                let pos = region.chunk(index);
                let content = read_chunk(&entry.path(), index).unwrap();
                if dir == "region" {
                    assert_eq!(content, self.chunks[&pos].nbt(pos), "content of chunk {pos} changed");
                } else {
                    assert_eq!(content, Compound::new().int("x", pos.x).int("z", pos.z).into_root());
                }
                present.insert(pos);
            }
        }
        present
    }

    /// Fingerprint of every file, to prove that nothing was modified.
    fn snapshot(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut files = BTreeMap::new();
        for dir in ["region", "poi", "entities"] {
            for entry in fs::read_dir(self.root.join(dir)).into_iter().flatten().flatten() {
                files.insert(entry.path(), fs::read(entry.path()).unwrap());
            }
        }
        files
    }
}

fn set(chunks: &[(i32, i32)]) -> BTreeSet<ChunkPos> {
    chunks.iter().map(|&(x, z)| ChunkPos::new(x, z)).collect()
}

fn world_of(label: &str, chunks: Vec<((i32, i32), TestChunk)>) -> World {
    World::create(label, chunks.into_iter().map(|((x, z), c)| (ChunkPos::new(x, z), c)).collect())
}

fn by_time(inhabited_time: i64) -> PruneOptions {
    PruneOptions { inhabited_time, threads: 4, ..PruneOptions::default() }
}

fn whitelist(csv: &str) -> Selection {
    Selection::from_csv(csv.as_bytes()).unwrap()
}

#[test]
fn prunes_by_inhabited_time() {
    let world = world_of(
        "time",
        vec![
            ((0, 0), chunk(1000)),
            ((1, 0), chunk(100)), // not greater than the threshold
            ((5, 5), chunk(0)),
            ((31, 31), chunk(101)),
            ((6, 6), TestChunk::default()), // no InhabitedTime: kept to be safe
            ((40, 0), chunk(5)),            // region 1|0 loses all chunks
            ((-1, -1), chunk(i64::MAX)),
        ],
    );
    let report = world.prune(&by_time(100)).unwrap();
    let expected = set(&[(0, 0), (31, 31), (6, 6), (-1, -1)]);
    assert_eq!(world.present("region"), expected);
    assert_eq!(world.present("poi"), expected);
    assert_eq!(world.present("entities"), expected);
    assert!(!world.root.join("region/r.1.0.mca").exists());
    assert!(!world.root.join("entities/r.1.0.mca").exists());

    assert_eq!(report.regions_scanned, 3);
    assert_eq!(report.chunks_scanned, 7);
    assert_eq!(report.chunks_removed(), 3);
    assert_eq!(report.errors, 0);
    assert_eq!(report.dirs.len(), 3);
    let region = &report.dirs[0];
    assert_eq!((region.files, region.files_deleted, region.files_rewritten, region.files_untouched), (3, 1, 1, 1));
    assert_eq!((region.chunks_kept, region.chunks_removed), (4, 3));
    assert!(report.bytes_after() < report.bytes_before());
    let on_disk: u64 = world.snapshot().values().map(|v| v.len() as u64).sum();
    assert_eq!(report.bytes_after(), on_disk);

    // pruning again changes nothing
    let before = world.snapshot();
    let again = world.prune(&by_time(100)).unwrap();
    assert_eq!(again.chunks_removed(), 0);
    assert_eq!(world.snapshot(), before);
}

#[test]
fn default_time_removes_only_never_inhabited_chunks() {
    let world = world_of("zero", vec![((0, 0), chunk(0)), ((1, 0), chunk(1)), ((2, 0), chunk(-5))]);
    world.prune(&by_time(0)).unwrap();
    assert_eq!(world.present("region"), set(&[(1, 0)]));
}

#[test]
fn keeps_radius_around_inhabited_chunks() {
    let mut chunks = Vec::new();
    for x in -8..8 {
        for z in -8..8 {
            chunks.push(((x, z), chunk(if (x, z) == (0, 0) || (x, z) == (5, -6) { 500 } else { 0 })));
        }
    }
    let world = world_of("radius", chunks);
    world.prune(&PruneOptions { radius: 2, ..by_time(100) }).unwrap();
    let mut expected = BTreeSet::new();
    for (cx, cz) in [(0, 0), (5, -6)] {
        for dx in -2i32..=2 {
            for dz in -2i32..=2 {
                let (x, z) = (cx + dx, cz + dz);
                if dx * dx + dz * dz <= 4 && (-8..8).contains(&x) && (-8..8).contains(&z) {
                    expected.insert(ChunkPos::new(x, z));
                }
            }
        }
    }
    assert_eq!(expected.len(), 2 * 13);
    assert_eq!(world.present("region"), expected);
    assert_eq!(world.present("entities"), expected);
}

fn village() -> Structure {
    // blocks 320..=400 x 320..=340: chunks 20..=25 x 20..=21, plus the margin
    Structure { id: "minecraft:village_plains", start: ChunkPos::new(20, 20), boxes: vec![[320, 320, 360, 330], [350, 325, 400, 340]] }
}

/// The chunks of the village's two pieces, each with a margin of one chunk.
fn village_chunks() -> BTreeSet<ChunkPos> {
    let mut chunks = rect(19..=23, 19..=21);
    chunks.extend(rect(20..=26, 19..=22));
    chunks
}

fn structure_world(label: &str, visited: i64) -> World {
    let village = village();
    let mineshaft = Structure { id: "minecraft:mineshaft", start: ChunkPos::new(40, 40), boxes: vec![[640, 640, 700, 700]] };
    let mut chunks: BTreeMap<ChunkPos, TestChunk> = BTreeMap::new();
    for x in 15..50 {
        for z in 15..50 {
            chunks.insert(ChunkPos::new(x, z), chunk(0));
        }
    }
    chunks.get_mut(&village.start).unwrap().starts.push(village.clone());
    chunks.get_mut(&mineshaft.start).unwrap().starts.push(mineshaft.clone());
    for x in 20..=25 {
        for z in 20..=21 {
            chunks.get_mut(&ChunkPos::new(x, z)).unwrap().references.push((village.id, village.start));
        }
    }
    for x in 40..=43 {
        for z in 40..=43 {
            chunks.get_mut(&ChunkPos::new(x, z)).unwrap().references.push((mineshaft.id, mineshaft.start));
        }
    }
    // a player visited one far corner of the village
    chunks.get_mut(&ChunkPos::new(25, 21)).unwrap().inhabited = Some(visited);
    // this chunk refers to a structure whose start does not exist
    let lonely = chunks.get_mut(&ChunkPos::new(48, 16)).unwrap();
    lonely.inhabited = Some(visited);
    lonely.references.push(("minecraft:stronghold", ChunkPos::new(500, 500)));
    World::create(label, chunks)
}

fn rect(x: std::ops::RangeInclusive<i32>, z: std::ops::RangeInclusive<i32>) -> BTreeSet<ChunkPos> {
    x.flat_map(|x| z.clone().map(move |z| ChunkPos::new(x, z))).collect()
}

#[test]
fn keeps_whole_structures_that_touch_kept_chunks() {
    let world = structure_world("structures", 5000);
    let report = world.prune(&by_time(100)).unwrap();
    let mut expected = village_chunks();
    expected.insert(ChunkPos::new(48, 16));
    assert_eq!(world.present("region"), expected);
    assert_eq!(world.present("poi"), expected);
    assert_eq!(report.structures.kept, 1);
    assert_eq!(report.structures.missing, 1);
}

#[test]
fn structure_that_starts_in_a_kept_chunk_is_kept() {
    let world = world_of(
        "start",
        vec![
            ((0, 0), TestChunk { starts: vec![Structure { id: "a:b", start: ChunkPos::new(0, 0), boxes: vec![[0, 0, 15, 40]] }], ..chunk(900) }),
            ((0, 2), chunk(0)),
            ((0, 3), chunk(0)),
            ((0, 4), chunk(0)),
            ((2, 0), chunk(0)),
        ],
    );
    world.prune(&by_time(100)).unwrap();
    assert_eq!(world.present("region"), set(&[(0, 0), (0, 2), (0, 3)]));
}

#[test]
fn whitelist_is_always_kept() {
    let world = structure_world("whitelist", 5000);
    // one chunk, and all of region 1|1 (chunks 32..=63)
    let options = PruneOptions { whitelist: whitelist("0;0;17;17\n1;1\n"), ..by_time(100) };
    world.prune(&options).unwrap();
    let mut expected = village_chunks();
    expected.insert(ChunkPos::new(48, 16));
    expected.insert(ChunkPos::new(17, 17));
    expected.extend(rect(32..=49, 32..=49));
    assert_eq!(world.present("region"), expected);
    assert_eq!(world.present("entities"), expected);
}

#[test]
fn whitelist_only_ignores_inhabited_time() {
    let world = structure_world("whitelist-only", 5000);
    // chunk 24|20 is part of the village, 17|17 is plain terrain
    let options = PruneOptions { whitelist: whitelist("0;0;24;20\n0;0;17;17\n"), whitelist_only: true, radius: 5, ..by_time(0) };
    let report = world.prune(&options).unwrap();
    let mut expected = village_chunks();
    expected.insert(ChunkPos::new(17, 17));
    assert_eq!(world.present("region"), expected);
    assert_eq!(world.present("poi"), expected);
    // regions without whitelisted chunks are not even scanned
    assert_eq!(report.regions_scanned, 1);
    assert!(!world.root.join("region/r.1.1.mca").exists());
    assert!(!world.root.join("entities/r.1.0.mca").exists());
}

#[test]
fn inverted_whitelist() {
    let world = world_of("inverted", vec![((0, 0), chunk(0)), ((1, 0), chunk(0)), ((2, 0), chunk(900)), ((40, 0), chunk(0)), ((70, 0), chunk(0))]);
    // everything is whitelisted except region 0|0 and chunk 40|0
    let options = PruneOptions { whitelist: whitelist("inverted\n0;0\n1;0;40;0\n"), ..by_time(100) };
    world.prune(&options).unwrap();
    assert_eq!(world.present("region"), set(&[(2, 0), (70, 0)]));

    let world = world_of("inverted-only", vec![((0, 0), chunk(0)), ((1, 0), chunk(0)), ((2, 0), chunk(900)), ((40, 0), chunk(0)), ((41, 0), chunk(0))]);
    let options = PruneOptions { whitelist: whitelist("inverted\n0;0\n1;0;40;0\n"), whitelist_only: true, ..by_time(100) };
    world.prune(&options).unwrap();
    assert_eq!(world.present("region"), set(&[(41, 0)]));
}

#[test]
fn reads_legacy_chunks_and_every_compression() {
    let s = Structure { id: "Village", start: ChunkPos::new(3, 3), boxes: vec![[48, 48, 60, 60]] };
    let world = world_of(
        "formats",
        vec![
            ((0, 0), TestChunk { legacy: true, storage: Some(Storage::Gzip), ..chunk(900) }),
            ((1, 0), TestChunk { legacy: true, ..chunk(5) }),
            ((3, 3), TestChunk { legacy: true, starts: vec![s.clone()], ..chunk(0) }),
            ((4, 4), TestChunk { legacy: true, references: vec![(s.id, s.start)], ..chunk(0) }),
            ((3, 4), TestChunk { legacy: true, references: vec![(s.id, s.start)], ..chunk(700) }),
            ((10, 0), TestChunk { storage: Some(Storage::Lz4), ..chunk(900) }),
            ((11, 0), TestChunk { storage: Some(Storage::Lz4), ..chunk(1) }),
            ((12, 0), TestChunk { storage: Some(Storage::Plain), ..chunk(900) }),
            ((13, 0), TestChunk { storage: Some(Storage::Plain), ..chunk(1) }),
            ((14, 0), TestChunk { storage: Some(Storage::External), ..chunk(900) }),
            ((15, 0), TestChunk { storage: Some(Storage::External), ..chunk(1) }),
        ],
    );
    let report = world.prune(&by_time(100)).unwrap();
    assert_eq!(world.present("region"), set(&[(0, 0), (3, 3), (4, 4), (3, 4), (10, 0), (12, 0), (14, 0)]));
    assert_eq!(report.errors, 0);
    // the oversized data of the removed chunk is gone, the other one stays
    assert!(world.root.join("region").join(mcc_file_name(ChunkPos::new(14, 0))).exists());
    assert!(!world.root.join("region").join(mcc_file_name(ChunkPos::new(15, 0))).exists());
    assert_eq!(report.dirs[0].external_removed, 1);
}

/// Overwrite the compressed data of a chunk with garbage.
fn corrupt_chunk(world: &World, pos: ChunkPos) {
    let path = world.root.join("region").join(pos.region().file_name());
    let mut bytes = fs::read(&path).unwrap();
    let location = Header::parse(&bytes).unwrap().location(pos.index());
    let start = location.offset as usize * 4096;
    bytes[start + 5..start + 200].fill(0x55);
    fs::write(&path, bytes).unwrap();
}

#[test]
fn unreadable_chunk_aborts_before_anything_is_changed() {
    let world = world_of("abort", vec![((0, 0), chunk(0)), ((1, 0), chunk(0)), ((2, 0), chunk(900)), ((40, 0), chunk(0))]);
    corrupt_chunk(&world, ChunkPos::new(1, 0));
    let before = world.snapshot();
    let error = world.prune(&by_time(100)).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("failed to read chunk 1|0"), "{message}");
    assert!(message.contains("r.0.0.mca"), "{message}");
    assert_eq!(world.snapshot(), before);
}

#[test]
fn unreadable_chunk_is_kept_when_continuing() {
    let world = world_of("continue", vec![((0, 0), chunk(0)), ((1, 0), chunk(0)), ((2, 0), chunk(900)), ((40, 0), chunk(0))]);
    corrupt_chunk(&world, ChunkPos::new(1, 0));
    let corrupted = fs::read(world.root.join("region/r.0.0.mca")).unwrap();
    let location = Header::parse(&corrupted).unwrap().location(1);
    let raw = corrupted[location.offset as usize * 4096..][..location.sectors as usize * 4096].to_vec();

    let report = world.prune(&PruneOptions { continue_on_error: true, ..by_time(100) }).unwrap();
    assert_eq!((report.errors, report.chunks_unreadable), (1, 1));
    // the damaged chunk is still there, byte for byte
    let after = fs::read(world.root.join("region/r.0.0.mca")).unwrap();
    let header = Header::parse(&after).unwrap();
    assert_eq!(header.present().iter().collect::<Vec<_>>(), vec![1, 2]);
    let location = header.location(1);
    assert_eq!(&after[location.offset as usize * 4096..][..location.sectors as usize * 4096], &raw[..]);
    assert_eq!(world.present("entities"), set(&[(1, 0), (2, 0)]));
}

#[test]
fn unreadable_region_file_is_kept_when_continuing() {
    let world = world_of("garbage", vec![((0, 0), chunk(0)), ((40, 0), chunk(0)), ((41, 0), chunk(900))]);
    // a location table full of garbage
    let path = world.root.join("region/r.0.0.mca");
    let mut bytes = fs::read(&path).unwrap();
    bytes[..4096].fill(0xFF);
    fs::write(&path, &bytes).unwrap();
    let before = world.snapshot();
    assert!(world.prune(&by_time(100)).is_err());
    assert_eq!(world.snapshot(), before);

    let report = world.prune(&PruneOptions { continue_on_error: true, ..by_time(100) }).unwrap();
    assert_eq!(report.chunks_unreadable, 1024);
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(world.present("entities"), set(&[(0, 0), (41, 0)]));
}

#[test]
fn dry_run_reports_without_changing_anything() {
    let world = structure_world("dry-run", 5000);
    let before = world.snapshot();
    let dry = world.prune(&PruneOptions { dry_run: true, ..by_time(100) }).unwrap();
    assert!(dry.dry_run);
    assert_eq!(world.snapshot(), before);
    assert!(dry.chunks_removed() > 1000);

    let mut real = world.prune(&by_time(100)).unwrap();
    assert_ne!(world.snapshot(), before);
    real.dry_run = true;
    assert_eq!(dry, real);
}

#[test]
fn cancelling_leaves_the_world_untouched() {
    let world = structure_world("cancel", 5000);
    let before = world.snapshot();
    let report = prune(&world.dimension(), &by_time(100), &NoProgress, &AtomicBool::new(true)).unwrap();
    assert!(report.cancelled);
    assert_eq!(world.snapshot(), before);
}

#[test]
fn empty_and_foreign_files() {
    let world = world_of("empty", vec![((0, 0), chunk(900))]);
    fs::write(world.root.join("region/r.5.5.mca"), b"").unwrap();
    fs::write(world.root.join("region/r.6.6.mca"), vec![0u8; HEADER_SIZE]).unwrap();
    fs::write(world.root.join("region/r.0.0.mca.bak"), b"keep me").unwrap();
    fs::write(world.root.join("region/notes.txt"), b"keep me").unwrap();
    fs::write(world.root.join("poi/r.9.9.mca"), vec![0u8; 100]).unwrap();
    world.prune(&by_time(100)).unwrap();
    assert!(!world.root.join("region/r.5.5.mca").exists());
    assert!(!world.root.join("region/r.6.6.mca").exists());
    assert!(!world.root.join("poi/r.9.9.mca").exists());
    assert!(world.root.join("region/r.0.0.mca.bak").exists());
    assert!(world.root.join("region/notes.txt").exists());
    assert!(world.root.join("region/r.0.0.mca").exists());
}

#[test]
fn removes_leftover_temporary_files() {
    let world = world_of("leftover", vec![((0, 0), chunk(900)), ((1, 0), chunk(0))]);
    let leftover = world.root.join("region/r.0.0.mca.wp-tmp");
    let unrelated = world.root.join("region/backup.wp-tmp");
    fs::write(&leftover, b"partial").unwrap();
    fs::write(&unrelated, b"not ours").unwrap();
    world.prune(&PruneOptions { dry_run: true, ..by_time(100) }).unwrap();
    assert!(leftover.exists());
    world.prune(&by_time(100)).unwrap();
    assert!(!leftover.exists());
    assert!(unrelated.exists());
    fs::remove_file(unrelated).unwrap();
    assert_eq!(world.present("region"), set(&[(0, 0)]));
}

#[test]
fn works_without_poi_and_entities() {
    let world = world_of("region-only", vec![((0, 0), chunk(900)), ((1, 0), chunk(0))]);
    fs::remove_dir_all(world.root.join("poi")).unwrap();
    fs::remove_dir_all(world.root.join("entities")).unwrap();
    let report = world.prune(&by_time(100)).unwrap();
    assert_eq!(report.dirs.len(), 1);
    assert_eq!(world.present("region"), set(&[(0, 0)]));
}

#[test]
fn prunes_a_26_1_world_folder() {
    let world = world_of("layout", vec![((0, 0), chunk(900)), ((1, 0), chunk(0))]);
    // move the dimension to where 26.1+ keeps the Nether
    let target = world.root.join("dimensions/minecraft/the_nether");
    fs::create_dir_all(&target).unwrap();
    for dir in ["region", "poi", "entities"] {
        fs::rename(world.root.join(dir), target.join(dir)).unwrap();
    }
    fs::write(world.root.join("level.dat"), b"").unwrap();
    assert!(resolve_dimensions(&world.root, &[]).is_err()); // there is no overworld
    let dimensions = resolve_dimensions(&world.root, &["the_nether".to_string()]).unwrap();
    let report = prune(&dimensions[0], &by_time(100), &NoProgress, &AtomicBool::new(false)).unwrap();
    assert_eq!(report.chunks_removed(), 1);
    let bytes = fs::read(target.join("region/r.0.0.mca")).unwrap();
    assert_eq!(Header::parse(&bytes).unwrap().present().iter().collect::<Vec<_>>(), vec![0]);
}

// ---- randomised comparison against a straightforward model ----

struct Rng(u64);

impl Rng {
    fn next(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }

    fn range(&mut self, lo: i32, hi: i32) -> i32 {
        lo + self.next((hi - lo) as u64) as i32
    }
}

fn random_world(label: &str, seed: u64) -> World {
    let mut rng = Rng(seed);
    let mut chunks: BTreeMap<ChunkPos, TestChunk> = BTreeMap::new();
    // a few blobs of generated terrain spread over nine regions
    for _ in 0..14 {
        let (cx, cz) = (rng.range(-40, 50), rng.range(-40, 50));
        let (w, h) = (rng.range(1, 14), rng.range(1, 14));
        let visited = rng.next(3) == 0;
        for x in cx..cx + w {
            for z in cz..cz + h {
                let inhabited = if visited && rng.next(4) != 0 { rng.next(5000) as i64 } else { rng.next(120) as i64 };
                chunks.insert(ChunkPos::new(x, z), chunk(inhabited));
            }
        }
    }
    let positions: Vec<ChunkPos> = chunks.keys().copied().collect();
    for i in 0..40 {
        let start = positions[rng.next(positions.len() as u64) as usize];
        let id = ["minecraft:village", "minecraft:mineshaft", "mymod:tower"][i % 3];
        let boxes: Vec<[i32; 4]> = (0..rng.range(1, 5))
            .map(|_| {
                let x0 = start.x * 16 + rng.range(-40, 40);
                let z0 = start.z * 16 + rng.range(-40, 40);
                [x0, z0, x0 + rng.range(0, 90), z0 + rng.range(0, 90)]
            })
            .collect();
        // every existing chunk that the pieces touch references the start
        for b in &boxes {
            for x in (b[0] >> 4)..=(b[2] >> 4) {
                for z in (b[1] >> 4)..=(b[3] >> 4) {
                    if let Some(c) = chunks.get_mut(&ChunkPos::new(x, z))
                        && !c.references.contains(&(id, start))
                    {
                        c.references.push((id, start));
                    }
                }
            }
        }
        let holder = chunks.get_mut(&start).unwrap();
        if !holder.starts.iter().any(|s| s.id == id) {
            holder.starts.push(Structure { id, start, boxes });
        } else {
            // the start slot is taken: these references now point to the older structure
        }
    }
    World::create(label, chunks)
}

/// What should be left of a world, computed the slow and obvious way.
fn model(world: &World, options: &PruneOptions) -> BTreeSet<ChunkPos> {
    let own_merit = |pos: &ChunkPos, c: &TestChunk| {
        if options.whitelist_only { options.whitelist.is_chunk_selected(*pos) } else { c.inhabited.is_none_or(|t| t > options.inhabited_time) }
    };
    // regions that are scanned at all
    let scanned = |pos: &ChunkPos| !options.whitelist_only || options.whitelist.is_any_chunk_in_region_selected(pos.region());
    let mut keep: BTreeSet<ChunkPos> = BTreeSet::new();
    let mut structures: BTreeSet<(&str, ChunkPos)> = BTreeSet::new();
    let r = options.radius as i32;
    for (pos, c) in world.chunks.iter().filter(|(pos, _)| scanned(pos)) {
        if !own_merit(pos, c) {
            continue;
        }
        structures.extend(c.references.iter().copied());
        structures.extend(c.starts.iter().map(|s| (s.id, s.start)));
        if !options.whitelist_only {
            for dx in -r..=r {
                for dz in -r..=r {
                    if dx * dx + dz * dz <= r * r {
                        keep.insert(ChunkPos::new(pos.x + dx, pos.z + dz));
                    }
                }
            }
        }
    }
    for (id, start) in structures {
        let Some(s) = world.chunks.get(&start).filter(|_| scanned(&start)).and_then(|c| c.starts.iter().find(|s| s.id == id)) else { continue };
        for b in &s.boxes {
            keep.extend(rect((b[0] >> 4) - 1..=(b[2] >> 4) + 1, (b[1] >> 4) - 1..=(b[3] >> 4) + 1));
        }
    }
    world.chunks.keys().copied().filter(|pos| keep.contains(pos) || options.whitelist.is_chunk_selected(*pos)).collect()
}

#[test]
fn random_worlds_match_the_model() {
    let whitelist_csv = "0;0;3;3\n-1;0\n1;1;40;40\n1;1;41;40\n";
    let variants: Vec<PruneOptions> = vec![
        by_time(100),
        PruneOptions { radius: 1, ..by_time(100) },
        PruneOptions { radius: 6, ..by_time(2500) },
        PruneOptions { whitelist: whitelist(whitelist_csv), radius: 2, ..by_time(1000) },
        PruneOptions { whitelist: whitelist(whitelist_csv), whitelist_only: true, ..by_time(0) },
        PruneOptions { whitelist: whitelist(&format!("inverted\n{whitelist_csv}")), ..by_time(4000) },
    ];
    for seed in 1..=4u64 {
        for (i, options) in variants.iter().enumerate() {
            let world = random_world("random", seed * 7919);
            let expected = model(&world, options);
            let report = world.prune(options).unwrap();
            let present = world.present("region");
            assert_eq!(present, expected, "seed {seed}, variant {i}");
            assert_eq!(world.present("poi"), expected, "seed {seed}, variant {i}");
            assert_eq!(world.present("entities"), expected, "seed {seed}, variant {i}");
            assert_eq!(report.chunks_removed() as usize, world.chunks.len() - expected.len());
            assert_eq!(report.errors, 0);
        }
    }
}

#[test]
fn result_does_not_depend_on_thread_count() {
    let mut results = Vec::new();
    for threads in [1, 2, 16] {
        let world = random_world("threads", 4242);
        let report = world.prune(&PruneOptions { threads, radius: 2, ..by_time(800) }).unwrap();
        let files: BTreeMap<String, Vec<u8>> =
            world.snapshot().into_iter().map(|(path, bytes)| (path.strip_prefix(&world.root).unwrap().to_string_lossy().into_owned(), bytes)).collect();
        results.push((report, files));
    }
    assert!(results[0].0.chunks_removed() > 0);
    assert_eq!(results[0], results[1]);
    assert_eq!(results[0], results[2]);
}

#[test]
fn leaves_no_temporary_files() {
    let world = random_world("temp", 77);
    world.prune(&by_time(500)).unwrap();
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            if entry.path().is_dir() { walk(&entry.path(), out) } else { out.push(entry.path()) }
        }
    }
    let mut files = Vec::new();
    walk(&world.root, &mut files);
    assert!(!files.is_empty());
    for file in files {
        let name = file.file_name().unwrap().to_str().unwrap().to_string();
        assert!(RegionPos::from_file_name(&name).is_some(), "unexpected file {name}");
    }
}
