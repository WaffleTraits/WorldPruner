//! A streaming NBT reader that extracts only what pruning needs from a chunk.
//!
//! Chunks are never materialised as a tag tree. The reader walks the binary
//! stream once, skips everything it is not interested in without allocating,
//! and stops as soon as both `InhabitedTime` and the structure data have been
//! seen, so the rest of the chunk does not even have to be decompressed.
//!
//! Both chunk layouts are understood:
//!
//! * 1.18 and newer (including 26.x): `InhabitedTime` and
//!   `structures.{starts,References}` in the root compound.
//! * 1.17 and older: `Level.InhabitedTime` and
//!   `Level.Structures.{Starts,References}`.

use std::io::{self, Read};

const TAG_END: u8 = 0;
const TAG_BYTE: u8 = 1;
const TAG_SHORT: u8 = 2;
const TAG_INT: u8 = 3;
const TAG_LONG: u8 = 4;
const TAG_FLOAT: u8 = 5;
const TAG_DOUBLE: u8 = 6;
const TAG_BYTE_ARRAY: u8 = 7;
const TAG_STRING: u8 = 8;
const TAG_LIST: u8 = 9;
const TAG_COMPOUND: u8 = 10;
const TAG_INT_ARRAY: u8 = 11;
const TAG_LONG_ARRAY: u8 = 12;

/// Vanilla refuses to read NBT nested deeper than this.
const MAX_DEPTH: u32 = 512;
const BUFFER_SIZE: usize = 16 * 1024;

/// An axis-aligned box in block coordinates, as stored in a structure piece's
/// `BB` tag (`[minX, minY, minZ, maxX, maxY, maxZ]`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BlockBox {
    pub min_x: i32,
    pub min_z: i32,
    pub max_x: i32,
    pub max_z: i32,
}

/// A structure start stored in a chunk.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct StructureStart {
    /// The structure id, e.g. `minecraft:village_plains`.
    pub id: String,
    /// Bounding boxes of the structure and all of its pieces.
    pub boxes: Vec<BlockBox>,
}

/// Everything pruning needs to know about one chunk.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ChunkInfo {
    /// `None` if the chunk has no `InhabitedTime` tag.
    pub inhabited_time: Option<i64>,
    /// Structures that start in this chunk.
    pub starts: Vec<StructureStart>,
    /// Structures that reach into this chunk: structure id and the packed
    /// positions of the chunks they start in.
    pub references: Vec<(String, Vec<i64>)>,
}

impl ChunkInfo {
    pub fn clear(&mut self) {
        self.inhabited_time = None;
        self.starts.clear();
        self.references.clear();
    }
}

