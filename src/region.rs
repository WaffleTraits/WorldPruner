//! Anvil region files (`r.<x>.<z>.mca`): header parsing, chunk decompression
//! and crash-safe rewriting.

use crate::chunk_set::ChunkSet;
use crate::pos::{CHUNKS_PER_REGION, RegionPos, mcc_file_name};
use anyhow::{Context, Result, anyhow, bail};
use flate2::{Decompress, FlushDecompress, Status};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const SECTOR_SIZE: usize = 4096;
/// Location table plus timestamp table.
pub const HEADER_SIZE: usize = 2 * SECTOR_SIZE;

const COMPRESSION_GZIP: u8 = 1;
const COMPRESSION_ZLIB: u8 = 2;
const COMPRESSION_NONE: u8 = 3;
const COMPRESSION_LZ4: u8 = 4;
const COMPRESSION_CUSTOM: u8 = 127;
/// Set on the compression byte when the chunk lives in a separate `.mcc` file.
const EXTERNAL_FLAG: u8 = 0x80;

/// Suffix of the temporary file a region is rewritten into.
pub const TEMP_SUFFIX: &str = ".wp-tmp";

/// One entry of a region file's location table.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Location {
    /// First sector of the chunk (0 if the chunk does not exist).
    pub offset: u32,
    /// Number of sectors reserved for the chunk.
    pub sectors: u8,
}

impl Location {
    pub fn exists(self) -> bool {
        self.offset != 0 && self.sectors != 0
    }

    fn byte_offset(self) -> u64 {
        u64::from(self.offset) * SECTOR_SIZE as u64
    }

    fn byte_len(self) -> usize {
        usize::from(self.sectors) * SECTOR_SIZE
    }
}

/// The two header sectors of a region file.
pub struct Header {
    bytes: Box<[u8; HEADER_SIZE]>,
}

impl Header {
    pub fn parse(bytes: &[u8]) -> Option<Header> {
        let bytes: [u8; HEADER_SIZE] = bytes.get(..HEADER_SIZE)?.try_into().ok()?;
        Some(Header { bytes: Box::new(bytes) })
    }

    pub fn read_from(file: &mut File) -> io::Result<Header> {
        let mut bytes = Box::new([0u8; HEADER_SIZE]);
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut bytes[..])?;
        Ok(Header { bytes })
    }

    pub fn location(&self, index: usize) -> Location {
        let b = &self.bytes[index * 4..index * 4 + 4];
        Location { offset: u32::from_be_bytes([0, b[0], b[1], b[2]]), sectors: b[3] }
    }

    pub fn timestamp(&self, index: usize) -> [u8; 4] {
        let at = SECTOR_SIZE + index * 4;
        [self.bytes[at], self.bytes[at + 1], self.bytes[at + 2], self.bytes[at + 3]]
    }

    /// The chunks that exist according to the location table.
    pub fn present(&self) -> ChunkSet {
        (0..CHUNKS_PER_REGION).filter(|&i| self.location(i).exists()).collect()
    }
}

/// Reusable decompression state. Keep one per worker thread.
pub struct Decompressor {
    zlib: Decompress,
    /// Compressed bytes of an external `.mcc` chunk.
    external: Vec<u8>,
    /// Output of formats that cannot be streamed.
    decoded: Vec<u8>,
}

impl Default for Decompressor {
    fn default() -> Self {
        Decompressor { zlib: Decompress::new(true), external: Vec::new(), decoded: Vec::new() }
    }
}

/// A stream of uncompressed chunk NBT.
pub enum ChunkReader<'a> {
    Zlib(ZlibReader<'a>),
    Gzip(Box<flate2::read::MultiGzDecoder<&'a [u8]>>),
    Plain(&'a [u8]),
}

impl Read for ChunkReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            ChunkReader::Zlib(r) => r.read(buf),
            ChunkReader::Gzip(r) => r.read(buf),
            ChunkReader::Plain(r) => r.read(buf),
        }
    }
}

/// Streaming zlib decoder over a byte slice that reuses its inflate state.
pub struct ZlibReader<'a> {
    state: &'a mut Decompress,
    input: &'a [u8],
    finished: bool,
}

