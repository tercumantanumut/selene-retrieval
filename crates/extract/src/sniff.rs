//! Content sniffing: what the bytes really are, whatever the extension says.
//!
//! Strong signatures win over the extension (a `.docx` that is a PDF is parsed as a PDF; a `.txt`
//! that is a zip is binary). Text-family kinds only switch to PDF when `%PDF-` is at offset 0, so
//! source code that merely mentions a PDF header near its start stays code.

use memchr::memmem;
use sr_common::protocol::extract::{DocKind, WorkerFailure};

use crate::Failure;

/// OLE compound file (legacy Office, and password-protected OOXML).
const CFB_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
/// PDF readers accept junk before the header within the first KiB.
const PDF_HEADER_WINDOW: usize = 1024;

pub fn is_pdf(b: &[u8], allow_offset: bool) -> bool {
    b.starts_with(b"%PDF-")
        || (allow_offset && memmem::find(&b[..b.len().min(PDF_HEADER_WINDOW)], b"%PDF-").is_some())
}

pub fn is_zip(b: &[u8]) -> bool {
    b.starts_with(b"PK\x03\x04") || b.starts_with(b"PK\x05\x06")
}

pub fn is_cfb(b: &[u8]) -> bool {
    b.starts_with(&CFB_MAGIC)
}

/// Decides which parser runs. OOXML zips are returned as the declared kind (or `Docx` for
/// `Auto`); [`crate::ooxml::extract`] re-detects the real type from the parts inside.
pub fn resolve(kind: DocKind, b: &[u8]) -> Result<DocKind, Failure> {
    match kind {
        DocKind::Pdf => {
            if b.is_empty() {
                Err(Failure::new(WorkerFailure::Malformed, "empty file"))
            } else if is_pdf(b, true) {
                Ok(DocKind::Pdf)
            } else if is_zip(b) {
                Ok(DocKind::Docx)
            } else {
                Err(Failure::new(WorkerFailure::Malformed, "no PDF header"))
            }
        }
        DocKind::Docx | DocKind::Xlsx | DocKind::Pptx => {
            if b.is_empty() {
                Err(Failure::new(WorkerFailure::Malformed, "empty file"))
            } else if is_zip(b) {
                Ok(kind)
            } else if is_cfb(b) {
                Err(Failure::new(
                    WorkerFailure::Encrypted,
                    "OLE compound file: password-protected or legacy Office format",
                ))
            } else if is_pdf(b, false) {
                Ok(DocKind::Pdf)
            } else {
                Err(Failure::new(WorkerFailure::Malformed, "not a zip package"))
            }
        }
        DocKind::Html | DocKind::Text | DocKind::Markdown | DocKind::Code => {
            if is_pdf(b, false) {
                Ok(DocKind::Pdf)
            } else if is_zip(b) || is_cfb(b) {
                Err(Failure::new(
                    WorkerFailure::Binary,
                    "archive or compound file",
                ))
            } else {
                Ok(kind)
            }
        }
        DocKind::Auto => {
            if is_pdf(b, true) {
                Ok(DocKind::Pdf)
            } else if is_zip(b) {
                Ok(DocKind::Docx)
            } else if is_cfb(b) {
                Err(Failure::new(
                    WorkerFailure::Unsupported,
                    "OLE compound file (legacy Office format)",
                ))
            } else {
                Ok(DocKind::Auto)
            }
        }
    }
}

/// Whether decoded text is an HTML document (only used for `Auto`).
pub fn looks_html(s: &str) -> bool {
    let mut t = s.trim_start_matches('\u{FEFF}').trim_start();
    // Skip up to 8 leading comments.
    for _ in 0..8 {
        if let Some(rest) = t.strip_prefix("<!--") {
            match rest.find("-->") {
                Some(end) => t = rest[end + 3..].trim_start(),
                None => return false,
            }
        } else {
            break;
        }
    }
    let head: String = t
        .chars()
        .take(1024)
        .collect::<String>()
        .to_ascii_lowercase();
    head.starts_with("<!doctype html")
        || head.starts_with("<html")
        || (head.starts_with('<')
            && (head.contains("<html") || head.contains("<head>") || head.contains("<body")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strong_signatures_override_the_extension() {
        assert_eq!(
            resolve(DocKind::Docx, b"%PDF-1.7 ...").unwrap(),
            DocKind::Pdf
        );
        assert_eq!(
            resolve(DocKind::Pdf, b"PK\x03\x04....").unwrap(),
            DocKind::Docx
        );
        assert_eq!(
            resolve(DocKind::Text, b"PK\x03\x04....").unwrap_err().code,
            WorkerFailure::Binary
        );
        assert_eq!(
            resolve(DocKind::Docx, &CFB_MAGIC).unwrap_err().code,
            WorkerFailure::Encrypted
        );
        assert_eq!(
            resolve(DocKind::Pdf, b"").unwrap_err().code,
            WorkerFailure::Malformed
        );
        assert_eq!(
            resolve(DocKind::Pdf, b"junk\n%PDF-1.4").unwrap(),
            DocKind::Pdf
        );
        // Code mentioning a PDF header after offset 0 stays code.
        assert_eq!(
            resolve(DocKind::Code, b"// writes %PDF-1.7").unwrap(),
            DocKind::Code
        );
    }

    #[test]
    fn html_sniff() {
        assert!(looks_html("\u{FEFF}  <!DOCTYPE html><p>x"));
        assert!(looks_html("<!-- c --><html>"));
        assert!(looks_html("<div>\n<body>"));
        assert!(!looks_html("plain text mentioning <html> later"));
        assert!(!looks_html("<!-- unterminated"));
    }
}
