//! Native-messaging framing: a little-endian u32 byte length, then that many bytes of
//! UTF-8 JSON. Chrome uses it on the bridge's stdio; we reuse it on the named pipe.

use std::io::{self, Read, Write};

/// Refuse absurd lengths instead of allocating them (a corrupt stream, not a real message).
pub const MAX_FRAME: u32 = 64 * 1024 * 1024;

pub fn read_frame(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len);
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("frame of {len} bytes exceeds limit")));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

pub fn write_frame(w: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len())
        .ok()
        .filter(|&l| l <= MAX_FRAME)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

pub fn read_json(r: &mut impl Read) -> io::Result<serde_json::Value> {
    let buf = read_frame(r)?;
    serde_json::from_slice(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn write_json(w: &mut impl Write, value: &serde_json::Value) -> io::Result<()> {
    write_frame(w, &serde_json::to_vec(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut buf = Vec::new();
        write_json(&mut buf, &serde_json::json!({"type": "ping", "n": 1})).unwrap();
        assert_eq!(&buf[..4], &(buf.len() as u32 - 4).to_le_bytes());
        let v = read_json(&mut buf.as_slice()).unwrap();
        assert_eq!(v["n"], 1);
    }

    #[test]
    fn rejects_oversized_lengths() {
        let mut bad = (MAX_FRAME + 1).to_le_bytes().to_vec();
        bad.extend_from_slice(b"{}");
        assert_eq!(read_frame(&mut bad.as_slice()).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn truncated_stream_is_an_error_not_a_hang() {
        let mut short = 10u32.to_le_bytes().to_vec();
        short.extend_from_slice(b"{}");
        assert_eq!(read_frame(&mut short.as_slice()).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }
}