impl Read for ZlibReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.finished || buf.is_empty() {
            return Ok(0);
        }
        loop {
            let before_in = self.state.total_in();
            let before_out = self.state.total_out();
            let status = self
                .state
                .decompress(self.input, buf, FlushDecompress::None)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("corrupt zlib stream: {e}")))?;
            let consumed = (self.state.total_in() - before_in) as usize;
            let produced = (self.state.total_out() - before_out) as usize;
            self.input = &self.input[consumed..];
            if status == Status::StreamEnd {
                self.finished = true;
                return Ok(produced);
            }
            if produced > 0 {
                return Ok(produced);
            }
            if consumed == 0 {
                // No progress is possible: the input ran out mid-stream.
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated zlib stream"));
            }
        }
    }
}

impl Decompressor {
    /// Open a chunk for reading. `sectors` are the sectors of the chunk as
    /// loaded by [`read_sectors`], `index` is its header slot and `dir` the
    /// directory of the region file, where oversized chunks are stored as
    /// `c.<x>.<z>.mcc`.
    pub fn open<'a>(&'a mut self, sectors: &'a [u8], region: RegionPos, index: usize, dir: &Path) -> Result<ChunkReader<'a>> {
        let Some((length, rest)) = sectors.split_first_chunk::<4>() else {
            bail!("chunk header is truncated");
        };
        let length = u32::from_be_bytes(*length) as usize;
        if length == 0 {
            bail!("chunk has a declared length of zero");
        }
        let Some(record) = rest.get(..length) else {
            bail!("chunk is truncated: {length} bytes declared, {} available", rest.len());
        };
        let (scheme, payload) = (record[0], &record[1..]);

        if scheme & EXTERNAL_FLAG == 0 {
            return self.decoder(scheme, Payload::Inline(payload));
        }
        let path = dir.join(mcc_file_name(region.chunk(index)));
        self.external.clear();
        File::open(&path)
            .and_then(|mut f| f.read_to_end(&mut self.external))
            .with_context(|| format!("failed to read oversized chunk file {}", path.display()))?;
        self.decoder(scheme & !EXTERNAL_FLAG, Payload::External)
    }

    fn decoder<'a>(&'a mut self, scheme: u8, payload: Payload<'a>) -> Result<ChunkReader<'a>> {
        let Decompressor { zlib, external, decoded } = self;
        let data: &'a [u8] = match payload {
            Payload::Inline(data) => data,
            Payload::External => external,
        };
        match scheme {
            COMPRESSION_ZLIB => {
                zlib.reset(true);
                Ok(ChunkReader::Zlib(ZlibReader { state: zlib, input: data, finished: false }))
            }
            COMPRESSION_GZIP => Ok(ChunkReader::Gzip(Box::new(flate2::read::MultiGzDecoder::new(data)))),
            COMPRESSION_NONE => Ok(ChunkReader::Plain(data)),
            COMPRESSION_LZ4 => {
                decode_lz4_blocks(data, decoded)?;
                Ok(ChunkReader::Plain(decoded))
            }
            COMPRESSION_CUSTOM => bail!("chunk uses a custom (modded) compression algorithm"),
            other => bail!("chunk has unknown compression type {other}"),
        }
    }
}

enum Payload<'a> {
    Inline(&'a [u8]),
    External,
}

/// Decode the framing of lz4-java's `LZ4BlockOutputStream`, which is what the
/// game writes with `region-file-compression=lz4`: a sequence of blocks, each
/// `"LZ4Block"`, a token byte, compressed and original length (little endian),
/// a checksum and the data. A block with an original length of 0 ends the
/// stream.
fn decode_lz4_blocks(mut data: &[u8], out: &mut Vec<u8>) -> Result<()> {
    const MAGIC: &[u8; 8] = b"LZ4Block";
    const HEADER_LEN: usize = 21;
    const METHOD_RAW: u8 = 0x10;
    const METHOD_LZ4: u8 = 0x20;
    /// lz4-java's maximum block size (32 MiB).
    const MAX_BLOCK: usize = 1 << 25;
    /// Far more than any real chunk; guards against decompression bombs.
    const MAX_TOTAL: usize = 1 << 30;

    out.clear();
    loop {
        let Some((header, rest)) = data.split_first_chunk::<HEADER_LEN>() else {
            bail!("truncated LZ4 block header");
        };
        if &header[..8] != MAGIC {
            bail!("invalid LZ4 block magic");
        }
        let token = header[8];
        let compressed_len = u32::from_le_bytes([header[9], header[10], header[11], header[12]]) as usize;
        let original_len = u32::from_le_bytes([header[13], header[14], header[15], header[16]]) as usize;
        if original_len == 0 {
            return Ok(());
        }
        if original_len > MAX_BLOCK || out.len() + original_len > MAX_TOTAL {
            bail!("LZ4 block is implausibly large ({original_len} bytes)");
        }
        let Some((block, rest)) = rest.split_at_checked(compressed_len) else {
            bail!("truncated LZ4 block");
        };
        match token & 0xF0 {
            METHOD_RAW => {
                if compressed_len != original_len {
                    bail!("raw LZ4 block has mismatching lengths");
                }
                out.extend_from_slice(block);
            }
            METHOD_LZ4 => {
                let start = out.len();
                out.resize(start + original_len, 0);
                let written = lz4_flex::block::decompress_into(block, &mut out[start..]).map_err(|e| anyhow!("corrupt LZ4 block: {e}"))?;
                if written != original_len {
                    bail!("LZ4 block decompressed to {written} bytes, expected {original_len}");
                }
            }
            other => bail!("unknown LZ4 block method {other:#04x}"),
        }
        data = rest;
    }
}

