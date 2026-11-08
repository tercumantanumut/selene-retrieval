//! DOCX / XLSX / PPTX text (zip + quick-xml), with zip-bomb and XML limits.
//!
//! Zip guards, all checked before or while inflating, never trusting declared sizes alone:
//! * entry count from the end-of-central-directory record(s) (incl. zip64) **before** the zip
//!   crate parses the central directory, and again after;
//! * per entry: declared size and declared ratio, then a [`Capped`] reader that fails as soon as
//!   the *actual* inflated bytes exceed `min(64 MiB, max(1 MiB, compressed × 200))`;
//! * per archive: actual inflated total ≤ 256 MiB across every entry read.
//!
//! XML guards: element depth ≤ 256 (`TooDeep`), custom entities are never expanded (billion
//! laughs is inert), DTDs are ignored, at most 2 M shared strings and 2000 sheets/slides.

use std::borrow::Cow;
use std::cell::Cell;
use std::io::{BufReader, Cursor, Read};

use memchr::memmem;
use quick_xml::events::Event;
use sr_common::protocol::extract::{DocKind, WorkerFailure};
use zip::ZipArchive;
use zip::result::ZipError;

use crate::Failure;
use crate::sink::TextSink;

pub const MAX_ENTRIES: u64 = 10_000;
pub const MAX_ENTRY_INFLATED: u64 = 64 * 1024 * 1024;
pub const MAX_TOTAL_INFLATED: u64 = 256 * 1024 * 1024;
pub const MAX_RATIO: u64 = 200;
/// Entries below this inflated size are exempt from the ratio rule (tiny XML compresses well).
pub const RATIO_FREE_BYTES: u64 = 1024 * 1024;
pub const MAX_XML_DEPTH: usize = 256;
pub const MAX_SHARED_STRINGS: usize = 2_000_000;
pub const MAX_PARTS: usize = 2000;

type Zip<'a> = ZipArchive<Cursor<&'a [u8]>>;

/// OOXML extraction result.
#[derive(Debug)]
pub struct OoxmlText {
    pub text: String,
    /// The real document type (from the parts inside).
    pub kind: DocKind,
    /// Sheets or slides read (`None` for DOCX).
    pub parts: Option<u32>,
    pub truncated: bool,
}

/// Inflate accounting shared by every entry of one archive.
struct Meter {
    total_left: Cell<u64>,
    bomb: Cell<bool>,
}

/// Reader that fails (instead of silently stopping) once more than `left` bytes come out.
struct Capped<'m, R> {
    inner: R,
    left: u64,
    meter: &'m Meter,
}

impl<R: Read> Read for Capped<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.left.min(self.meter.total_left.get());
        if left == 0 {
            let mut one = [0u8; 1];
            return match self.inner.read(&mut one)? {
                0 => Ok(0),
                _ => {
                    self.meter.bomb.set(true);
                    Err(std::io::Error::other("inflated size over the limit"))
                }
            };
        }
        let n = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
        let got = self.inner.read(&mut buf[..n])?;
        self.left -= got as u64;
        self.meter
            .total_left
            .set(self.meter.total_left.get() - got as u64);
        Ok(got)
    }
}

fn zip_err(e: ZipError) -> Failure {
    match &e {
        ZipError::UnsupportedArchive(m) if *m == ZipError::PASSWORD_REQUIRED => {
            Failure::new(WorkerFailure::Encrypted, "encrypted zip entry")
        }
        ZipError::UnsupportedArchive(m) => {
            Failure::new(WorkerFailure::Unsupported, format!("zip: {m}"))
        }
        _ => Failure::new(WorkerFailure::Malformed, format!("zip: {e}")),
    }
}

