//! sr-extract parsers. File bytes in, text out; no I/O of any kind.
//!
//! The `sr-extract` binary (src/main.rs) runs these parsers inside a sandboxed worker process
//! (AppContainer with zero capabilities, Job Object with a 1 GiB cap; Amendment 3.11). They are
//! still written as if the sandbox did not exist: every parser is linear in its input or bounded by
//! an explicit limit, every failure is a typed [`Failure`] that maps to one `ErrorCode`, and no
//! parser recurses on attacker-controlled structure.
//!
//! | Kind | Parser | Limits |
//! |---|---|---|
//! | text / markdown / code | [`text::decode`]: BOM, BOM-less UTF-16, UTF-8, chardetng legacy guess | binary sniff (NUL window, control-char ratio), text cap |
//! | html | [`html::html_to_text`]: single-pass tokenizer, no DOM | linear time, so nesting depth costs nothing |
//! | pdf | [`pdf::PdfEngine`] (pdfium) | page count never trusted, page/failed-page caps, text cap |
//! | docx / xlsx / pptx | [`ooxml`] (zip + quick-xml) | entry count, per-entry and total inflated bytes, ratio, XML depth |

pub mod html;
pub mod ooxml;
pub mod pdf;
pub mod sink;
pub mod sniff;
pub mod text;

use sr_common::protocol::extract::{DocKind, WorkerFailure, clean_detail};

pub use pdf::PdfEngine;

/// A successful extraction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extracted {
    pub text: String,
    /// The kind actually parsed (after sniffing).
    pub detected: DocKind,
    /// Detected text encoding (text kinds and HTML).
    pub encoding: Option<&'static str>,
    /// Pages (PDF), slides (PPTX) or sheets (XLSX) extracted.
    pub pages: Option<u32>,
    /// The text cap or a page/entry limit cut the output.
    pub truncated: bool,
}

/// A typed parse failure. `detail` is diagnostics only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub code: WorkerFailure,
    pub detail: String,
}

impl Failure {
    pub fn new(code: WorkerFailure, detail: impl AsRef<str>) -> Self {
        Self {
            code,
            detail: clean_detail(detail.as_ref()),
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.detail)
    }
}

impl std::error::Error for Failure {}

/// Extracts text from `bytes` believed to be `kind`. `max_text` caps the output (bytes of UTF-8).
/// `pdf` is the pdfium engine if it loaded; PDFs fail with `EngineUnavailable` without it.
pub fn extract(
    kind: DocKind,
    bytes: &[u8],
    max_text: usize,
    pdf: Option<&PdfEngine>,
) -> Result<Extracted, Failure> {
    let resolved = sniff::resolve(kind, bytes)?;
    match resolved {
        DocKind::Pdf => {
            let engine = pdf.ok_or_else(|| {
                Failure::new(WorkerFailure::EngineUnavailable, "pdf engine not loaded")
            })?;
            let out = engine.extract(bytes, max_text)?;
            Ok(Extracted {
                text: out.text,
                detected: DocKind::Pdf,
                encoding: None,
                pages: Some(out.pages),
                truncated: out.truncated,
            })
        }
        DocKind::Docx | DocKind::Xlsx | DocKind::Pptx => {
            let out = ooxml::extract(resolved, bytes, max_text)?;
            Ok(Extracted {
                text: out.text,
                detected: resolved,
                encoding: None,
                pages: out.parts,
                truncated: out.truncated,
            })
        }
        DocKind::Html => {
            // Decode with a generous input window: markup shrinks, so the output cap governs.
            let decoded = text::decode(bytes, text::html_input_window(max_text))?;
            let mut sink = sink::TextSink::new(max_text, decoded.text.len());
            html::html_to_text(&decoded.text, &mut sink);
            let (text, truncated) = sink.finish();
            Ok(Extracted {
                text,
                detected: DocKind::Html,
                encoding: Some(decoded.encoding),
                pages: None,
                truncated: truncated || decoded.truncated,
            })
        }
        DocKind::Text | DocKind::Markdown | DocKind::Code | DocKind::Auto => {
            let decoded = text::decode(bytes, max_text)?;
            // Auto: content that is clearly an HTML document is rendered as HTML.
            if kind == DocKind::Auto && sniff::looks_html(&decoded.text) {
                let mut sink = sink::TextSink::new(max_text, decoded.text.len());
                html::html_to_text(&decoded.text, &mut sink);
                let (text, truncated) = sink.finish();
                return Ok(Extracted {
                    text,
                    detected: DocKind::Html,
                    encoding: Some(decoded.encoding),
                    pages: None,
                    truncated: truncated || decoded.truncated,
                });
            }
            Ok(Extracted {
                text: decoded.text,
                detected: if resolved == DocKind::Auto {
                    DocKind::Text
                } else {
                    resolved
                },
                encoding: Some(decoded.encoding),
                pages: None,
                truncated: decoded.truncated,
            })
        }
    }
}
