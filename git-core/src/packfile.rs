/// Git packfile parsing and generation (v2, with ref_delta support).
///
/// Packfile format:
///   "PACK" (4 bytes magic)
///   version (4 bytes big-endian, = 2)
///   num_objects (4 bytes big-endian)
///   [objects...]
///   sha1_checksum (20 bytes over everything before it)

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use sha1::{Digest, Sha1};
use std::io::{Read, Write};

/// A parsed git object from a packfile.
#[derive(Debug, Clone)]
pub struct PackObject {
    pub obj_type: String,
    pub data: Vec<u8>,
}

/// An unresolved ref_delta from a packfile.
#[derive(Debug, Clone)]
pub struct UnresolvedDelta {
    pub base_sha: String,
    pub delta_data: Vec<u8>,
}

/// Result of parsing a packfile.
#[derive(Debug)]
pub struct ParseResult {
    pub objects: Vec<PackObject>,
    pub deltas: Vec<UnresolvedDelta>,
}

impl PackObject {
    /// Compute the git SHA-1 for this object.
    pub fn sha1(&self) -> String {
        let header = format!("{} {}\0", self.obj_type, self.data.len());
        let mut hasher = Sha1::new();
        hasher.update(header.as_bytes());
        hasher.update(&self.data);
        let result = hasher.finalize();
        result.iter().map(|b| format!("{:02x}", b)).collect()
    }
}

fn type_from_bits(bits: u8) -> Option<&'static str> {
    match bits {
        1 => Some("commit"),
        2 => Some("tree"),
        3 => Some("blob"),
        4 => Some("tag"),
        _ => None,
    }
}

fn type_to_bits(obj_type: &str) -> u8 {
    match obj_type {
        "commit" => 1,
        "tree" => 2,
        "blob" => 3,
        "tag" => 4,
        _ => panic!("unknown object type: {}", obj_type),
    }
}