/// Read the pruning-relevant parts of a chunk from uncompressed NBT.
pub fn read_chunk_info(source: impl Read, info: &mut ChunkInfo) -> io::Result<()> {
    info.clear();
    let mut reader = Reader::new(source);
    let root_type = reader.u8()?;
    if root_type != TAG_COMPOUND {
        return Err(invalid(format!("root tag has type {root_type}, expected a compound")));
    }
    reader.skip_string()?;
    let mut parser = ChunkParser { reader, info, name: Vec::new(), found_time: false, found_structures: false };
    parser.root()
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// A small buffered reader with cheap skipping.
struct Reader<R> {
    source: R,
    buf: Box<[u8]>,
    pos: usize,
    len: usize,
}

impl<R: Read> Reader<R> {
    fn new(source: R) -> Self {
        Reader { source, buf: vec![0; BUFFER_SIZE].into_boxed_slice(), pos: 0, len: 0 }
    }

    /// Refill the (fully consumed) buffer. Fails at the end of the stream.
    fn fill(&mut self) -> io::Result<()> {
        debug_assert_eq!(self.pos, self.len);
        loop {
            match self.source.read(&mut self.buf) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "chunk data ends unexpectedly")),
                Ok(n) => {
                    self.pos = 0;
                    self.len = n;
                    return Ok(());
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    fn read_exact(&mut self, mut out: &mut [u8]) -> io::Result<()> {
        while !out.is_empty() {
            if self.pos == self.len {
                self.fill()?;
            }
            let n = out.len().min(self.len - self.pos);
            out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
            self.pos += n;
            out = &mut out[n..];
        }
        Ok(())
    }

    fn array<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let mut bytes = [0; N];
        if self.len - self.pos >= N {
            bytes.copy_from_slice(&self.buf[self.pos..self.pos + N]);
            self.pos += N;
        } else {
            self.read_exact(&mut bytes)?;
        }
        Ok(bytes)
    }

    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.array::<1>()?[0])
    }

    fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    fn i32(&mut self) -> io::Result<i32> {
        Ok(i32::from_be_bytes(self.array()?))
    }

    fn i64(&mut self) -> io::Result<i64> {
        Ok(i64::from_be_bytes(self.array()?))
    }

    /// Length prefix of an array tag.
    fn array_len(&mut self) -> io::Result<u64> {
        let len = self.i32()?;
        u64::try_from(len).map_err(|_| invalid(format!("negative array length {len}")))
    }

    fn skip(&mut self, mut n: u64) -> io::Result<()> {
        loop {
            let available = (self.len - self.pos) as u64;
            if n <= available {
                self.pos += n as usize;
                return Ok(());
            }
            n -= available;
            self.pos = self.len;
            self.fill()?;
        }
    }

    fn skip_string(&mut self) -> io::Result<()> {
        let len = self.u16()?;
        self.skip(u64::from(len))
    }

    /// Read a string's bytes into `out` (replacing its contents).
    fn string_into(&mut self, out: &mut Vec<u8>) -> io::Result<()> {
        let len = usize::from(self.u16()?);
        out.clear();
        out.resize(len, 0);
        self.read_exact(out)
    }

    /// Skip the payload of a tag of the given type.
    fn skip_payload(&mut self, tag: u8, depth: u32) -> io::Result<()> {
        match tag {
            TAG_BYTE => self.skip(1),
            TAG_SHORT => self.skip(2),
            TAG_INT | TAG_FLOAT => self.skip(4),
            TAG_LONG | TAG_DOUBLE => self.skip(8),
            TAG_BYTE_ARRAY => {
                let len = self.array_len()?;
                self.skip(len)
            }
            TAG_STRING => self.skip_string(),
            TAG_INT_ARRAY => {
                let len = self.array_len()?;
                self.skip(len * 4)
            }
            TAG_LONG_ARRAY => {
                let len = self.array_len()?;
                self.skip(len * 8)
            }
            TAG_LIST => {
                let (element, count) = self.list_header()?;
                self.skip_list_items(element, count, depth)
            }
            TAG_COMPOUND => {
                check_depth(depth)?;
                loop {
                    let tag = self.u8()?;
                    if tag == TAG_END {
                        return Ok(());
                    }
                    self.skip_string()?;
                    self.skip_payload(tag, depth + 1)?;
                }
            }
            other => Err(invalid(format!("unknown tag type {other}"))),
        }
    }

    /// Element type and element count of a list.
    fn list_header(&mut self) -> io::Result<(u8, u64)> {
        let element = self.u8()?;
        let count = u64::try_from(self.i32()?).unwrap_or(0);
        if element == TAG_END && count > 0 {
            return Err(invalid("non-empty list of end tags"));
        }
        Ok((element, count))
    }

    fn skip_list_items(&mut self, element: u8, count: u64, depth: u32) -> io::Result<()> {
        let fixed = match element {
            TAG_END => 0,
            TAG_BYTE => 1,
            TAG_SHORT => 2,
            TAG_INT | TAG_FLOAT => 4,
            TAG_LONG | TAG_DOUBLE => 8,
            _ => {
                check_depth(depth)?;
                for _ in 0..count {
                    self.skip_payload(element, depth + 1)?;
                }
                return Ok(());
            }
        };
        self.skip(count * fixed)
    }
}

fn check_depth(depth: u32) -> io::Result<()> {
    if depth >= MAX_DEPTH { Err(invalid("NBT is nested too deeply")) } else { Ok(()) }
}

struct ChunkParser<'a, R> {
    reader: Reader<R>,
    info: &'a mut ChunkInfo,
    /// Scratch buffer for tag names.
    name: Vec<u8>,
    found_time: bool,
    found_structures: bool,
}