/// What a rewrite did (or, in a dry run, would do) to a region file.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct RewriteOutcome {
    /// The file was left exactly as it was.
    pub untouched: bool,
    /// The whole file was deleted.
    pub deleted_file: bool,
    /// Chunks that existed and still exist.
    pub kept_chunks: u32,
    /// Chunks that were removed.
    pub removed_chunks: u32,
    /// Removed chunks whose location entry was already unusable.
    pub corrupt_chunks: u32,
    /// Oversized chunk files (`.mcc`) that were deleted.
    pub removed_external: u32,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

/// Which chunks of a region file to keep.
pub enum Keep<'a> {
    /// Remove every chunk, i.e. delete the file.
    Nothing,
    /// Keep exactly these chunks.
    Only(&'a ChunkSet),
}

impl Keep<'_> {
    fn contains(&self, index: usize) -> bool {
        match self {
            Keep::Nothing => false,
            Keep::Only(set) => set.get(index),
        }
    }
}

/// Remove all chunks that are not in `keep` from a region file.
///
/// Kept chunks are copied byte for byte (so their compression does not
/// matter) into a compacted temporary file next to the original, which then
/// atomically replaces it. If nothing has to be removed, the file is not
/// touched at all; if nothing is left, it is deleted. `has_external` tells
/// whether a `.mcc` file exists for a chunk index, so that the oversized data
/// of removed chunks is deleted along with them.
///
/// With `dry_run` nothing on disk is modified.
pub fn prune_region_file(path: &Path, region: RegionPos, keep: Keep<'_>, has_external: &dyn Fn(usize) -> bool, dry_run: bool) -> Result<RewriteOutcome> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut source = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let metadata = source.metadata().with_context(|| format!("failed to stat {}", path.display()))?;
    let file_len = metadata.len();
    let mut outcome = RewriteOutcome { bytes_before: file_len, bytes_after: file_len, ..RewriteOutcome::default() };

    // Oversized chunk files that have to go once the region file is updated.
    let mut stale_external = Vec::new();
    let mut note_removed = |index: usize, outcome: &mut RewriteOutcome| {
        outcome.removed_chunks += 1;
        if has_external(index) {
            stale_external.push(dir.join(mcc_file_name(region.chunk(index))));
        }
    };

    // (source location, header slot) of every chunk that stays
    let mut kept: Vec<(Location, usize)> = Vec::new();
    let header = if file_len >= HEADER_SIZE as u64 {
        Some(Header::read_from(&mut source).with_context(|| format!("failed to read header of {}", path.display()))?)
    } else {
        // A file without a complete header holds no chunks.
        None
    };
    if let Some(header) = &header {
        for index in 0..CHUNKS_PER_REGION {
            let location = header.location(index);
            if !location.exists() {
                continue;
            }
            if !keep.contains(index) {
                note_removed(index, &mut outcome);
            } else if location.offset < 2 || location.byte_offset() >= file_len {
                // The game cannot load this chunk either and would regenerate it.
                log::warn!("{}: dropping chunk {} with an invalid location (sector {})", path.display(), region.chunk(index), location.offset);
                outcome.corrupt_chunks += 1;
                note_removed(index, &mut outcome);
            } else {
                kept.push((location, index));
            }
        }
    }
    outcome.kept_chunks = kept.len() as u32;
    outcome.removed_external = stale_external.len() as u32;

    if kept.is_empty() {
        outcome.deleted_file = true;
        outcome.bytes_after = 0;
        if !dry_run {
            drop(source);
            fs::remove_file(path).with_context(|| format!("failed to delete {}", path.display()))?;
            remove_files(&stale_external);
        }
        return Ok(outcome);
    }
    if outcome.removed_chunks == 0 {
        outcome.untouched = true;
        return Ok(outcome);
    }
    let header = header.expect("kept chunks imply a header");

    // Copy in file order so the source is read sequentially.
    kept.sort_by_key(|(location, _)| location.offset);
    let mut new_header = vec![0u8; HEADER_SIZE];
    let mut next_sector: u32 = 2;
    let mut plan = Vec::with_capacity(kept.len());
    for &(location, index) in &kept {
        let sectors = sectors_in_use(&mut source, location, file_len).unwrap_or(location.sectors);
        let entry = (next_sector << 8) | u32::from(sectors);
        new_header[index * 4..index * 4 + 4].copy_from_slice(&entry.to_be_bytes());
        new_header[SECTOR_SIZE + index * 4..SECTOR_SIZE + index * 4 + 4].copy_from_slice(&header.timestamp(index));
        plan.push((location, sectors));
        next_sector += u32::from(sectors);
    }
    outcome.bytes_after = u64::from(next_sector) * SECTOR_SIZE as u64;
    if dry_run {
        return Ok(outcome);
    }

    let temp_path = temp_path_for(path);
    let written = (|| -> Result<()> {
        let mut temp = File::create(&temp_path).with_context(|| format!("failed to create {}", temp_path.display()))?;
        temp.write_all(&new_header)?;
        let mut buffer = vec![0u8; 255 * SECTOR_SIZE];
        for &(location, sectors) in &plan {
            let chunk = &mut buffer[..usize::from(sectors) * SECTOR_SIZE];
            source.seek(SeekFrom::Start(location.byte_offset()))?;
            let read = read_up_to(&mut source, chunk)?;
            // A final sector that is cut short is padded with zeroes.
            chunk[read..].fill(0);
            temp.write_all(chunk)?;
        }
        // The data has to be on disk before it replaces the original.
        temp.sync_all()?;
        Ok(())
    })();
    // Windows cannot replace a file that is still open.
    drop(source);
    let result = written.and_then(|()| {
        copy_ownership(&metadata, &temp_path);
        fs::rename(&temp_path, path).with_context(|| format!("failed to replace {}", path.display()))
    });
    if let Err(e) = result {
        let _ = fs::remove_file(&temp_path);
        return Err(e.context(format!("failed to rewrite {}", path.display())));
    }
    remove_files(&stale_external);
    Ok(outcome)
}

