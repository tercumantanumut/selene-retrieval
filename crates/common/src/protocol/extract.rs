//! Extract-worker protocol: `sr-extract` (sandboxed parser process) ⇄ the supervisor in the
//! daemon (`sr-extract-client`). Owner: M1-B. docs/02-architecture.md §2.1, Amendments 3.11, 3.14.
//!
//! Transport: the worker's stdin/stdout (anonymous pipes). Every message is one **frame**:
//! `[u32 little-endian length][length bytes]`. Both sides check the length against a cap
//! **before** allocating: each side treats the other as untrusted (the supervisor must assume a
//! compromised worker, the worker must assume hostile file bytes).
//!
//! Conversation:
//! 1. Worker → supervisor, once at start: JSON [`WorkerReady`].
//! 2. Per job, supervisor → worker: JSON [`WorkerRequest`] frame, then one **raw** frame holding
//!    the file bytes. The worker never opens files (it has no file-system rights at all under
//!    AppContainer); the supervisor reads the file and streams the bytes.
//! 3. Per job, worker → supervisor: JSON [`WorkerResponse`] frame. If the result is
//!    [`WorkerResult::Ok`], exactly one **raw** frame follows with the UTF-8 text
//!    (`text_bytes` long). Text is never embedded in JSON (no escaping cost, no 2× copy; 3.14).
//! 4. The supervisor closes stdin to ask the worker to exit; it kills the worker's Job Object on
//!    timeout or on any protocol violation.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

use crate::error::ErrorCode;

/// Extract protocol version (checked in [`WorkerReady::proto`]; worker and daemon ship together,
/// so only an exact match is accepted).
pub const EXTRACT_PROTO: u16 = 1;

/// Cap for JSON frames (ready, request, response header).
pub const MAX_JSON_FRAME: u32 = 64 * 1024;
/// Cap for the file-bytes frame. Matches the largest `max_file_bytes` setting (200 MiB).
pub const MAX_PAYLOAD_BYTES: u64 = 200 * 1024 * 1024;
/// Extracted text is truncated (on a char boundary) at this size.
pub const MAX_TEXT_BYTES: u64 = 16 * 1024 * 1024;
/// Cap for the detail string of a failure (diagnostics only, never shown in the Basic UI).
pub const MAX_DETAIL_CHARS: usize = 512;

/// What the bytes are believed to be. The supervisor derives it from the extension; the worker
/// sniffs the content and overrides it for strong signatures (PDF, OOXML zip, binary).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocKind {
    /// Unknown: the worker decides from the content.
    Auto,
    /// Plain text (txt, log, csv ...): decoded verbatim so line numbers match the file.
    Text,
    /// Markdown: decoded verbatim (the chunker splits on markdown structure).
    Markdown,
    /// Source code: decoded verbatim.
    Code,
    Html,
    Pdf,
    Docx,
    Xlsx,
    Pptx,
}

impl DocKind {
    /// Kind for a file extension (case-insensitive, without the dot). `None` = not a supported
    /// type (the caller skips the file or passes [`DocKind::Auto`]).
    pub fn from_extension(ext: &str) -> Option<DocKind> {
        let e = ext.to_ascii_lowercase();
        Some(match e.as_str() {
            "pdf" => DocKind::Pdf,
            "docx" | "docm" | "dotx" => DocKind::Docx,
            "xlsx" | "xlsm" | "xltx" => DocKind::Xlsx,
            "pptx" | "pptm" | "potx" => DocKind::Pptx,
            "html" | "htm" | "xhtml" | "shtml" => DocKind::Html,
            "md" | "markdown" | "mdx" | "mdown" | "mkd" => DocKind::Markdown,
            "txt" | "text" | "log" | "csv" | "tsv" | "rst" | "adoc" | "org" | "tex" | "ini"
            | "cfg" | "conf" | "properties" => DocKind::Text,
            "rs" | "py" | "pyi" | "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "go" | "java"
            | "kt" | "kts" | "cs" | "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx"
            | "m" | "mm" | "swift" | "rb" | "php" | "scala" | "lua" | "r" | "dart" | "sh"
            | "bash" | "zsh" | "ps1" | "psm1" | "bat" | "cmd" | "sql" | "toml" | "yaml" | "yml"
            | "json" | "jsonc" | "xml" | "css" | "scss" | "less" | "vue" | "svelte" | "astro"
            | "zig" | "nim" | "ex" | "exs" | "erl" | "hs" | "ml" | "fs" | "fsx" | "clj"
            | "gradle" | "cmake" | "proto" | "graphql" | "tf" | "vb" | "pl" | "pm" => DocKind::Code,
            _ => return None,
        })
    }