/// Parse a packfile, returning full objects and unresolved deltas.
pub fn parse(data: &[u8]) -> Result<ParseResult, String> {
    if data.len() < 12 {
        return Err("packfile too short".into());
    }

    if &data[0..4] != b"PACK" {
        return Err("invalid packfile magic".into());
    }

    let version = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    if version != 2 && version != 3 {
        return Err(format!("unsupported packfile version: {}", version));
    }

    let num_objects = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
    let mut pos = 12;

    // First pass: parse all entries, tracking start positions
    enum ParsedEntry {
        Object(PackObject),
        RefDelta(UnresolvedDelta),
        OfsDelta { base_pos: usize, delta_data: Vec<u8> },
    }

    let mut entries: Vec<(usize, ParsedEntry)> = Vec::new(); // (start_pos, entry)

    for _ in 0..num_objects {
        if pos >= data.len() {
            return Err("unexpected end of packfile".into());
        }

        let obj_start = pos;

        // Read type and size (variable-length encoding)
        let first_byte = data[pos];
        let type_bits = (first_byte >> 4) & 0x07;
        let mut size: u64 = (first_byte & 0x0f) as u64;
        let mut shift = 4;
        pos += 1;

        let mut prev_byte = first_byte;
        while prev_byte & 0x80 != 0 {
            if pos >= data.len() {
                return Err("unexpected end of packfile in size".into());
            }
            prev_byte = data[pos];
            size |= ((prev_byte & 0x7f) as u64) << shift;
            shift += 7;
            pos += 1;
        }

        match type_bits {
            7 => {
                // REF_DELTA: 20-byte base SHA + zlib compressed delta
                if pos + 20 > data.len() {
                    return Err("unexpected end of packfile in ref_delta base SHA".into());
                }
                let base_sha: String = data[pos..pos + 20]
                    .iter()
                    .map(|b| format!("{:02x}", b))
                    .collect();
                pos += 20;

                let mut decoder = ZlibDecoder::new(&data[pos..]);
                let mut delta_data = Vec::with_capacity(size as usize);
                decoder
                    .read_to_end(&mut delta_data)
                    .map_err(|e| format!("zlib decompression failed for delta: {}", e))?;
                pos += decoder.total_in() as usize;

                entries.push((obj_start, ParsedEntry::RefDelta(UnresolvedDelta {
                    base_sha,
                    delta_data,
                })));
            }
            6 => {
                // OFS_DELTA: variable-length negative offset + zlib compressed delta
                let mut byte = data[pos];
                pos += 1;
                let mut offset: u64 = (byte & 0x7f) as u64;
                while byte & 0x80 != 0 {
                    byte = data[pos];
                    pos += 1;
                    offset = ((offset + 1) << 7) | (byte & 0x7f) as u64;
                }

                let base_pos = obj_start.checked_sub(offset as usize)
                    .ok_or_else(|| format!("ofs_delta offset {} exceeds object position {}", offset, obj_start))?;

                let mut decoder = ZlibDecoder::new(&data[pos..]);
                let mut delta_data = Vec::with_capacity(size as usize);
                decoder
                    .read_to_end(&mut delta_data)
                    .map_err(|e| format!("zlib decompression failed for ofs_delta: {}", e))?;
                pos += decoder.total_in() as usize;

                entries.push((obj_start, ParsedEntry::OfsDelta { base_pos, delta_data }));
            }
            _ => {
                let obj_type = type_from_bits(type_bits)
                    .ok_or_else(|| format!("unknown object type: {}", type_bits))?;

                let mut decoder = ZlibDecoder::new(&data[pos..]);
                let mut decompressed = Vec::with_capacity(size as usize);
                decoder
                    .read_to_end(&mut decompressed)
                    .map_err(|e| format!("zlib decompression failed: {}", e))?;
                pos += decoder.total_in() as usize;

                entries.push((obj_start, ParsedEntry::Object(PackObject {
                    obj_type: obj_type.to_string(),
                    data: decompressed,
                })));
            }
        }
    }

    // Build position-to-index map for OFS_DELTA resolution
    let mut pos_to_idx: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for (i, (start_pos, _)) in entries.iter().enumerate() {
        pos_to_idx.insert(*start_pos, i);
    }

    // Resolve OFS_DELTAs: recursively find the base object and apply delta chain
    fn resolve_ofs(
        idx: usize,
        entries: &[(usize, ParsedEntry)],
        pos_to_idx: &std::collections::HashMap<usize, usize>,
        resolved: &mut Vec<Option<PackObject>>,
    ) -> Result<PackObject, String> {
        if let Some(obj) = &resolved[idx] {
            return Ok(obj.clone());
        }
        match &entries[idx].1 {
            ParsedEntry::Object(obj) => {
                let obj = obj.clone();
                resolved[idx] = Some(obj.clone());
                Ok(obj)
            }
            ParsedEntry::OfsDelta { base_pos, delta_data } => {
                let base_idx = pos_to_idx.get(base_pos)
                    .ok_or_else(|| format!("ofs_delta base at offset {} not found", base_pos))?;
                let base_obj = resolve_ofs(*base_idx, entries, pos_to_idx, resolved)?;
                let resolved_data = apply_delta(&base_obj.data, delta_data)?;
                let obj = PackObject {
                    obj_type: base_obj.obj_type.clone(),
                    data: resolved_data,
                };
                resolved[idx] = Some(obj.clone());
                Ok(obj)
            }
            ParsedEntry::RefDelta(_) => {
                Err("cannot resolve ref_delta as ofs_delta base".into())
            }
        }
    }

    let mut resolved: Vec<Option<PackObject>> = vec![None; entries.len()];
    let mut objects = Vec::new();
    let mut deltas = Vec::new();

    for i in 0..entries.len() {
        match &entries[i].1 {
            ParsedEntry::Object(_) => {
                let obj = resolve_ofs(i, &entries, &pos_to_idx, &mut resolved)?;
                objects.push(obj);
            }
            ParsedEntry::OfsDelta { .. } => {
                let obj = resolve_ofs(i, &entries, &pos_to_idx, &mut resolved)?;
                objects.push(obj);
            }
            ParsedEntry::RefDelta(_) => {
                // Extract the delta - we need to destructure but entries is borrowed
                // Just match again to get the data
                if let ParsedEntry::RefDelta(d) = &entries[i].1 {
                    deltas.push(UnresolvedDelta {
                        base_sha: d.base_sha.clone(),
                        delta_data: d.delta_data.clone(),
                    });
                }
            }
        }
    }

    Ok(ParseResult { objects, deltas })
}