/// Reads the entry counts declared by every end-of-central-directory candidate in the tail
/// (and the zip64 record behind it) and refuses archives over [`MAX_ENTRIES`].
fn precheck_entry_count(b: &[u8]) -> Result<(), Failure> {
    const EOCD: &[u8] = b"PK\x05\x06";
    const Z64_LOCATOR: &[u8] = b"PK\x06\x07";
    const Z64_EOCD: &[u8] = b"PK\x06\x06";
    let tail_start = b.len().saturating_sub(22 + 65_535 + 20);
    let tail = &b[tail_start..];
    let mut max_entries = 0u64;
    let rd16 = |p: usize| -> u64 {
        b.get(p..p + 2)
            .map_or(0, |s| u64::from(u16::from_le_bytes([s[0], s[1]])))
    };
    let rd64 = |p: usize| -> Option<u64> {
        let s = b.get(p..p + 8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(s);
        Some(u64::from_le_bytes(a))
    };
    for pos in memmem::rfind_iter(tail, EOCD).take(64) {
        let p = tail_start + pos;
        if p + 22 > b.len() {
            continue;
        }
        let total = rd16(p + 10).max(rd16(p + 8));
        max_entries = max_entries.max(total);
        if total == 0xFFFF
            && p >= 20
            && b[p - 20..].starts_with(Z64_LOCATOR)
            && let Some(off) = rd64(p - 20 + 8).and_then(|o| usize::try_from(o).ok())
            && b.get(off..).is_some_and(|s| s.starts_with(Z64_EOCD))
        {
            max_entries = max_entries.max(rd64(off + 32).unwrap_or(u64::MAX));
        }
    }
    if max_entries > MAX_ENTRIES {
        return Err(Failure::new(
            WorkerFailure::ZipBomb,
            format!("{max_entries} entries declared (limit {MAX_ENTRIES})"),
        ));
    }
    Ok(())
}

/// Extracts the text of an OOXML package. `declared` is a hint; the real type comes from the
/// main part present in the package.
pub fn extract(declared: DocKind, b: &[u8], max_text: usize) -> Result<OoxmlText, Failure> {
    precheck_entry_count(b)?;
    let mut z = ZipArchive::new(Cursor::new(b)).map_err(zip_err)?;
    if z.len() as u64 > MAX_ENTRIES {
        return Err(Failure::new(
            WorkerFailure::ZipBomb,
            format!("{} entries", z.len()),
        ));
    }
    let kind = detect(&z, declared)?;
    let meter = Meter {
        total_left: Cell::new(MAX_TOTAL_INFLATED),
        bomb: Cell::new(false),
    };
    let mut sink = TextSink::new(max_text, b.len().saturating_mul(4));
    let mut limited = false;
    let parts = match kind {
        DocKind::Docx => {
            docx(&mut z, &meter, &mut sink)?;
            None
        }
        DocKind::Xlsx => Some(xlsx(&mut z, &meter, &mut sink, &mut limited)?),
        _ => Some(pptx(&mut z, &meter, &mut sink, &mut limited)?),
    };
    let (text, truncated) = sink.finish();
    Ok(OoxmlText {
        text,
        kind,
        parts,
        truncated: truncated || limited,
    })
}

fn detect(z: &Zip<'_>, declared: DocKind) -> Result<DocKind, Failure> {
    let has = |n: &str| z.index_for_name(n).is_some();
    let main = |k: DocKind| match k {
        DocKind::Docx => has("word/document.xml"),
        DocKind::Xlsx => has("xl/workbook.xml"),
        DocKind::Pptx => has("ppt/presentation.xml"),
        _ => false,
    };
    if main(declared) {
        return Ok(declared);
    }
    for k in [DocKind::Docx, DocKind::Xlsx, DocKind::Pptx] {
        if main(k) {
            return Ok(k);
        }
    }
    Err(Failure::new(
        WorkerFailure::Unsupported,
        "zip package without a Word, Excel or PowerPoint document",
    ))
}

/// Parses the XML entry `name` (if present) and feeds each event to `visit` until it returns
/// `false`. Returns `Ok(false)` when the entry does not exist.
fn read_part(
    z: &mut Zip<'_>,
    name: &str,
    meter: &Meter,
    mut visit: impl FnMut(&Event<'_>) -> bool,
) -> Result<bool, Failure> {
    let Some(idx) = z.index_for_name(name) else {
        return Ok(false);
    };
    let f = z.by_index(idx).map_err(zip_err)?;
    if f.encrypted() {
        return Err(Failure::new(
            WorkerFailure::Encrypted,
            "encrypted zip entry",
        ));
    }
    let declared = f.size();
    let comp = f.compressed_size().max(1);
    let ratio_cap = RATIO_FREE_BYTES.max(comp.saturating_mul(MAX_RATIO));
    if declared > MAX_ENTRY_INFLATED || declared > ratio_cap {
        return Err(Failure::new(
            WorkerFailure::ZipBomb,
            format!("entry declares {declared} B from {comp} B"),
        ));
    }
    let capped = Capped {
        inner: f,
        left: MAX_ENTRY_INFLATED.min(ratio_cap),
        meter,
    };
    let mut rd = quick_xml::Reader::from_reader(BufReader::with_capacity(64 * 1024, capped));
    {
        let c = rd.config_mut();
        c.trim_text_start = false;
        c.trim_text_end = false;
        c.expand_empty_elements = false;
        c.check_end_names = true;
    }
    let mut buf = Vec::new();
    let mut depth = 0usize;
    loop {
        match rd.read_event_into(&mut buf) {
            Ok(Event::Eof) => return Ok(true),
            Ok(ev) => {
                match &ev {
                    Event::Start(_) => {
                        depth += 1;
                        if depth > MAX_XML_DEPTH {
                            return Err(Failure::new(
                                WorkerFailure::TooDeep,
                                format!("XML nesting deeper than {MAX_XML_DEPTH}"),
                            ));
                        }
                    }
                    Event::End(_) => depth = depth.saturating_sub(1),
                    _ => {}
                }
                if !visit(&ev) {
                    return Ok(true);
                }
            }
            Err(e) => {
                return Err(if meter.bomb.get() {
                    Failure::new(WorkerFailure::ZipBomb, "inflated size over the limit")
                } else {
                    Failure::new(WorkerFailure::Malformed, format!("xml: {e}"))
                });
            }
        }
        buf.clear();
    }
}

/// Text of a text or entity-reference event (custom entities are never expanded).
fn event_text<'a>(ev: &'a Event<'_>) -> Option<Cow<'a, str>> {
    match ev {
        Event::Text(t) => Some(t.xml10_content()),
        Event::GeneralRef(g) => {
            if let Ok(Some(c)) = g.resolve_char_ref() {
                Some(Cow::Owned(c.to_string()))
            } else {
                quick_xml::escape::resolve_predefined_entity(g).map(Cow::Borrowed)
            }
        }
        _ => None,
    }
}

fn local<'a>(ev: &'a Event<'_>) -> Option<&'a [u8]> {
    match ev {
        Event::Start(e) | Event::Empty(e) => Some(e.local_name().into_inner().as_bytes()),
        Event::End(e) => Some(e.local_name().into_inner().as_bytes()),
        _ => None,
    }
}

/// Value of the attribute whose local name is `key` (entities in it unescaped).
fn attr(ev: &Event<'_>, key: &[u8]) -> Option<String> {
    let (Event::Start(e) | Event::Empty(e)) = ev else {
        return None;
    };
    for a in e.attributes().with_checks(false).flatten() {
        if a.key.local_name().as_ref().as_bytes() == key {
            let raw = a.value.into_owned();
            return Some(match quick_xml::escape::unescape(&raw) {
                Ok(c) => c.into_owned(),
                Err(_) => raw,
            });
        }
    }
    None
}

// ------------------------------------------------------------------ DOCX

fn docx(z: &mut Zip<'_>, meter: &Meter, sink: &mut TextSink) -> Result<(), Failure> {
    for (i, part) in [
        "word/document.xml",
        "word/footnotes.xml",
        "word/endnotes.xml",
    ]
    .into_iter()
    .enumerate()
    {
        if sink.is_full() {
            break;
        }
        if i > 0 && z.index_for_name(part).is_some() {
            sink.end_line();
        }
        let mut in_t = false;
        read_part(z, part, meter, |ev| {
            match ev {
                Event::Start(_) => {
                    if local(ev) == Some(b"t") {
                        in_t = true;
                    }
                }
                Event::Empty(_) => match local(ev) {
                    Some(b"tab") => {
                        sink.push('\t');
                    }
                    Some(b"br" | b"cr" | b"p") => {
                        sink.push('\n');
                    }
                    _ => {}
                },
                Event::End(_) => match local(ev) {
                    Some(b"t") => in_t = false,
                    Some(b"p" | b"tr") => {
                        sink.push('\n');
                    }
                    Some(b"tc") => {
                        sink.push('\t');
                    }
                    _ => {}
                },
                _ if in_t => {
                    if let Some(t) = event_text(ev) {
                        sink.push_str(&t);
                    }
                }
                _ => {}
            }
            !sink.is_full()
        })?;
    }
    Ok(())
}

// ------------------------------------------------------------------ relationships

/// Resolves a relationship target relative to the part directory `base` (`xl`, `ppt`).
fn resolve_target(base: &str, target: &str) -> Option<String> {
    let joined = match target.strip_prefix('/') {
        Some(abs) => abs.to_string(),
        None => format!("{base}/{target}"),
    };
    let mut out: Vec<&str> = Vec::new();
    for seg in joined.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop()?;
            }
            s => out.push(s),
        }
    }
    Some(out.join("/"))
}