    /// Text-family kinds are decoded verbatim (no markup removal), so lines match the file.
    pub const fn is_verbatim_text(self) -> bool {
        matches!(self, DocKind::Text | DocKind::Markdown | DocKind::Code)
    }
}

/// First frame the worker sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerReady {
    pub proto: u16,
    /// `sr-extract` crate version.
    pub version: String,
    pub pid: u32,
    /// Token integrity level as seen by the worker: `low`, `medium`, ... (AppContainer → `low`).
    pub integrity: String,
    pub app_container: bool,
    /// Whether pdfium loaded. PDF jobs fail with [`WorkerFailure::EngineUnavailable`] otherwise.
    pub pdf_engine: bool,
    /// Why pdfium did not load (diagnostics only).
    pub pdf_engine_error: Option<String>,
    pub startup_ms: u64,
}

/// Job header (followed by the raw payload frame).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerRequest {
    pub id: u64,
    pub kind: DocKind,
    /// Text cap for this job (clamped to [`MAX_TEXT_BYTES`] by the worker).
    pub max_text_bytes: u64,
}

/// Job result header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerResponse {
    pub id: u64,
    pub result: WorkerResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WorkerResult {
    /// Success: a raw UTF-8 frame of exactly `text_bytes` follows.
    Ok {
        text_bytes: u64,
        /// The kind actually parsed (after sniffing).
        detected: DocKind,
        /// Detected text encoding (`UTF-8`, `UTF-16LE`, `windows-1252`, ...), text kinds only.
        encoding: Option<String>,
        /// Pages (PDF), slides (PPTX) or sheets (XLSX) actually extracted.
        pages: Option<u32>,
        /// The text was cut at `max_text_bytes`, or a page/entry limit was hit.
        truncated: bool,
        parse_ms: u64,
    },
    Failed {
        code: WorkerFailure,
        /// Diagnostics only (≤ [`MAX_DETAIL_CHARS`]); never contains a path (the worker has none).
        detail: String,
    },
}

/// Why the worker could not extract a file. Every variant maps to one [`ErrorCode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerFailure {
    /// Content is binary (not text in any supported encoding).
    Binary,
    /// Not a supported format (e.g. a zip that is not OOXML).
    Unsupported,
    /// Damaged, or not what the extension claims (bad PDF, bad zip, bad XML).
    Malformed,
    /// Password-protected or encrypted.
    Encrypted,
    /// Zip-bomb guard: too many entries, compression ratio or inflated total over the limits.
    ZipBomb,
    /// Nesting deeper than the parser limit.
    TooDeep,
    /// Payload larger than the frame cap.
    TooLarge,
    /// pdfium is not available in this worker.
    EngineUnavailable,
    /// The parser panicked (caught; the worker stays up).
    ParserPanic,
    /// Worker bug or protocol misuse.
    Internal,
}

impl WorkerFailure {
    /// The one user-facing code for this failure.
    pub const fn error_code(self) -> ErrorCode {
        match self {
            WorkerFailure::Binary
            | WorkerFailure::Unsupported
            | WorkerFailure::Malformed
            | WorkerFailure::Encrypted
            | WorkerFailure::ZipBomb
            | WorkerFailure::TooDeep
            | WorkerFailure::ParserPanic => ErrorCode::FileUnreadable,
            WorkerFailure::TooLarge => ErrorCode::FileTooLarge,
            WorkerFailure::EngineUnavailable | WorkerFailure::Internal => ErrorCode::Internal,
        }
    }

    /// Whether the failure is a property of the file (record it in the per-file dead letter and
    /// don't retry until the file changes) rather than of this installation.
    pub const fn is_file_fault(self) -> bool {
        !matches!(
            self,
            WorkerFailure::EngineUnavailable | WorkerFailure::Internal
        )
    }
}

/// Writes one frame.
pub fn write_frame<W: Write + ?Sized>(w: &mut W, data: &[u8]) -> io::Result<()> {
    write_frame_len(w, data.len() as u64)?;
    w.write_all(data)
}

/// Writes only the length prefix (the caller then streams exactly `len` bytes).
pub fn write_frame_len<W: Write + ?Sized>(w: &mut W, len: u64) -> io::Result<()> {
    let n = u32::try_from(len)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    w.write_all(&n.to_le_bytes())
}