/// Apply a git binary delta to a source object, producing the target.
pub fn apply_delta(source: &[u8], delta: &[u8]) -> Result<Vec<u8>, String> {
    let mut pos = 0;

    // Read source size (variable-length)
    let (_source_size, bytes_read) = read_delta_size(delta, pos)?;
    pos += bytes_read;

    // Read target size (variable-length)
    let (target_size, bytes_read) = read_delta_size(delta, pos)?;
    pos += bytes_read;

    let mut target = Vec::with_capacity(target_size as usize);

    while pos < delta.len() {
        let instruction = delta[pos];
        pos += 1;

        if instruction & 0x80 != 0 {
            // Copy from source
            let mut offset: u32 = 0;
            let mut size: u32 = 0;

            if instruction & 0x01 != 0 {
                offset |= delta[pos] as u32;
                pos += 1;
            }
            if instruction & 0x02 != 0 {
                offset |= (delta[pos] as u32) << 8;
                pos += 1;
            }
            if instruction & 0x04 != 0 {
                offset |= (delta[pos] as u32) << 16;
                pos += 1;
            }
            if instruction & 0x08 != 0 {
                offset |= (delta[pos] as u32) << 24;
                pos += 1;
            }
            if instruction & 0x10 != 0 {
                size |= delta[pos] as u32;
                pos += 1;
            }
            if instruction & 0x20 != 0 {
                size |= (delta[pos] as u32) << 8;
                pos += 1;
            }
            if instruction & 0x40 != 0 {
                size |= (delta[pos] as u32) << 16;
                pos += 1;
            }
            if size == 0 {
                size = 0x10000;
            }

            let start = offset as usize;
            let end = start + size as usize;
            if end > source.len() {
                return Err(format!(
                    "delta copy out of bounds: offset={}, size={}, source_len={}",
                    offset,
                    size,
                    source.len()
                ));
            }
            target.extend_from_slice(&source[start..end]);
        } else if instruction != 0 {
            // Insert literal data
            let size = instruction as usize;
            if pos + size > delta.len() {
                return Err("delta insert out of bounds".into());
            }
            target.extend_from_slice(&delta[pos..pos + size]);
            pos += size;
        } else {
            return Err("delta instruction byte 0 is reserved".into());
        }
    }

    if target.len() != target_size as usize {
        return Err(format!(
            "delta target size mismatch: expected {}, got {}",
            target_size,
            target.len()
        ));
    }

    Ok(target)
}

fn read_delta_size(data: &[u8], start: usize) -> Result<(u64, usize), String> {
    let mut pos = start;
    let mut size: u64 = 0;
    let mut shift = 0;

    loop {
        if pos >= data.len() {
            return Err("unexpected end of delta size".into());
        }
        let byte = data[pos];
        pos += 1;
        size |= ((byte & 0x7f) as u64) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            break;
        }
    }

    Ok((size, pos - start))
}

