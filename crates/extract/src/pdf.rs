//! PDF text through pdfium (loaded by absolute path only).
//!
//! The page count is never trusted (M0-C: a cyclic page tree reports `/Count 1000000`): at most
//! [`MAX_PAGES`] pages are visited, a run of [`MAX_FAILED_PAGES`] pages that fail to load ends
//! the document, and each page contributes at most [`MAX_PAGE_TEXT`] bytes. Memory and time are
//! bounded outside the parser by the Job Object (1 GiB) and the supervisor's timer.

use std::path::Path;

use pdfium_render::prelude::Pdfium;
use pdfium_render::prelude::{PdfiumError, PdfiumInternalError};
use sr_common::protocol::extract::WorkerFailure;

use crate::Failure;
use crate::sink::TextSink;

pub const MAX_PAGES: i32 = 10_000;
pub const MAX_FAILED_PAGES: u32 = 32;
pub const MAX_PAGE_TEXT: usize = 4 * 1024 * 1024;
/// Page separator in the output (form feed, as pdftotext does).
pub const PAGE_BREAK: char = '\u{c}';

/// A bound pdfium library.
pub struct PdfEngine {
    pdfium: Pdfium,
}

impl std::fmt::Debug for PdfEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PdfEngine")
    }
}

/// PDF extraction result.
#[derive(Debug)]
pub struct PdfText {
    pub text: String,
    pub pages: u32,
    pub truncated: bool,
}

impl PdfEngine {
    /// Binds pdfium from an **absolute** path (never a search-path lookup).
    pub fn load(dll: &Path) -> Result<Self, String> {
        if !dll.is_absolute() {
            return Err("pdfium path must be absolute".into());
        }
        let bindings = Pdfium::bind_to_library(dll).map_err(|e| format!("{e:?}"))?;
        Ok(Self {
            pdfium: Pdfium::new(bindings),
        })
    }

    pub fn extract(&self, bytes: &[u8], max_text: usize) -> Result<PdfText, Failure> {
        let doc = self
            .pdfium
            .load_pdf_from_byte_slice(bytes, None)
            .map_err(|e| match e {
                PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError) => {
                    Failure::new(WorkerFailure::Encrypted, "password-protected PDF")
                }
                PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::SecurityError) => {
                    Failure::new(WorkerFailure::Encrypted, "unsupported PDF security handler")
                }
                other => Failure::new(WorkerFailure::Malformed, format!("pdf: {other:?}")),
            })?;
        let pages = doc.pages();
        let declared = pages.len();
        let mut limited = declared > MAX_PAGES;
        let mut sink = TextSink::new(max_text, bytes.len());
        let (mut done, mut failed_run) = (0u32, 0u32);
        for i in 0..declared.clamp(0, MAX_PAGES) {
            if sink.is_full() {
                break;
            }
            let page = match pages.get(i) {
                Ok(p) => p,
                Err(_) => {
                    failed_run += 1;
                    if failed_run >= MAX_FAILED_PAGES {
                        limited = true;
                        break;
                    }
                    continue;
                }
            };
            failed_run = 0;
            if done > 0 {
                sink.push(PAGE_BREAK);
            }
            if let Ok(t) = page.text() {
                let mut s = t.all();
                if crate::text::truncate_at(&mut s, MAX_PAGE_TEXT) {
                    limited = true;
                }
                sink.push_str(&s);
            }
            done += 1;
        }
        let (text, truncated) = sink.finish();
        Ok(PdfText {
            text,
            pages: done,
            truncated: truncated || limited,
        })
    }
}