/// Reads a length prefix. `Ok(None)` on a clean EOF before the first byte. Fails with
/// `InvalidData` when the length exceeds `cap` (nothing is allocated).
pub fn read_frame_len<R: Read + ?Sized>(r: &mut R, cap: u64) -> io::Result<Option<u64>> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match r.read(&mut len[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let n = u64::from(u32::from_le_bytes(len));
    if n > cap {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {n} bytes exceeds the {cap}-byte cap"),
        ));
    }
    Ok(Some(n))
}

/// Reads one frame of at most `cap` bytes. `Ok(None)` on a clean EOF.
pub fn read_frame<R: Read + ?Sized>(r: &mut R, cap: u64) -> io::Result<Option<Vec<u8>>> {
    let Some(n) = read_frame_len(r, cap)? else {
        return Ok(None);
    };
    let n = usize::try_from(n).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
    let mut buf = Vec::new();
    buf.try_reserve_exact(n)
        .map_err(|_| io::Error::new(io::ErrorKind::OutOfMemory, "frame allocation refused"))?;
    buf.resize(n, 0);
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

/// Strips control characters from `detail` and caps it at [`MAX_DETAIL_CHARS`].
pub fn clean_detail(detail: &str) -> String {
    detail
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_DETAIL_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_caps_are_checked_before_allocating() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"hello").unwrap();
        write_frame(&mut buf, b"").unwrap();
        let mut r = &buf[..];
        assert_eq!(
            read_frame(&mut r, 16).unwrap().as_deref(),
            Some(&b"hello"[..])
        );
        assert_eq!(read_frame(&mut r, 16).unwrap().as_deref(), Some(&b""[..]));
        assert_eq!(read_frame(&mut r, 16).unwrap(), None, "clean EOF");

        // A hostile 4 GiB length prefix is refused without allocating.
        let evil = u32::MAX.to_le_bytes();
        let err = read_frame(&mut &evil[..], MAX_JSON_FRAME.into()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        // Truncated prefix / body.
        assert!(read_frame(&mut &[1u8, 0][..], 16).is_err());
        assert!(read_frame(&mut &[5u8, 0, 0, 0, b'a'][..], 16).is_err());
    }

    #[test]
    fn json_shapes_are_stable() {
        let r = WorkerResponse {
            id: 7,
            result: WorkerResult::Failed {
                code: WorkerFailure::ZipBomb,
                detail: "x".into(),
            },
        };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(
            s,
            r#"{"id":7,"result":{"status":"failed","code":"zip_bomb","detail":"x"}}"#
        );
        let back: WorkerResponse = serde_json::from_str(&s).unwrap();
        assert_eq!(back, r);
        let req = WorkerRequest {
            id: 1,
            kind: DocKind::Pdf,
            max_text_bytes: MAX_TEXT_BYTES,
        };
        let s = serde_json::to_string(&req).unwrap();
        assert_eq!(s, r#"{"id":1,"kind":"pdf","max_text_bytes":16777216}"#);
    }

    #[test]
    fn every_failure_maps_to_an_extract_code() {
        use WorkerFailure as F;
        for f in [
            F::Binary,
            F::Unsupported,
            F::Malformed,
            F::Encrypted,
            F::ZipBomb,
            F::TooDeep,
            F::TooLarge,
            F::EngineUnavailable,
            F::ParserPanic,
            F::Internal,
        ] {
            let c = f.error_code();
            assert!(matches!(
                c,
                ErrorCode::FileUnreadable | ErrorCode::FileTooLarge | ErrorCode::Internal
            ));
            assert_eq!(f.is_file_fault(), c != ErrorCode::Internal);
        }
    }

    #[test]
    fn extensions_map_case_insensitively() {
        assert_eq!(DocKind::from_extension("PDF"), Some(DocKind::Pdf));
        assert_eq!(DocKind::from_extension("Md"), Some(DocKind::Markdown));
        assert_eq!(DocKind::from_extension("rs"), Some(DocKind::Code));
        assert_eq!(DocKind::from_extension("xlsx"), Some(DocKind::Xlsx));
        assert_eq!(DocKind::from_extension("exe"), None);
        assert!(DocKind::Code.is_verbatim_text());
        assert!(!DocKind::Html.is_verbatim_text());
    }

    #[test]
    fn detail_is_cleaned_and_capped() {
        let d = clean_detail(&format!("a\nb\u{7}{}", "x".repeat(2000)));
        assert!(d.starts_with("a b "));
        assert_eq!(d.chars().count(), MAX_DETAIL_CHARS);
    }
}