/// Number of sectors a chunk really occupies according to its length prefix,
/// or `None` if the prefix is implausible (then the reserved size is copied).
fn sectors_in_use(source: &mut File, location: Location, file_len: u64) -> Option<u8> {
    if location.byte_offset() + 4 > file_len {
        return None;
    }
    let mut length = [0u8; 4];
    source.seek(SeekFrom::Start(location.byte_offset())).ok()?;
    source.read_exact(&mut length).ok()?;
    let total = u32::from_be_bytes(length) as usize + 4;
    if total <= 4 || total > location.byte_len() {
        return None;
    }
    Some(total.div_ceil(SECTOR_SIZE) as u8)
}

/// Load the sectors reserved for a chunk into `buf`. Fewer bytes than
/// reserved are returned if the file ends early (old versions did not pad the
/// last chunk to a full sector).
pub fn read_sectors(file: &mut File, location: Location, buf: &mut Vec<u8>) -> Result<()> {
    buf.clear();
    if location.offset < 2 {
        bail!("chunk data overlaps the region header");
    }
    buf.resize(location.byte_len(), 0);
    file.seek(SeekFrom::Start(location.byte_offset()))?;
    let read = read_up_to(file, buf)?;
    buf.truncate(read);
    if read == 0 {
        bail!("chunk sector {} is outside of the file", location.offset);
    }
    Ok(())
}