/// Zlib-compress data for on-chain storage.
pub fn zlib_compress(data: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

/// Zlib-decompress data retrieved from on-chain storage.
pub fn zlib_decompress(data: &[u8]) -> Vec<u8> {
    let mut decoder = ZlibDecoder::new(data);
    let mut decompressed = Vec::new();
    std::io::Read::read_to_end(&mut decoder, &mut decompressed).unwrap();
    decompressed
}

/// Compute a binary delta from `base` to `target` using git's delta format.
///
/// The delta can be applied with `apply_delta(base, delta)` to reconstruct `target`.
/// Returns delta bytes that are typically much smaller than `target` when the two
/// inputs are similar.
pub fn compute_delta(base: &[u8], target: &[u8]) -> Vec<u8> {
    use std::collections::HashMap;

    const BLOCK_SIZE: usize = 16;

    fn fnv_hash(data: &[u8]) -> u64 {
        let mut hash: u64 = 0xcbf29ce484222325;
        for &b in data {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash
    }

    fn encode_size(buf: &mut Vec<u8>, mut val: usize) {
        loop {
            let mut byte = (val & 0x7f) as u8;
            val >>= 7;
            if val > 0 { byte |= 0x80; }
            buf.push(byte);
            if val == 0 { break; }
        }
    }

    fn emit_copy(buf: &mut Vec<u8>, offset: usize, size: usize) {
        let mut cmd: u8 = 0x80;
        let mut data = Vec::new();
        for i in 0..4 {
            let byte = ((offset >> (i * 8)) & 0xff) as u8;
            if byte != 0 { cmd |= 1 << i; data.push(byte); }
        }
        let enc_size = if size == 0x10000 { 0 } else { size };
        for i in 0..3 {
            let byte = ((enc_size >> (i * 8)) & 0xff) as u8;
            if byte != 0 { cmd |= 1 << (4 + i); data.push(byte); }
        }
        buf.push(cmd);
        buf.extend_from_slice(&data);
    }

    fn flush_inserts(buf: &mut Vec<u8>, insert_buf: &mut Vec<u8>) {
        while !insert_buf.is_empty() {
            let count = insert_buf.len().min(127);
            buf.push(count as u8);
            buf.extend_from_slice(&insert_buf[..count]);
            insert_buf.drain(..count);
        }
    }

    // Build index of BLOCK_SIZE-byte windows in base
    let mut index: HashMap<u64, Vec<usize>> = HashMap::new();
    if base.len() >= BLOCK_SIZE {
        for i in (0..=base.len() - BLOCK_SIZE).step_by(BLOCK_SIZE) {
            index.entry(fnv_hash(&base[i..i + BLOCK_SIZE])).or_default().push(i);
        }
    }

    let mut result = Vec::new();
    encode_size(&mut result, base.len());
    encode_size(&mut result, target.len());

    let mut pos = 0;
    let mut insert_buf: Vec<u8> = Vec::new();

    while pos < target.len() {
        let remaining = target.len() - pos;
        let mut best_offset = 0usize;
        let mut best_len = 0usize;

        if remaining >= BLOCK_SIZE {
            let hash = fnv_hash(&target[pos..pos + BLOCK_SIZE]);
            if let Some(offsets) = index.get(&hash) {
                for &base_off in offsets {
                    let max_len = remaining.min(base.len() - base_off);
                    let mut len = 0;
                    while len < max_len && target[pos + len] == base[base_off + len] {
                        len += 1;
                    }
                    if len > best_len { best_len = len; best_offset = base_off; }
                }
            }
        }

        if best_len >= BLOCK_SIZE {
            flush_inserts(&mut result, &mut insert_buf);
            let mut copied = 0;
            while copied < best_len {
                let chunk = (best_len - copied).min(0x10000);
                emit_copy(&mut result, best_offset + copied, chunk);
                copied += chunk;
            }
            pos += best_len;
        } else {
            insert_buf.push(target[pos]);
            pos += 1;
        }
    }

    flush_inserts(&mut result, &mut insert_buf);
    result
}

/// Build a packfile from a list of objects.
/// Build a packfile from objects, with OFS_DELTA compression.
///
/// Objects of the same type are sorted by size (largest first). Each object
/// is delta-compressed against the previous object of the same type if the
/// delta is smaller. This matches git's heuristic for intra-pack deltas.
pub fn build(objects: &[PackObject]) -> Vec<u8> {
    build_with_bases(objects, &[])
}

/// Build a thin packfile: objects delta-compressed against external bases.
///
/// `base_objects` are available for delta computation but NOT included in
/// the output pack. Uses REF_DELTA (type 7) to reference bases by SHA.
/// The receiver needs the base objects from a previous pack to resolve them.
pub fn build_with_bases(objects: &[PackObject], base_objects: &[PackObject]) -> Vec<u8> {
    // Build a map of base objects by type for delta matching
    let mut bases_by_type: std::collections::HashMap<&str, Vec<&PackObject>> =
        std::collections::HashMap::new();
    for obj in base_objects {
        bases_by_type.entry(&obj.obj_type).or_default().push(obj);
    }

    // For each object, try to find the best delta base (from bases or intra-pack)
    struct Entry<'a> {
        obj: &'a PackObject,
        /// REF_DELTA against external base (sha, delta_data)
        ref_delta: Option<(String, Vec<u8>)>,
        /// OFS_DELTA against intra-pack entry at this index
        ofs_delta_idx: Option<usize>,
        ofs_delta_data: Option<Vec<u8>>,
    }

    let mut entries: Vec<Entry> = Vec::with_capacity(objects.len());
    let mut last_by_type: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();

    // Sort objects by type+size for better intra-pack deltas
    let mut sorted_indices: Vec<usize> = (0..objects.len()).collect();
    sorted_indices.sort_by(|&a, &b| {
        objects[a].obj_type.cmp(&objects[b].obj_type)
            .then(objects[b].data.len().cmp(&objects[a].data.len()))
    });

    // Map from original index to entry index
    let mut orig_to_entry: Vec<usize> = vec![0; objects.len()];

    for &orig_idx in &sorted_indices {
        let obj = &objects[orig_idx];
        let entry_idx = entries.len();
        orig_to_entry[orig_idx] = entry_idx;

        let mut best_delta: Option<Vec<u8>> = None;
        let mut best_base_sha: Option<String> = None;
        let mut best_ofs_idx: Option<usize> = None;

        // Try external bases first (usually better — previous version of same file)
        if let Some(bases) = bases_by_type.get(obj.obj_type.as_str()) {
            for base in bases {
                let delta = compute_delta(&base.data, &obj.data);
                if delta.len() < obj.data.len() * 9 / 10 {
                    if best_delta.is_none() || delta.len() < best_delta.as_ref().unwrap().len() {
                        best_delta = Some(delta);
                        best_base_sha = Some(base.sha1());
                        best_ofs_idx = None;
                    }
                }
            }
        }

        // Try intra-pack delta
        if let Some(&prev_entry_idx) = last_by_type.get(obj.obj_type.as_str()) {
            let base = entries[prev_entry_idx].obj;
            let delta = compute_delta(&base.data, &obj.data);
            if delta.len() < obj.data.len() * 9 / 10 {
                if best_delta.is_none() || delta.len() < best_delta.as_ref().unwrap().len() {
                    best_delta = Some(delta);
                    best_base_sha = None;
                    best_ofs_idx = Some(prev_entry_idx);
                }
            }
        }

        let entry = if let Some(sha) = best_base_sha {
            Entry { obj, ref_delta: Some((sha, best_delta.unwrap())), ofs_delta_idx: None, ofs_delta_data: None }
        } else if let Some(idx) = best_ofs_idx {
            Entry { obj, ref_delta: None, ofs_delta_idx: Some(idx), ofs_delta_data: best_delta }
        } else {
            Entry { obj, ref_delta: None, ofs_delta_idx: None, ofs_delta_data: None }
        };

        last_by_type.insert(&obj.obj_type, entry_idx);
        entries.push(entry);
    }

    // Second pass: write the packfile
    let mut out = Vec::new();
    out.extend_from_slice(b"PACK");
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());

    let mut start_positions: Vec<usize> = Vec::with_capacity(entries.len());

    for entry in entries.iter() {
        let obj_start = out.len();
        start_positions.push(obj_start);

        if let Some((ref base_sha, ref delta)) = entry.ref_delta {
            // REF_DELTA (type 7): reference base by SHA
            encode_pack_header(&mut out, 7, delta.len() as u64);
            // 20-byte base SHA
            let sha_bytes: Vec<u8> = (0..20)
                .map(|i| u8::from_str_radix(&base_sha[i*2..i*2+2], 16).unwrap())
                .collect();
            out.extend_from_slice(&sha_bytes);
            // Zlib-compressed delta
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(delta).unwrap();
            out.extend_from_slice(&encoder.finish().unwrap());
        } else if let (Some(base_idx), Some(delta)) = (entry.ofs_delta_idx, &entry.ofs_delta_data) {
            // OFS_DELTA (type 6): reference base by offset
            let base_pos = start_positions[base_idx];
            let offset = obj_start - base_pos;
            encode_pack_header(&mut out, 6, delta.len() as u64);
            encode_ofs_offset(&mut out, offset as u64);
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(delta).unwrap();
            out.extend_from_slice(&encoder.finish().unwrap());
        } else {
            // Full object
            let type_bits = type_to_bits(&entry.obj.obj_type);
            encode_pack_header(&mut out, type_bits, entry.obj.data.len() as u64);
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&entry.obj.data).unwrap();
            out.extend_from_slice(&encoder.finish().unwrap());
        }
    }

    // Trailing SHA-1 checksum
    let mut hasher = Sha1::new();
    hasher.update(&out);
    out.extend_from_slice(&hasher.finalize());

    out
}