/// `Id → Target` of internal relationships in a `.rels` part.
fn rels(z: &mut Zip<'_>, name: &str, meter: &Meter) -> Result<Vec<(String, String)>, Failure> {
    let mut out = Vec::new();
    read_part(z, name, meter, |ev| {
        if matches!(local(ev), Some(b"Relationship"))
            && !matches!(ev, Event::End(_))
            && attr(ev, b"TargetMode").as_deref() != Some("External")
            && let (Some(id), Some(t)) = (attr(ev, b"Id"), attr(ev, b"Target"))
        {
            out.push((id, t));
        }
        out.len() < 4 * MAX_PARTS
    })?;
    Ok(out)
}

/// Ordered part names: from the main part's `id` list through its relationships, falling back
/// to `prefix<N>.xml` entries sorted by N.
fn ordered_parts(
    z: &mut Zip<'_>,
    ids: &[(String, String)],
    rels: &[(String, String)],
    base: &str,
    prefix: &str,
) -> Vec<(String, String)> {
    let mut parts: Vec<(String, String)> = ids
        .iter()
        .filter_map(|(label, rid)| {
            let (_, target) = rels.iter().find(|(id, _)| id == rid)?;
            let name = resolve_target(base, target)?;
            z.index_for_name(&name).map(|_| (label.clone(), name))
        })
        .take(MAX_PARTS)
        .collect();
    if parts.is_empty() {
        let mut numbered: Vec<(u64, String)> = z
            .file_names()
            .filter_map(|n| n.ok())
            .filter_map(|n| {
                let num = n.strip_prefix(prefix)?.strip_suffix(".xml")?.parse().ok()?;
                Some((num, n.into_owned()))
            })
            .collect();
        numbered.sort();
        parts = numbered
            .into_iter()
            .take(MAX_PARTS)
            .map(|(n, name)| (n.to_string(), name))
            .collect();
    }
    parts
}

