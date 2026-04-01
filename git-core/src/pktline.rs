/// Git pkt-line protocol encoding/decoding.
///
/// Each pkt-line is prefixed with a 4-hex-digit length (including the 4 bytes).
/// Special packets: 0000 = flush, 0001 = delimiter, 0002 = response-end.

/// Encode a single pkt-line.
pub fn encode(data: &[u8]) -> Vec<u8> {
    let len = data.len() + 4;
    let mut out = format!("{:04x}", len).into_bytes();
    out.extend_from_slice(data);
    out
}

/// Encode a flush packet (0000).
pub fn flush() -> Vec<u8> {
    b"0000".to_vec()
}

/// Read all pkt-lines from a byte buffer until a flush packet.
/// Returns a list of line payloads (without the length prefix).
pub fn read_until_flush(data: &[u8]) -> (Vec<Vec<u8>>, &[u8]) {
    let mut lines = Vec::new();
    let mut pos = 0;

    while pos + 4 <= data.len() {
        let len_str = std::str::from_utf8(&data[pos..pos + 4]).unwrap_or("0000");
        let len = usize::from_str_radix(len_str, 16).unwrap_or(0);

        if len == 0 {
            // Flush packet
            pos += 4;
            break;
        }

        if len < 4 || pos + len > data.len() {
            break;
        }

        let payload = data[pos + 4..pos + len].to_vec();
        lines.push(payload);
        pos += len;
    }

    (lines, &data[pos..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode() {
        let result = encode(b"hello\n");
        assert_eq!(result, b"000ahello\n");
    }

    #[test]
    fn test_flush() {
        assert_eq!(flush(), b"0000");
    }

    #[test]
    fn test_read_until_flush() {
        let mut data = Vec::new();
        data.extend_from_slice(&encode(b"line1\n"));
        data.extend_from_slice(&encode(b"line2\n"));
        data.extend_from_slice(&flush());
        data.extend_from_slice(b"remaining");

        let (lines, rest) = read_until_flush(&data);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], b"line1\n");
        assert_eq!(lines[1], b"line2\n");
        assert_eq!(rest, b"remaining");
    }
}