/// Encode pack object header (type + size, variable-length).
fn encode_pack_header(out: &mut Vec<u8>, type_bits: u8, size: u64) {
    let mut first_byte = (type_bits << 4) | (size & 0x0f) as u8;
    let mut remaining = size >> 4;
    if remaining > 0 { first_byte |= 0x80; }
    out.push(first_byte);
    while remaining > 0 {
        let mut byte = (remaining & 0x7f) as u8;
        remaining >>= 7;
        if remaining > 0 { byte |= 0x80; }
        out.push(byte);
    }
}

/// Encode OFS_DELTA negative offset (variable-length, MSB continuation).
fn encode_ofs_offset(out: &mut Vec<u8>, offset: u64) {
    let mut bytes = Vec::new();
    let mut off = offset;
    bytes.push((off & 0x7f) as u8);
    off >>= 7;
    while off > 0 {
        off -= 1;
        bytes.push((off & 0x7f) as u8 | 0x80);
        off >>= 7;
    }
    bytes.reverse();
    out.extend_from_slice(&bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip() {
        let objects = vec![
            PackObject {
                obj_type: "blob".to_string(),
                data: b"hello world\n".to_vec(),
            },
            PackObject {
                obj_type: "blob".to_string(),
                data: b"another file\n".to_vec(),
            },
        ];

        let packed = build(&objects);
        let result = parse(&packed).unwrap();

        assert_eq!(result.objects.len(), 2);
        assert!(result.deltas.is_empty());
        // Objects may be reordered by the delta-aware builder (sorted by type+size)
        let mut datas: Vec<&[u8]> = result.objects.iter().map(|o| o.data.as_slice()).collect();
        datas.sort();
        assert!(datas.contains(&b"hello world\n".as_slice()));
        assert!(datas.contains(&b"another file\n".as_slice()));
    }

    #[test]
    fn test_sha1() {
        let obj = PackObject {
            obj_type: "blob".to_string(),
            data: b"hello world".to_vec(),
        };
        assert_eq!(obj.sha1(), "95d09f2b10159347eece71399a7e2e907ea3df4f");
    }

    #[test]
    fn test_apply_delta() {
        // Simple delta: copy all of source then insert " world"
        let source = b"hello";
        // Build a delta manually:
        // source_size = 5, target_size = 11
        // copy: offset=0, size=5
        // insert: " world"
        let delta = vec![
            5,  // source size
            11, // target size
            // Copy instruction: 0x80 | 0x01 (offset byte) | 0x10 (size byte)
            0x91, 0x00, // offset = 0
            0x05, // size = 5
            // Insert instruction: 6 bytes
            6, b' ', b'w', b'o', b'r', b'l', b'd',
        ];

        let result = apply_delta(source, &delta).unwrap();
        assert_eq!(result, b"hello world");
    }

    #[test]
    fn test_ofs_delta() {
        // Build a packfile with one base blob + one OFS_DELTA referencing it
        let base_data = b"hello world\n";
        let target_data = b"hello world! updated\n";

        // Compute the delta from base to target
        let delta_bytes = compute_delta(base_data, target_data);

        let mut pack = Vec::new();

        // Header: PACK v2, 2 objects
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&2u32.to_be_bytes());

        // Object 1: blob "hello world\n"
        let obj1_start = pack.len();
        {
            let type_bits: u8 = 3; // blob
            let size = base_data.len() as u64;
            let mut first_byte = (type_bits << 4) | (size & 0x0f) as u8;
            let mut remaining = size >> 4;
            if remaining > 0 { first_byte |= 0x80; }
            pack.push(first_byte);
            while remaining > 0 {
                let mut byte = (remaining & 0x7f) as u8;
                remaining >>= 7;
                if remaining > 0 { byte |= 0x80; }
                pack.push(byte);
            }
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(base_data).unwrap();
            pack.extend_from_slice(&encoder.finish().unwrap());
        }

        // Object 2: OFS_DELTA referencing object 1
        let obj2_start = pack.len();
        {
            let type_bits: u8 = 6; // OFS_DELTA
            let size = delta_bytes.len() as u64;
            let mut first_byte = (type_bits << 4) | (size & 0x0f) as u8;
            let mut remaining = size >> 4;
            if remaining > 0 { first_byte |= 0x80; }
            pack.push(first_byte);
            while remaining > 0 {
                let mut byte = (remaining & 0x7f) as u8;
                remaining >>= 7;
                if remaining > 0 { byte |= 0x80; }
                pack.push(byte);
            }

            // Encode negative offset (obj2_start - obj1_start)
            let offset = obj2_start - obj1_start;
            // Variable-length encoding: last byte has MSB=0, previous bytes have MSB=1
            // Encoding: first byte = offset & 0x7f, then (offset = (offset >> 7) - 1) while > 0
            let mut offset_bytes = Vec::new();
            let mut off = offset as u64;
            offset_bytes.push((off & 0x7f) as u8);
            off >>= 7;
            while off > 0 {
                off -= 1;
                offset_bytes.push((off & 0x7f) as u8 | 0x80);
                off >>= 7;
            }
            offset_bytes.reverse();
            pack.extend_from_slice(&offset_bytes);

            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&delta_bytes).unwrap();
            pack.extend_from_slice(&encoder.finish().unwrap());
        }

        // Trailing SHA-1
        let mut hasher = Sha1::new();
        hasher.update(&pack);
        let checksum = hasher.finalize();
        pack.extend_from_slice(&checksum);

        // Parse and verify
        let result = parse(&pack).unwrap();
        assert_eq!(result.objects.len(), 2, "should have 2 resolved objects");
        assert!(result.deltas.is_empty(), "should have no unresolved deltas");
        assert_eq!(result.objects[0].data, base_data);
        assert_eq!(result.objects[1].data, target_data);
        assert_eq!(result.objects[1].obj_type, "blob");
    }

    #[test]
    fn test_build_delta_compression() {
        // Two similar blobs — the builder should delta-compress the smaller one
        let base = b"line 1: hello world content here\nline 2: more content\nline 3: even more\n";
        let target = b"line 1: hello world content HERE\nline 2: more content\nline 3: even more\n";

        let objects = vec![
            PackObject { obj_type: "blob".to_string(), data: base.to_vec() },
            PackObject { obj_type: "blob".to_string(), data: target.to_vec() },
        ];

        let packed_with_delta = build(&objects);

        // Build without delta for comparison: use individual full objects
        let mut packed_no_delta = Vec::new();
        packed_no_delta.extend_from_slice(b"PACK");
        packed_no_delta.extend_from_slice(&2u32.to_be_bytes());
        packed_no_delta.extend_from_slice(&2u32.to_be_bytes());
        for obj in &objects {
            encode_pack_header(&mut packed_no_delta, type_to_bits(&obj.obj_type), obj.data.len() as u64);
            let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
            enc.write_all(&obj.data).unwrap();
            packed_no_delta.extend_from_slice(&enc.finish().unwrap());
        }
        let mut hasher = Sha1::new();
        hasher.update(&packed_no_delta);
        packed_no_delta.extend_from_slice(&hasher.finalize());

        eprintln!("Pack with delta: {} bytes, without: {} bytes",
            packed_with_delta.len(), packed_no_delta.len());
        assert!(
            packed_with_delta.len() < packed_no_delta.len(),
            "Delta pack ({}) should be smaller than full pack ({})",
            packed_with_delta.len(), packed_no_delta.len()
        );

        // Verify roundtrip — all content preserved
        let result = parse(&packed_with_delta).unwrap();
        assert_eq!(result.objects.len(), 2);
        assert!(result.deltas.is_empty());
        let mut datas: Vec<Vec<u8>> = result.objects.iter().map(|o| o.data.clone()).collect();
        datas.sort();
        assert!(datas.contains(&base.to_vec()));
        assert!(datas.contains(&target.to_vec()));
    }

    #[test]
    fn test_build_with_bases_cross_pack_delta() {
        // Simulate incremental push: base blob is already on-chain,
        // new blob is a small edit. build_with_bases should produce a
        // thin pack with REF_DELTA, much smaller than the full blob.
        let base_data: Vec<u8> = (0..200)
            .map(|i| format!("line {}: hello world content\n", i))
            .collect::<String>().into_bytes();

        let mut new_data = base_data.clone();
        new_data[500] = b'X'; // 1-byte change

        let base_obj = PackObject { obj_type: "blob".to_string(), data: base_data.clone() };
        let new_obj = PackObject { obj_type: "blob".to_string(), data: new_data.clone() };

        // Pack without bases (full blob)
        let pack_full = build(&[new_obj.clone()]);
        // Pack with base (thin pack with REF_DELTA)
        let pack_delta = build_with_bases(&[new_obj.clone()], &[base_obj.clone()]);

        eprintln!("Cross-pack delta: full={} bytes, with_bases={} bytes",
            pack_full.len(), pack_delta.len());

        assert!(
            pack_delta.len() < pack_full.len(),
            "Thin pack ({}) should be smaller than full pack ({})",
            pack_delta.len(), pack_full.len()
        );

        // Parse: the new blob should be an unresolved REF_DELTA
        let result = parse(&pack_delta).unwrap();
        assert_eq!(result.deltas.len(), 1, "Should have 1 REF_DELTA");
        assert_eq!(result.deltas[0].base_sha, base_obj.sha1());

        // Resolve the delta manually and verify content
        let resolved = apply_delta(&base_data, &result.deltas[0].delta_data).unwrap();
        assert_eq!(resolved, new_data);
    }
}