// ------------------------------------------------------------------ XLSX

fn xlsx(
    z: &mut Zip<'_>,
    meter: &Meter,
    sink: &mut TextSink,
    limited: &mut bool,
) -> Result<u32, Failure> {
    // Shared strings.
    let mut shared: Vec<String> = Vec::new();
    {
        let (mut in_si, mut in_t, mut in_rph) = (false, false, false);
        let mut cur = String::new();
        read_part(z, "xl/sharedStrings.xml", meter, |ev| {
            match (ev, local(ev)) {
                (Event::Start(_), Some(b"si")) => {
                    in_si = true;
                    cur.clear();
                }
                (Event::Empty(_), Some(b"si")) => shared.push(String::new()),
                (Event::End(_), Some(b"si")) => {
                    in_si = false;
                    shared.push(std::mem::take(&mut cur));
                }
                (Event::Start(_), Some(b"rPh")) => in_rph = true,
                (Event::End(_), Some(b"rPh")) => in_rph = false,
                (Event::Start(_), Some(b"t")) => in_t = true,
                (Event::End(_), Some(b"t")) => in_t = false,
                _ if in_si && in_t && !in_rph => {
                    if let Some(t) = event_text(ev) {
                        cur.push_str(&t);
                    }
                }
                _ => {}
            }
            if shared.len() >= MAX_SHARED_STRINGS {
                *limited = true;
                return false;
            }
            true
        })?;
    }

    // Sheet order and names.
    let mut sheets: Vec<(String, String)> = Vec::new();
    read_part(z, "xl/workbook.xml", meter, |ev| {
        if matches!(local(ev), Some(b"sheet"))
            && !matches!(ev, Event::End(_))
            && let (Some(name), Some(rid)) = (attr(ev, b"name"), rel_id(ev))
        {
            sheets.push((name, rid));
        }
        sheets.len() < MAX_PARTS
    })?;
    let rel = rels(z, "xl/_rels/workbook.xml.rels", meter)?;
    let parts = ordered_parts(z, &sheets, &rel, "xl", "xl/worksheets/sheet");

    let mut done = 0u32;
    for (label, name) in parts {
        if sink.is_full() {
            *limited = true;
            break;
        }
        sink.end_line();
        sink.push_str("Sheet: ");
        sink.push_str(&label);
        sink.push('\n');
        let (mut ctype, mut val, mut in_v, mut in_is_t) =
            (String::new(), String::new(), false, false);
        let mut cells_in_row = 0u32;
        read_part(z, &name, meter, |ev| {
            match (ev, local(ev)) {
                (Event::Start(_), Some(b"c")) => {
                    ctype = attr(ev, b"t").unwrap_or_default();
                    val.clear();
                }
                (Event::Start(_), Some(b"v")) => in_v = true,
                (Event::End(_), Some(b"v")) => in_v = false,
                (Event::Start(_), Some(b"t")) if ctype == "inlineStr" => in_is_t = true,
                (Event::End(_), Some(b"t")) => in_is_t = false,
                (Event::End(_), Some(b"c")) => {
                    let shown: Cow<'_, str> = match ctype.as_str() {
                        "s" => val
                            .trim()
                            .parse::<usize>()
                            .ok()
                            .and_then(|i| shared.get(i))
                            .map_or(Cow::Borrowed(""), |s| Cow::Borrowed(s.as_str())),
                        "b" => Cow::Borrowed(if val.trim() == "1" { "TRUE" } else { "FALSE" }),
                        _ => Cow::Borrowed(val.as_str()),
                    };
                    if !shown.is_empty() {
                        if cells_in_row > 0 {
                            sink.push('\t');
                        }
                        sink.push_str(&shown);
                        cells_in_row += 1;
                    }
                }
                (Event::End(_), Some(b"row")) => {
                    if cells_in_row > 0 {
                        sink.push('\n');
                    }
                    cells_in_row = 0;
                }
                _ if in_v || in_is_t => {
                    if let Some(t) = event_text(ev)
                        && val.len() < 1 << 20
                    {
                        val.push_str(&t);
                    }
                }
                _ => {}
            }
            !sink.is_full()
        })?;
        done += 1;
    }
    Ok(done)
}

