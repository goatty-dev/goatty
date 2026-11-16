//! Framing: `u32` little-endian length prefix + postcard body.

use std::io::{self, Read, Write};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Largest frame a peer accepts; protects against garbage length prefixes.
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

/// A complete, length-prefixed frame, ready to write. Shared between
/// subscribers so a broadcast costs one encode.
pub type Frame = Arc<[u8]>;

pub fn encode<M: Serialize>(msg: &M) -> io::Result<Frame> {
    let mut buf = vec![0u8; 4];
    let mut buf = postcard::to_extend(msg, std::mem::take(&mut buf)).map_err(io::Error::other)?;
    let len = (buf.len() - 4) as u32;
    buf[..4].copy_from_slice(&len.to_le_bytes());
    Ok(buf.into())
}

/// zstd level for snapshots and history: fast enough that compressing a
/// 2000-line snapshot costs about a millisecond (see `remote-architecture.md` §7).
pub const ZSTD_LEVEL: i32 = 3;

/// Payloads smaller than this go uncompressed (they fit a few packets anyway).
const COMPRESS_MIN: usize = 1024;

/// Encode `msg`, compressed into a [`ServerMsg::Zstd`](crate::ServerMsg::Zstd)
/// when `compress` is set, it's large enough and that makes it smaller.
pub fn encode_server(msg: &crate::ServerMsg, compress: bool) -> io::Result<Frame> {
    if compress && msg.payload_len() >= COMPRESS_MIN {
        let plain = postcard::to_allocvec(msg).map_err(io::Error::other)?;
        let packed = zstd::bulk::compress(&plain, ZSTD_LEVEL)?;
        if packed.len() < plain.len() {
            return encode(&crate::ServerMsg::Zstd { data: packed });
        }
    }
    encode(msg)
}

impl crate::ServerMsg {
    /// The message inside a [`ServerMsg::Zstd`](crate::ServerMsg::Zstd)
    /// (anything else is returned as it is).
    pub fn unpack(self) -> io::Result<Self> {
        match self {
            crate::ServerMsg::Zstd { data } => {
                let plain = zstd::bulk::decompress(&data, MAX_FRAME)?;
                match postcard::from_bytes(&plain) {
                    Ok(crate::ServerMsg::Zstd { .. }) => Err(io::Error::new(io::ErrorKind::InvalidData, "nested zstd frame")),
                    Ok(m) => Ok(m),
                    Err(e) => Err(io::Error::new(io::ErrorKind::InvalidData, e)),
                }
            }
            m => Ok(m),
        }
    }
}

/// Encode and write one message (no flush; callers decide batching).
pub fn write_msg<W: Write, M: Serialize>(w: &mut W, msg: &M) -> io::Result<()> {
    w.write_all(&encode(msg)?)
}

pub fn read_msg<R: Read, M: for<'de> Deserialize<'de>>(r: &mut R) -> io::Result<M> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    postcard::from_bytes(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClientMsg, ServerMsg};

    #[test]
    fn roundtrip() {
        let mut buf = Vec::new();
        write_msg(&mut buf, &ClientMsg::Input { session: 7, data: b"ls\r".to_vec() }).unwrap();
        write_msg(&mut buf, &ServerMsg::Exited { session: 3, code: Some(0) }).unwrap();
        let mut r = &buf[..];
        assert!(matches!(read_msg::<_, ClientMsg>(&mut r).unwrap(), ClientMsg::Input { session: 7, data } if data == b"ls\r"));
        assert!(matches!(read_msg::<_, ServerMsg>(&mut r).unwrap(), ServerMsg::Exited { session: 3, code: Some(0) }));
        assert!(r.is_empty());
    }

    #[test]
    fn snapshots_compress_when_asked() {
        let snapshot: Vec<u8> = (0..2000).flat_map(|i| format!("\x1b[32mline {i}\x1b[0m some output\r\n").into_bytes()).collect();
        let msg = ServerMsg::Attached { req: 3, session: 1, snapshot: snapshot.clone() };
        let plain = encode_server(&msg, false).unwrap();
        let packed = encode_server(&msg, true).unwrap();
        assert!(packed.len() * 4 < plain.len(), "{} vs {}", packed.len(), plain.len());
        let read = read_msg::<_, ServerMsg>(&mut &packed[..]).unwrap();
        assert!(matches!(read, ServerMsg::Zstd { .. }));
        assert!(matches!(read.unpack().unwrap(), ServerMsg::Attached { req: 3, session: 1, snapshot: s } if s == snapshot));
        // Small or incompressible payloads go as they are.
        let small = ServerMsg::Resync { session: 1, skipped: 0, snapshot: b"x".to_vec() };
        assert!(matches!(read_msg::<_, ServerMsg>(&mut &encode_server(&small, true).unwrap()[..]).unwrap(), ServerMsg::Resync { .. }));
    }

    #[test]
    fn bytes_are_compact() {
        // serde_bytes: a 64 KiB chunk costs only a few bytes of header.
        let f = encode(&ServerMsg::Output { session: 1, data: vec![b'x'; 65536] }).unwrap();
        assert!(f.len() < 65536 + 12, "{}", f.len());
    }
}
