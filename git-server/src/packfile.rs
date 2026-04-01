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
    let mut objects = Vec::new();
    let mut deltas = Vec::new();

    for _ in 0..num_objects {
        if pos >= data.len() {
            return Err("unexpected end of packfile".into());
        }

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

                deltas.push(UnresolvedDelta {
                    base_sha,
                    delta_data,
                });
            }
            6 => {
                // OFS_DELTA: variable-length negative offset + zlib compressed delta
                // Read the offset
                let mut byte = data[pos];
                pos += 1;
                let mut offset: u64 = (byte & 0x7f) as u64;
                while byte & 0x80 != 0 {
                    byte = data[pos];
                    pos += 1;
                    offset = ((offset + 1) << 7) | (byte & 0x7f) as u64;
                }
                // We can't easily resolve ofs_delta without tracking object positions
                // For now, decompress and store as unresolved (caller can handle)
                let mut decoder = ZlibDecoder::new(&data[pos..]);
                let mut _delta_data = Vec::with_capacity(size as usize);
                decoder
                    .read_to_end(&mut _delta_data)
                    .map_err(|e| format!("zlib decompression failed for ofs_delta: {}", e))?;
                pos += decoder.total_in() as usize;

                return Err(format!(
                    "ofs_delta objects not yet supported (offset={})",
                    offset
                ));
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

                objects.push(PackObject {
                    obj_type: obj_type.to_string(),
                    data: decompressed,
                });
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

/// Build a packfile from a list of objects.
pub fn build(objects: &[PackObject]) -> Vec<u8> {
    let mut out = Vec::new();

    // Header
    out.extend_from_slice(b"PACK");
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&(objects.len() as u32).to_be_bytes());

    // Objects
    for obj in objects {
        let type_bits = type_to_bits(&obj.obj_type);
        let size = obj.data.len() as u64;

        // Encode type + size (variable-length)
        let mut first_byte = (type_bits << 4) | (size & 0x0f) as u8;
        let mut remaining = size >> 4;

        if remaining > 0 {
            first_byte |= 0x80;
        }
        out.push(first_byte);

        while remaining > 0 {
            let mut byte = (remaining & 0x7f) as u8;
            remaining >>= 7;
            if remaining > 0 {
                byte |= 0x80;
            }
            out.push(byte);
        }

        // Compress data with zlib
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&obj.data).unwrap();
        let compressed = encoder.finish().unwrap();
        out.extend_from_slice(&compressed);
    }

    // Trailing SHA-1 checksum
    let mut hasher = Sha1::new();
    hasher.update(&out);
    let checksum = hasher.finalize();
    out.extend_from_slice(&checksum);

    out
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
        assert_eq!(result.objects[0].obj_type, "blob");
        assert_eq!(result.objects[0].data, b"hello world\n");
        assert_eq!(result.objects[1].obj_type, "blob");
        assert_eq!(result.objects[1].data, b"another file\n");
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
}