impl<R: Read> ChunkParser<'_, R> {
    fn done(&self) -> bool {
        self.found_time && self.found_structures
    }

    fn name_string(&self) -> String {
        String::from_utf8_lossy(&self.name).into_owned()
    }

    /// Next entry of a compound: its tag type (with the name left in
    /// `self.name`), or `None` at the end of the compound.
    fn entry(&mut self) -> io::Result<Option<u8>> {
        let tag = self.reader.u8()?;
        if tag == TAG_END {
            return Ok(None);
        }
        self.reader.string_into(&mut self.name)?;
        Ok(Some(tag))
    }

    fn root(&mut self) -> io::Result<()> {
        while let Some(tag) = self.entry()? {
            match (tag, self.name.as_slice()) {
                (TAG_LONG, b"InhabitedTime") if !self.found_time => {
                    self.info.inhabited_time = Some(self.reader.i64()?);
                    self.found_time = true;
                }
                (TAG_COMPOUND, b"structures") if !self.found_structures => {
                    self.structures(b"starts", 1)?;
                    self.found_structures = true;
                }
                (TAG_COMPOUND, b"Level") => {
                    // Pre-1.18 chunks keep everything in `Level`. It takes
                    // precedence over root tags, like in the original tool.
                    self.info.clear();
                    self.found_time = false;
                    self.found_structures = false;
                    self.level()?;
                    // Whatever follows the legacy `Level` compound is irrelevant.
                    return Ok(());
                }
                _ => self.reader.skip_payload(tag, 1)?,
            }
            if self.done() {
                // A modern chunk never has a `Level` compound, so stop early.
                return Ok(());
            }
        }
        Ok(())
    }

    fn level(&mut self) -> io::Result<()> {
        while let Some(tag) = self.entry()? {
            match (tag, self.name.as_slice()) {
                (TAG_LONG, b"InhabitedTime") if !self.found_time => {
                    self.info.inhabited_time = Some(self.reader.i64()?);
                    self.found_time = true;
                }
                (TAG_COMPOUND, b"Structures") if !self.found_structures => {
                    self.structures(b"Starts", 2)?;
                    self.found_structures = true;
                }
                _ => self.reader.skip_payload(tag, 2)?,
            }
            if self.done() {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Parse the `structures` compound. `depth` is the nesting depth of the
    /// compound itself.
    fn structures(&mut self, starts_key: &[u8], depth: u32) -> io::Result<()> {
        while let Some(tag) = self.entry()? {
            if tag == TAG_COMPOUND && self.name == starts_key {
                self.starts(depth + 1)?;
            } else if tag == TAG_COMPOUND && self.name == b"References" {
                self.references(depth + 1)?;
            } else {
                self.reader.skip_payload(tag, depth + 1)?;
            }
        }
        Ok(())
    }

    fn references(&mut self, depth: u32) -> io::Result<()> {
        while let Some(tag) = self.entry()? {
            if tag != TAG_LONG_ARRAY {
                self.reader.skip_payload(tag, depth + 1)?;
                continue;
            }
            let len = self.reader.array_len()?;
            if len == 0 {
                continue;
            }
            let id = self.name_string();
            // Do not trust the length prefix for the allocation.
            let mut positions = Vec::with_capacity(len.min(1024) as usize);
            for _ in 0..len {
                positions.push(self.reader.i64()?);
            }
            self.info.references.push((id, positions));
        }
        Ok(())
    }

    fn starts(&mut self, depth: u32) -> io::Result<()> {
        while let Some(tag) = self.entry()? {
            if tag != TAG_COMPOUND {
                self.reader.skip_payload(tag, depth + 1)?;
                continue;
            }
            let mut start = StructureStart { id: self.name_string(), boxes: Vec::new() };
            let mut has_children = false;
            while let Some(tag) = self.entry()? {
                match (tag, self.name.as_slice()) {
                    (TAG_INT_ARRAY, b"BB") => {
                        if let Some(bb) = self.block_box()? {
                            start.boxes.push(bb);
                        }
                    }
                    (TAG_LIST, b"Children") => {
                        has_children = true;
                        self.children(&mut start.boxes, depth + 2)?;
                    }
                    _ => self.reader.skip_payload(tag, depth + 2)?,
                }
            }
            // Placeholder starts (`id: "INVALID"`) have no pieces at all and
            // only say "no structure starts here".
            if has_children {
                self.info.starts.push(start);
            }
        }
        Ok(())
    }

    /// Collect the `BB` of every structure piece in a `Children` list.
    fn children(&mut self, boxes: &mut Vec<BlockBox>, depth: u32) -> io::Result<()> {
        let (element, count) = self.reader.list_header()?;
        if element != TAG_COMPOUND {
            return self.reader.skip_list_items(element, count, depth);
        }
        for _ in 0..count {
            while let Some(tag) = self.entry()? {
                if tag == TAG_INT_ARRAY && self.name == b"BB" {
                    if let Some(bb) = self.block_box()? {
                        boxes.push(bb);
                    }
                } else {
                    self.reader.skip_payload(tag, depth + 2)?;
                }
            }
        }
        Ok(())
    }

    /// Read an int array as a bounding box. Arrays that are too short are
    /// ignored.
    fn block_box(&mut self) -> io::Result<Option<BlockBox>> {
        let len = self.reader.array_len()?;
        if len < 6 {
            self.reader.skip(len * 4)?;
            return Ok(None);
        }
        let mut v = [0i32; 6];
        for slot in &mut v {
            *slot = self.reader.i32()?;
        }
        self.reader.skip((len - 6) * 4)?;
        Ok(Some(BlockBox { min_x: v[0], min_z: v[2], max_x: v[3], max_z: v[5] }))
    }
}

/// A tiny NBT writer, used to build chunks for tests.
#[doc(hidden)]
pub mod write {
    use super::*;

    /// Builds the payload of a compound tag.
    #[derive(Default, Clone)]
    pub struct Compound {
        bytes: Vec<u8>,
    }

    impl Compound {
        pub fn new() -> Self {
            Compound::default()
        }

        fn header(&mut self, tag: u8, name: &str) {
            self.bytes.push(tag);
            self.bytes.extend_from_slice(&(name.len() as u16).to_be_bytes());
            self.bytes.extend_from_slice(name.as_bytes());
        }

        pub fn byte(mut self, name: &str, v: i8) -> Self {
            self.header(TAG_BYTE, name);
            self.bytes.push(v as u8);
            self
        }

        pub fn int(mut self, name: &str, v: i32) -> Self {
            self.header(TAG_INT, name);
            self.bytes.extend_from_slice(&v.to_be_bytes());
            self
        }

        pub fn long(mut self, name: &str, v: i64) -> Self {
            self.header(TAG_LONG, name);
            self.bytes.extend_from_slice(&v.to_be_bytes());
            self
        }

        pub fn double(mut self, name: &str, v: f64) -> Self {
            self.header(TAG_DOUBLE, name);
            self.bytes.extend_from_slice(&v.to_be_bytes());
            self
        }

        pub fn string(mut self, name: &str, v: &str) -> Self {
            self.header(TAG_STRING, name);
            self.bytes.extend_from_slice(&(v.len() as u16).to_be_bytes());
            self.bytes.extend_from_slice(v.as_bytes());
            self
        }

        pub fn byte_array(mut self, name: &str, v: &[u8]) -> Self {
            self.header(TAG_BYTE_ARRAY, name);
            self.bytes.extend_from_slice(&(v.len() as i32).to_be_bytes());
            self.bytes.extend_from_slice(v);
            self
        }

        pub fn int_array(mut self, name: &str, v: &[i32]) -> Self {
            self.header(TAG_INT_ARRAY, name);
            self.bytes.extend_from_slice(&(v.len() as i32).to_be_bytes());
            for i in v {
                self.bytes.extend_from_slice(&i.to_be_bytes());
            }
            self
        }

        pub fn long_array(mut self, name: &str, v: &[i64]) -> Self {
            self.header(TAG_LONG_ARRAY, name);
            self.bytes.extend_from_slice(&(v.len() as i32).to_be_bytes());
            for i in v {
                self.bytes.extend_from_slice(&i.to_be_bytes());
            }
            self
        }

        pub fn compound(mut self, name: &str, v: Compound) -> Self {
            self.header(TAG_COMPOUND, name);
            self.bytes.extend_from_slice(&v.bytes);
            self.bytes.push(TAG_END);
            self
        }

        pub fn compound_list(mut self, name: &str, items: Vec<Compound>) -> Self {
            self.header(TAG_LIST, name);
            self.bytes.push(if items.is_empty() { TAG_END } else { TAG_COMPOUND });
            self.bytes.extend_from_slice(&(items.len() as i32).to_be_bytes());
            for item in items {
                self.bytes.extend_from_slice(&item.bytes);
                self.bytes.push(TAG_END);
            }
            self
        }

        pub fn long_list(mut self, name: &str, items: &[i64]) -> Self {
            self.header(TAG_LIST, name);
            self.bytes.push(TAG_LONG);
            self.bytes.extend_from_slice(&(items.len() as i32).to_be_bytes());
            for i in items {
                self.bytes.extend_from_slice(&i.to_be_bytes());
            }
            self
        }

        /// Serialise as a complete NBT document with an unnamed root.
        pub fn into_root(self) -> Vec<u8> {
            let mut out = vec![TAG_COMPOUND, 0, 0];
            out.extend_from_slice(&self.bytes);
            out.push(TAG_END);
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::write::Compound;
    use super::*;

    fn parse(bytes: &[u8]) -> io::Result<ChunkInfo> {
        let mut info = ChunkInfo::default();
        read_chunk_info(bytes, &mut info)?;
        Ok(info)
    }

    /// Feeds one byte at a time, to exercise buffer refills everywhere.
    struct Trickle<'a>(&'a [u8]);
    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.0.is_empty() || buf.is_empty() {
                return Ok(0);
            }
            buf[0] = self.0[0];
            self.0 = &self.0[1..];
            Ok(1)
        }
    }

    fn piece(bb: [i32; 6]) -> Compound {
        Compound::new().string("id", "minecraft:jigsaw").int_array("BB", &bb).int("GD", 0)
    }

    fn modern_chunk() -> Vec<u8> {
        Compound::new()
            .int("DataVersion", 4790)
            .string("Status", "minecraft:full")
            .compound_list("sections", vec![Compound::new().byte("Y", -4).byte_array("SkyLight", &[7; 2048])])
            .long_list("PostProcessing", &[1, 2, 3])
            .compound(
                "structures",
                Compound::new()
                    .compound(
                        "starts",
                        Compound::new()
                            .compound(
                                "minecraft:village_plains",
                                Compound::new()
                                    .string("id", "minecraft:village_plains")
                                    .int("ChunkX", 3)
                                    .int("ChunkZ", -2)
                                    .int("references", 0)
                                    .compound_list("Children", vec![piece([48, 60, -32, 60, 70, -20]), piece([64, 60, -40, 90, 80, -1])]),
                            )
                            .compound("minecraft:mineshaft", Compound::new().string("id", "INVALID")),
                    )
                    .compound(
                        "References",
                        Compound::new()
                            .long_array("minecraft:village_plains", &[crate::pos::ChunkPos::new(3, -2).to_long()])
                            .long_array("minecraft:empty", &[]),
                    ),
            )
            .long("InhabitedTime", 4242)
            .double("ignored", 1.5)
            .into_root()
    }

    fn expected_modern() -> ChunkInfo {
        ChunkInfo {
            inhabited_time: Some(4242),
            starts: vec![StructureStart {
                id: "minecraft:village_plains".into(),
                boxes: vec![BlockBox { min_x: 48, min_z: -32, max_x: 60, max_z: -20 }, BlockBox { min_x: 64, min_z: -40, max_x: 90, max_z: -1 }],
            }],
            references: vec![("minecraft:village_plains".into(), vec![crate::pos::ChunkPos::new(3, -2).to_long()])],
        }
    }

    #[test]
    fn modern_layout() {
        assert_eq!(parse(&modern_chunk()).unwrap(), expected_modern());
    }

    #[test]
    fn works_across_buffer_refills() {
        let mut info = ChunkInfo::default();
        read_chunk_info(Trickle(&modern_chunk()), &mut info).unwrap();
        assert_eq!(info, expected_modern());
    }

    #[test]
    fn legacy_layout() {
        let bytes = Compound::new()
            .int("DataVersion", 2586)
            .compound(
                "Level",
                Compound::new().int("xPos", 1).long("InhabitedTime", 99).compound(
                    "Structures",
                    Compound::new()
                        .compound(
                            "Starts",
                            Compound::new().compound(
                                "Village",
                                Compound::new().int_array("BB", &[0, 0, 0, 100, 50, 100]).compound_list("Children", vec![piece([1, 2, 3, 4, 5, 6])]),
                            ),
                        )
                        .compound("References", Compound::new().long_array("Village", &[7, 8])),
                ),
            )
            .into_root();
        let info = parse(&bytes).unwrap();
        assert_eq!(info.inhabited_time, Some(99));
        assert_eq!(
            info.starts,
            vec![StructureStart {
                id: "Village".into(),
                boxes: vec![BlockBox { min_x: 0, min_z: 0, max_x: 100, max_z: 100 }, BlockBox { min_x: 1, min_z: 3, max_x: 4, max_z: 6 },],
            }]
        );
        assert_eq!(info.references, vec![("Village".to_string(), vec![7, 8])]);
    }

    #[test]
    fn stops_reading_once_everything_is_found() {
        let mut bytes = Compound::new().long("InhabitedTime", 5).compound("structures", Compound::new()).into_root();
        bytes.pop(); // drop the root's end tag...
        bytes.extend_from_slice(&[99, 1, 2, 3]); // ...and append garbage
        let info = parse(&bytes).unwrap();
        assert_eq!(info.inhabited_time, Some(5));
    }

    #[test]
    fn missing_tags() {
        let info = parse(&Compound::new().int("DataVersion", 1).into_root()).unwrap();
        assert_eq!(info, ChunkInfo::default());
        // InhabitedTime of the wrong type is ignored, like in the original.
        let info = parse(&Compound::new().int("InhabitedTime", 5).into_root()).unwrap();
        assert_eq!(info.inhabited_time, None);
        // short BB arrays are ignored
        let bytes = Compound::new()
            .compound(
                "structures",
                Compound::new().compound(
                    "starts",
                    Compound::new().compound("a:b", Compound::new().compound_list("Children", vec![Compound::new().int_array("BB", &[1, 2, 3])])),
                ),
            )
            .into_root();
        assert_eq!(parse(&bytes).unwrap().starts, vec![StructureStart { id: "a:b".into(), boxes: vec![] }]);
    }

    #[test]
    fn malformed_input_is_an_error_not_a_panic() {
        let good = modern_chunk();
        assert!(parse(&[]).is_err());
        assert!(parse(&[TAG_LONG, 0, 0]).is_err());
        // every strict prefix that ends before all data was found must fail cleanly
        for len in 0..good.len() {
            let _ = parse(&good[..len]);
        }
        assert!(parse(&good[..good.len() / 2]).is_err());
        // unknown tag type
        assert!(parse(&[TAG_COMPOUND, 0, 0, 42, 0, 1, b'x', 0]).is_err());
        // negative array length
        let mut bad = vec![TAG_COMPOUND, 0, 0, TAG_BYTE_ARRAY, 0, 1, b'x'];
        bad.extend_from_slice(&(-1i32).to_be_bytes());
        assert!(parse(&bad).is_err());
        // absurd nesting depth
        let mut deep = vec![TAG_COMPOUND, 0, 0];
        for _ in 0..600 {
            deep.extend_from_slice(&[TAG_COMPOUND, 0, 1, b'x']);
        }
        assert!(parse(&deep).is_err());
        // deeply nested lists
        let mut lists = vec![TAG_COMPOUND, 0, 0, TAG_LIST, 0, 1, b'x'];
        for _ in 0..600 {
            lists.extend_from_slice(&[TAG_LIST, 0, 0, 0, 1]);
        }
        assert!(parse(&lists).is_err());
    }
}
