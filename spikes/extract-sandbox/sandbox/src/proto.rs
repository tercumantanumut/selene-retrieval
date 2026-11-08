//! Shared wire protocol for the M0-C spike (included by both bins via #[path]).
//!
//! Framing: every message is `[u32 little-endian length][length bytes]`.
//! Request  = header frame (JSON `Request`) followed by payload frame (raw file bytes).
//! Response = one JSON frame (`Response`); the worker also sends one `Ready` frame at start-up.
//! Both sides check the length against a cap BEFORE allocating, because each side
//! treats the other as untrusted (the parent must assume a compromised worker).
#![allow(dead_code)]

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

pub const MAX_HEADER: u32 = 64 * 1024;
pub const MAX_PAYLOAD: u32 = 64 * 1024 * 1024; // 64 MiB input file cap
pub const MAX_RESPONSE: u32 = 40 * 1024 * 1024; // response JSON cap (text cap + escaping headroom)
pub const MAX_TEXT_BYTES: usize = 16 * 1024 * 1024; // extracted text is truncated here

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Request {
    pub id: u64,
    /// "pdf" | "docx" | "html" | "md" | "txt" | "probe:<name>"
    pub kind: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Ready {
    pub ready: bool,
    pub pid: u32,
    pub integrity: String,
    pub app_container: bool,
    pub pdfium_load_ms: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    /// Error code from the worker (stable, machine readable), e.g. "PdfInvalid", "ZipBomb".
    pub code: Option<String>,
    pub msg: Option<String>,
    pub text: Option<String>,
    pub pages: Option<u32>,
    pub chars: u64,
    pub truncated: bool,
    pub worker_ms: u64,
}

pub fn write_frame(w: &mut impl Write, data: &[u8]) -> io::Result<()> {
    let n = u32::try_from(data.len()).map_err(|_| io::Error::other("frame too large"))?;
    w.write_all(&n.to_le_bytes())?;
    w.write_all(data)?;
    w.flush()
}

/// Reads one frame; `Ok(None)` on clean EOF before the length prefix.
pub fn read_frame(r: &mut impl Read, cap: u32) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_le_bytes(len);
    if n > cap {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("frame {n} > cap {cap}")));
    }
    let mut buf = vec![0u8; n as usize];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}