fn read_up_to(source: &mut File, mut buf: &mut [u8]) -> io::Result<usize> {
    let mut total = 0;
    while !buf.is_empty() {
        match source.read(buf) {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                buf = &mut buf[n..];
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

fn temp_path_for(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(TEMP_SUFFIX);
    PathBuf::from(name)
}

/// Give the replacement file the permissions and owner of the original, so
/// pruning as another user (e.g. root) does not lock the server out.
#[cfg(unix)]
fn copy_ownership(original: &fs::Metadata, temp: &Path) {
    use std::os::unix::fs::MetadataExt;
    let _ = fs::set_permissions(temp, original.permissions());
    let _ = std::os::unix::fs::chown(temp, Some(original.uid()), Some(original.gid()));
}

#[cfg(not(unix))]
fn copy_ownership(_original: &fs::Metadata, _temp: &Path) {}

fn remove_files(paths: &[PathBuf]) {
    for path in paths {
        if let Err(e) = fs::remove_file(path)
            && e.kind() != io::ErrorKind::NotFound
        {
            log::warn!("failed to delete {}: {e}", path.display());
        }
    }
}

/// Helpers to build region files for tests.
#[doc(hidden)]
pub mod testing {
    use super::*;
    use std::io::Write;

    /// How a test chunk is stored.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Storage {
        Gzip,
        Zlib,
        Plain,
        Lz4,
        /// zlib, in a separate `.mcc` file
        External,
    }

    pub fn compress(nbt: &[u8], storage: Storage) -> Vec<u8> {
        match storage {
            Storage::Zlib | Storage::External => {
                let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
                enc.write_all(nbt).unwrap();
                enc.finish().unwrap()
            }
            Storage::Gzip => {
                let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
                enc.write_all(nbt).unwrap();
                enc.finish().unwrap()
            }
            Storage::Plain => nbt.to_vec(),
            Storage::Lz4 => {
                let mut out = Vec::new();
                // split into two blocks to exercise the block framing
                let (a, b) = nbt.split_at(nbt.len() / 2);
                for (part, raw) in [(a, false), (b, true)] {
                    if part.is_empty() {
                        continue;
                    }
                    let block = if raw { part.to_vec() } else { lz4_flex::block::compress(part) };
                    out.extend_from_slice(b"LZ4Block");
                    out.push(if raw { 0x10 } else { 0x20 } | 6);
                    out.extend_from_slice(&(block.len() as u32).to_le_bytes());
                    out.extend_from_slice(&(part.len() as u32).to_le_bytes());
                    out.extend_from_slice(&[0; 4]);
                    out.extend_from_slice(&block);
                }
                out.extend_from_slice(b"LZ4Block");
                out.push(0x10 | 6);
                out.extend_from_slice(&[0; 12]);
                out
            }
        }
    }

    /// Write a region file containing the given chunks (header slot,
    /// uncompressed NBT, storage). Returns the path of the file.
    pub fn write_region(dir: &Path, region: RegionPos, chunks: &[(usize, Vec<u8>, Storage)]) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let mut file = vec![0u8; HEADER_SIZE];
        for (index, nbt, storage) in chunks {
            let compressed = compress(nbt, *storage);
            let scheme = match storage {
                Storage::Gzip => COMPRESSION_GZIP,
                Storage::Zlib => COMPRESSION_ZLIB,
                Storage::Plain => COMPRESSION_NONE,
                Storage::Lz4 => COMPRESSION_LZ4,
                Storage::External => COMPRESSION_ZLIB | EXTERNAL_FLAG,
            };
            let payload: &[u8] = if *storage == Storage::External {
                fs::write(dir.join(mcc_file_name(region.chunk(*index))), &compressed).unwrap();
                &[]
            } else {
                &compressed
            };
            let sector = file.len() / SECTOR_SIZE;
            file.extend_from_slice(&(payload.len() as u32 + 1).to_be_bytes());
            file.push(scheme);
            file.extend_from_slice(payload);
            file.resize(file.len().div_ceil(SECTOR_SIZE) * SECTOR_SIZE, 0);
            let sectors = file.len() / SECTOR_SIZE - sector;
            assert!(sectors < 256);
            let entry = ((sector as u32) << 8) | sectors as u32;
            file[index * 4..index * 4 + 4].copy_from_slice(&entry.to_be_bytes());
            let timestamp = 1_700_000_000u32 + *index as u32;
            file[SECTOR_SIZE + index * 4..SECTOR_SIZE + index * 4 + 4].copy_from_slice(&timestamp.to_be_bytes());
        }
        let path = dir.join(region.file_name());
        fs::write(&path, file).unwrap();
        path
    }

    /// Read back the uncompressed NBT of a chunk, or `None` if it is absent.
    pub fn read_chunk(path: &Path, index: usize) -> Option<Vec<u8>> {
        let region = RegionPos::from_file_name(path.file_name()?.to_str()?)?;
        let mut file = File::open(path).ok()?;
        let location = Header::read_from(&mut file).ok()?.location(index);
        if !location.exists() {
            return None;
        }
        let mut sectors = Vec::new();
        read_sectors(&mut file, location, &mut sectors).unwrap();
        let mut decompressor = Decompressor::default();
        let mut reader = decompressor.open(&sectors, region, index, path.parent()?).unwrap();
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        Some(out)
    }

    /// A unique, empty scratch directory that is removed when dropped.
    pub struct TempDir(pub PathBuf);

    impl TempDir {
        pub fn new(label: &str) -> TempDir {
            use std::sync::atomic::{AtomicU32, Ordering};
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!("worldpruner-{label}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    const R: RegionPos = RegionPos::new(-1, 2);

    fn payload(seed: usize, len: usize) -> Vec<u8> {
        (0..len).map(|i| ((i * 31 + seed * 7) % 251) as u8).collect()
    }

    fn no_external(_: usize) -> bool {
        false
    }

    #[test]
    fn reads_every_storage_format() {
        let dir = TempDir::new("formats");
        let chunks: Vec<_> = [Storage::Gzip, Storage::Zlib, Storage::Plain, Storage::Lz4, Storage::External]
            .into_iter()
            .enumerate()
            .map(|(i, storage)| (i * 100, payload(i, 20_000 + i), storage))
            .collect();
        let path = write_region(dir.path(), R, &chunks);
        for (index, nbt, storage) in &chunks {
            assert_eq!(read_chunk(&path, *index).as_ref(), Some(nbt), "{storage:?}");
        }
        assert_eq!(read_chunk(&path, 1), None);
    }

    #[test]
    fn reports_corrupt_chunks() {
        let dir = TempDir::new("corrupt");
        let path = write_region(dir.path(), R, &[(0, payload(0, 50_000), Storage::Zlib), (1, payload(1, 100), Storage::External)]);
        let header = Header::parse(&fs::read(&path).unwrap()).unwrap();
        let mut file = File::open(&path).unwrap();
        let mut d = Decompressor::default();
        let mut sectors = Vec::new();

        // sector outside of the file / inside the header
        let err = read_sectors(&mut file, Location { offset: 9999, sectors: 1 }, &mut sectors).unwrap_err();
        assert!(err.to_string().contains("outside"));
        let err = read_sectors(&mut file, Location { offset: 1, sectors: 1 }, &mut sectors).unwrap_err();
        assert!(err.to_string().contains("header"));
        // more sectors reserved than the file has left
        read_sectors(&mut file, Location { offset: header.location(0).offset, sectors: 255 }, &mut sectors).unwrap();
        assert!(sectors.len() < 255 * SECTOR_SIZE);
        assert!(d.open(&sectors, R, 0, dir.path()).is_ok());

        let open_err = |d: &mut Decompressor, sectors: &[u8], index: usize| d.open(sectors, R, index, dir.path()).err().map(|e| format!("{e:#}"));
        // missing .mcc file
        fs::remove_file(dir.path().join(mcc_file_name(R.chunk(1)))).unwrap();
        read_sectors(&mut file, header.location(1), &mut sectors).unwrap();
        assert!(open_err(&mut d, &sectors, 1).unwrap().contains("oversized"));
        // too short for a chunk header
        assert!(open_err(&mut d, &sectors[..3], 1).unwrap().contains("header is truncated"));
        // declared length larger than the reserved sectors
        read_sectors(&mut file, header.location(0), &mut sectors).unwrap();
        sectors[..4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(open_err(&mut d, &sectors, 0).unwrap().contains("truncated"));
        sectors[..4].copy_from_slice(&0u32.to_be_bytes());
        assert!(open_err(&mut d, &sectors, 0).unwrap().contains("length of zero"));
        // unknown and custom compression
        sectors[..4].copy_from_slice(&100u32.to_be_bytes());
        sectors[4] = 9;
        assert!(open_err(&mut d, &sectors, 0).unwrap().contains("unknown compression"));
        sectors[4] = COMPRESSION_CUSTOM;
        assert!(open_err(&mut d, &sectors, 0).unwrap().contains("custom"));
        // garbage zlib data fails while reading, not while opening
        sectors[4] = COMPRESSION_ZLIB;
        sectors[5..60].fill(0xAA);
        let mut reader = d.open(&sectors, R, 0, dir.path()).unwrap();
        assert!(reader.read_to_end(&mut Vec::new()).is_err());
        // truncated zlib stream
        let good = compress(&payload(3, 50_000), Storage::Zlib);
        let mut state = Decompress::new(true);
        let mut reader = ZlibReader { state: &mut state, input: &good[..good.len() / 2], finished: false };
        let err = reader.read_to_end(&mut Vec::new()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn prune_keeps_selected_chunks_byte_for_byte() {
        let dir = TempDir::new("prune");
        let chunks: Vec<_> = (0..40).map(|i| (i * 25, payload(i, 3000 + i * 700), Storage::Zlib)).collect();
        let path = write_region(dir.path(), R, &chunks);
        let before = fs::read(&path).unwrap();
        let old_header = Header::parse(&before).unwrap();

        let keep: ChunkSet = chunks.iter().map(|c| c.0).filter(|i| i % 50 == 0).collect();
        let outcome = prune_region_file(&path, R, Keep::Only(&keep), &no_external, false).unwrap();
        assert_eq!(outcome.kept_chunks, 20);
        assert_eq!(outcome.removed_chunks, 20);
        assert!(!outcome.untouched && !outcome.deleted_file);

        let after = fs::read(&path).unwrap();
        assert_eq!(after.len() as u64, outcome.bytes_after);
        assert!(after.len() < before.len());
        assert_eq!(after.len() % SECTOR_SIZE, 0);
        let new_header = Header::parse(&after).unwrap();
        assert_eq!(new_header.present(), keep);
        let mut used = vec![false; after.len() / SECTOR_SIZE];
        for (index, nbt, _) in &chunks {
            if keep.get(*index) {
                assert_eq!(read_chunk(&path, *index).as_ref(), Some(nbt));
                assert_eq!(new_header.timestamp(*index), old_header.timestamp(*index));
                let location = new_header.location(*index);
                for sector in location.offset..location.offset + u32::from(location.sectors) {
                    assert!(!std::mem::replace(&mut used[sector as usize], true), "sector {sector} is used twice");
                }
            } else {
                assert_eq!(read_chunk(&path, *index), None);
                assert_eq!(new_header.timestamp(*index), [0; 4]);
            }
        }
        // compacted: every sector after the header is in use
        assert!(used[2..].iter().all(|&u| u));
        assert!(!temp_path_for(&path).exists());
    }

    #[test]
    fn prune_leaves_file_alone_when_nothing_is_removed() {
        let dir = TempDir::new("untouched");
        let path = write_region(dir.path(), R, &[(3, payload(1, 5000), Storage::Zlib)]);
        let before = fs::read(&path).unwrap();
        let mut keep = ChunkSet::new();
        keep.set(3);
        keep.set(4); // selected but absent
        let outcome = prune_region_file(&path, R, Keep::Only(&keep), &no_external, false).unwrap();
        assert!(outcome.untouched);
        assert_eq!((outcome.kept_chunks, outcome.removed_chunks), (1, 0));
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn prune_deletes_file_when_nothing_is_left() {
        let dir = TempDir::new("delete");
        let chunks = [(3, payload(1, 5000), Storage::Zlib), (9, payload(2, 100), Storage::External)];
        let mcc = dir.path().join(mcc_file_name(R.chunk(9)));

        let path = write_region(dir.path(), R, &chunks);
        let keep: ChunkSet = [100].into_iter().collect();
        let outcome = prune_region_file(&path, R, Keep::Only(&keep), &|i| i == 9, false).unwrap();
        assert!(outcome.deleted_file);
        assert_eq!((outcome.removed_chunks, outcome.removed_external, outcome.bytes_after), (2, 1, 0));
        assert!(!path.exists() && !mcc.exists());

        let path = write_region(dir.path(), R, &chunks);
        let outcome = prune_region_file(&path, R, Keep::Nothing, &|i| i == 9, false).unwrap();
        assert!(outcome.deleted_file);
        assert!(!path.exists() && !mcc.exists());

        // header-only and truncated files hold no chunks and are deleted too
        for len in [0, 100, HEADER_SIZE] {
            fs::write(&path, vec![0u8; len]).unwrap();
            let outcome = prune_region_file(&path, R, Keep::Only(&ChunkSet::full()), &no_external, false).unwrap();
            assert!(outcome.deleted_file, "{len}");
            assert!(!path.exists());
        }
    }

    #[test]
    fn prune_removes_external_data_of_removed_chunks_only() {
        let dir = TempDir::new("external");
        let chunks = [(1, payload(1, 9000), Storage::External), (2, payload(2, 9000), Storage::External), (3, payload(3, 9000), Storage::Zlib)];
        let path = write_region(dir.path(), R, &chunks);
        let keep: ChunkSet = [1, 3].into_iter().collect();
        let outcome = prune_region_file(&path, R, Keep::Only(&keep), &|i| i == 1 || i == 2, false).unwrap();
        assert_eq!((outcome.kept_chunks, outcome.removed_chunks, outcome.removed_external), (2, 1, 1));
        assert!(dir.path().join(mcc_file_name(R.chunk(1))).exists());
        assert!(!dir.path().join(mcc_file_name(R.chunk(2))).exists());
        assert_eq!(read_chunk(&path, 1), Some(payload(1, 9000)));
        assert_eq!(read_chunk(&path, 3), Some(payload(3, 9000)));
    }

    #[test]
    fn dry_run_changes_nothing() {
        let dir = TempDir::new("dry");
        let chunks = [(1, payload(1, 9000), Storage::External), (2, payload(2, 9000), Storage::Zlib), (3, payload(3, 9000), Storage::Zlib)];
        let path = write_region(dir.path(), R, &chunks);
        let before = fs::read(&path).unwrap();
        let keep: ChunkSet = [3].into_iter().collect();
        let dry = prune_region_file(&path, R, Keep::Only(&keep), &|i| i == 1, true).unwrap();
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(dir.path().join(mcc_file_name(R.chunk(1))).exists());
        let dry_delete = prune_region_file(&path, R, Keep::Nothing, &|i| i == 1, true).unwrap();
        assert!(dry_delete.deleted_file && path.exists());

        let real = prune_region_file(&path, R, Keep::Only(&keep), &|i| i == 1, false).unwrap();
        assert_eq!(dry, real);
        assert_eq!(fs::metadata(&path).unwrap().len(), real.bytes_after);
    }

    #[test]
    fn prune_survives_damaged_location_tables() {
        let dir = TempDir::new("damaged");
        let chunks = [(0, payload(1, 5000), Storage::Zlib), (1, payload(2, 5000), Storage::Zlib), (2, payload(3, 5000), Storage::Zlib)];
        let path = write_region(dir.path(), R, &chunks);
        let mut file = fs::read(&path).unwrap();
        // chunk 1 points far outside of the file, chunk 5 into the header
        file[4..8].copy_from_slice(&((50_000u32 << 8) | 1).to_be_bytes());
        file[20..24].copy_from_slice(&((1u32 << 8) | 1).to_be_bytes());
        // chunk 2 claims more sectors than the file has left
        let entry = u32::from_be_bytes(file[8..12].try_into().unwrap());
        file[8..12].copy_from_slice(&((entry & !0xFF) | 200).to_be_bytes());
        fs::write(&path, &file).unwrap();

        let keep: ChunkSet = [1, 2, 5].into_iter().collect();
        let outcome = prune_region_file(&path, R, Keep::Only(&keep), &no_external, false).unwrap();
        assert_eq!((outcome.kept_chunks, outcome.removed_chunks, outcome.corrupt_chunks), (1, 3, 2));
        assert_eq!(read_chunk(&path, 2), Some(payload(3, 5000)));
        assert_eq!(read_chunk(&path, 0), None);
        assert_eq!(read_chunk(&path, 1), None);
        assert_eq!(fs::metadata(&path).unwrap().len(), outcome.bytes_after);
    }

    #[test]
    fn prune_pads_unaligned_files() {
        let dir = TempDir::new("unaligned");
        let chunks = [(0, payload(1, 5000), Storage::Plain), (1, payload(2, 5000), Storage::Plain)];
        let path = write_region(dir.path(), R, &chunks);
        // cut the padding of the last chunk off and corrupt its length prefix
        // so the reserved size has to be copied
        let mut file = fs::read(&path).unwrap();
        let location = Header::parse(&file).unwrap().location(1);
        let start = location.offset as usize * SECTOR_SIZE;
        file[start..start + 4].copy_from_slice(&0u32.to_be_bytes());
        file.truncate(start + 5010);
        fs::write(&path, &file).unwrap();

        let keep: ChunkSet = [1].into_iter().collect();
        let outcome = prune_region_file(&path, R, Keep::Only(&keep), &no_external, false).unwrap();
        assert_eq!((outcome.kept_chunks, outcome.removed_chunks), (1, 1));
        let after = fs::read(&path).unwrap();
        assert_eq!(after.len(), HEADER_SIZE + 2 * SECTOR_SIZE);
        assert_eq!(&after[HEADER_SIZE + 4..HEADER_SIZE + 5010], &file[start + 4..]);
        assert!(after[HEADER_SIZE + 5010..].iter().all(|&b| b == 0));
    }
}