// ------------------------------------------------------------------ PPTX

fn pptx(
    z: &mut Zip<'_>,
    meter: &Meter,
    sink: &mut TextSink,
    limited: &mut bool,
) -> Result<u32, Failure> {
    let mut ids: Vec<(String, String)> = Vec::new();
    read_part(z, "ppt/presentation.xml", meter, |ev| {
        if matches!(local(ev), Some(b"sldId"))
            && !matches!(ev, Event::End(_))
            && let Some(rid) = rel_id(ev)
        {
            ids.push(((ids.len() + 1).to_string(), rid));
        }
        ids.len() < MAX_PARTS
    })?;
    let rel = rels(z, "ppt/_rels/presentation.xml.rels", meter)?;
    let parts = ordered_parts(z, &ids, &rel, "ppt", "ppt/slides/slide");

    let mut done = 0u32;
    for (n, (_, name)) in parts.into_iter().enumerate() {
        if sink.is_full() {
            *limited = true;
            break;
        }
        sink.end_line();
        sink.push_str(&format!("Slide {}\n", n + 1));
        let mut in_t = false;
        read_part(z, &name, meter, |ev| {
            match (ev, local(ev)) {
                (Event::Start(_), Some(b"t")) => in_t = true,
                (Event::End(_), Some(b"t")) => in_t = false,
                (Event::End(_), Some(b"p")) | (Event::Empty(_), Some(b"br")) => {
                    if !matches!(sink.last_char(), Some('\n') | None) {
                        sink.push('\n');
                    }
                }
                _ if in_t => {
                    if let Some(t) = event_text(ev) {
                        sink.push_str(&t);
                    }
                }
                _ => {}
            }
            !sink.is_full()
        })?;
        done += 1;
    }
    Ok(done)
}

/// The `r:id` attribute (namespace-prefixed `id`) of a `sldId`.
fn rel_id(ev: &Event<'_>) -> Option<String> {
    let (Event::Start(e) | Event::Empty(e)) = ev else {
        return None;
    };
    e.attributes()
        .with_checks(false)
        .flatten()
        .find(|a| a.key.local_name().as_ref() == "id" && a.key.prefix().is_some())
        .map(|a| a.value.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_resolve_and_never_escape_the_root() {
        assert_eq!(
            resolve_target("xl", "worksheets/sheet1.xml").as_deref(),
            Some("xl/worksheets/sheet1.xml")
        );
        assert_eq!(
            resolve_target("xl", "/xl/worksheets/s.xml").as_deref(),
            Some("xl/worksheets/s.xml")
        );
        assert_eq!(
            resolve_target("ppt", "./slides/../slides/slide2.xml").as_deref(),
            Some("ppt/slides/slide2.xml")
        );
        assert_eq!(resolve_target("xl", "../../../etc"), None);
    }

    #[test]
    fn eocd_precheck_catches_huge_declared_counts() {
        // Minimal EOCD claiming 60 000 entries.
        let mut b = vec![0u8; 100];
        b.extend_from_slice(b"PK\x05\x06");
        b.extend_from_slice(&[0, 0, 0, 0]);
        b.extend_from_slice(&60_000u16.to_le_bytes());
        b.extend_from_slice(&60_000u16.to_le_bytes());
        b.extend_from_slice(&[0u8; 10]);
        assert_eq!(
            precheck_entry_count(&b).unwrap_err().code,
            WorkerFailure::ZipBomb
        );
        assert!(precheck_entry_count(b"not a zip").is_ok());
    }
}
