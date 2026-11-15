//! Shared helpers for the sr-extract integration tests: staging the worker next to pdfium (the
//! install layout), per-test AppContainer profiles, and the hostile corpus (a Rust port of
//! spikes/extract-sandbox/gen_fixtures.py plus OOXML additions), generated once and cached.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sr_common::protocol::extract::DocKind;
use sr_extract_client::{ExtractConfig, SandboxMode, delete_app_container_profile};

/// The pdfium library's file name on this platform (`pdfium.dll`, `libpdfium.dylib`, ...).
pub fn pdfium_name() -> String {
    format!(
        "{}pdfium{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    )
}

/// pdfium.dll: `SR_PDFIUM_DLL`, else `<workspace root>/vendor-bin/pdfium-7881/pdfium.dll`, where
/// `cargo xtask fetch-pdfium` puts the pinned chromium/7881 build (`libpdfium.dylib` on macOS).
pub fn pdfium_dll() -> PathBuf {
    if let Some(p) = std::env::var_os("SR_PDFIUM_DLL") {
        return PathBuf::from(p);
    }
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../vendor-bin/pdfium-7881")
        .join(pdfium_name());
    let p = std::fs::canonicalize(&p).unwrap_or_else(|e| {
        panic!(
            "{} not found at {} ({e}); run `cargo xtask fetch-pdfium` or set SR_PDFIUM_DLL \
             to a pdfium chromium/7881 build",
            pdfium_name(),
            p.display()
        )
    });
    strip_verbatim(p)
}

fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => p,
    }
}

pub fn tmp_root() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
}

/// Copies the worker (`SR_EXTRACT_EXE` or the freshly built one) and pdfium into `<tmp>/stage/<name>/` (the install layout:
/// both files side by side) and returns `(exe, dll)`.
pub fn stage(name: &str) -> (PathBuf, PathBuf) {
    let dir = tmp_root().join("stage").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let exe = dir.join(format!("sr-extract{}", std::env::consts::EXE_SUFFIX));
    let dll = dir.join(pdfium_name());
    // SR_EXTRACT_EXE: run the suite against another build (e.g. the +crt-static packaging one).
    let src = std::env::var_os("SR_EXTRACT_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_sr-extract")));
    // macOS caches a binary's code signature per file: copying a new build over the old file in
    // place makes the kernel kill it at exec. A fresh file avoids that.
    #[cfg(unix)]
    let _ = std::fs::remove_file(&exe);
    std::fs::copy(src, &exe).unwrap();
    if !dll.exists() {
        std::fs::copy(pdfium_dll(), &dll).unwrap();
    }
    (exe, dll)
}

/// A per-test AppContainer profile, deleted before and after the test.
pub struct Profile(pub String);

impl Profile {
    pub fn new(test: &str) -> Profile {
        let name = format!("SeleneRetrieval.Extract.Test.{test}");
        assert!(name.len() <= 64, "profile name too long");
        let _ = delete_app_container_profile(&name);
        Profile(name)
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        let _ = delete_app_container_profile(&self.0);
    }
}

pub fn config(exe: &Path, dll: &Path, mode: SandboxMode, profile: &str) -> ExtractConfig {
    let mut c = ExtractConfig::new(exe, Some(dll.to_path_buf()));
    c.sandbox = mode;
    c.app_container_name = profile.into();
    c
}

pub fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

pub fn quick_timeouts(c: &mut ExtractConfig, base: Duration) {
    c.timeout_base = base;
    c.timeout_per_mib = Duration::ZERO;
    c.timeout_max = base;
}

// ------------------------------------------------------------------ generators

pub fn rnd(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 32) as u8
        })
        .collect()
}

pub fn zlib(data: &[u8], level: u32) -> Vec<u8> {
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(level));
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

/// objs: bodies of objects 1..n (object 1 is the catalog).
pub fn build_pdf(objs: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objs.len() + 1
        )
        .as_bytes(),
    );
    out
}

pub fn pdf_stream(content: &[u8], filter: Option<&str>) -> Vec<u8> {
    let mut v = format!("<< /Length {}", content.len()).into_bytes();
    if let Some(f) = filter {
        v.extend_from_slice(format!(" /Filter /{f}").as_bytes());
    }
    v.extend_from_slice(b" >>\nstream\n");
    v.extend_from_slice(content);
    v.extend_from_slice(b"\nendstream");
    v
}

pub fn sample_pdf(pages: usize) -> Vec<u8> {
    let font = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec();
    let mut objs: Vec<Vec<u8>> = vec![Vec::new(), Vec::new(), font];
    let mut kids = Vec::new();
    for p in 0..pages {
        let page_no = objs.len() + 1;
        kids.push(page_no);
        objs.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>", page_no + 1).into_bytes());
        let mut c = b"BT /F1 12 Tf 72 720 Td 14 TL\n".to_vec();
        for line in [
            format!("Selene retrieval spike page {}.", p + 1),
            "The quick brown fox jumps over the lazy dog.".to_string(),
            format!("Keyword: pdfium-ort-coexist-{}", p + 1),
        ] {
            c.extend_from_slice(format!("({line}) '\n").as_bytes());
        }
        c.extend_from_slice(b"ET");
        objs.push(pdf_stream(&zlib(&c, 6), Some("FlateDecode")));
    }
    objs[0] = b"<< /Type /Catalog /Pages 2 0 R >>".to_vec();
    let kids_s: Vec<String> = kids.iter().map(|k| format!("{k} 0 R")).collect();
    objs[1] = format!(
        "<< /Type /Pages /Kids [{}] /Count {} >>",
        kids_s.join(" "),
        kids.len()
    )
    .into_bytes();
    build_pdf(&objs)
}

fn page_tree_pdf(page: &str, extra: Vec<Vec<u8>>) -> Vec<u8> {
    let mut objs = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        page.as_bytes().to_vec(),
    ];
    objs.extend(extra);
    build_pdf(&objs)
}

const CT: &str = r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/></Types>"#;
const NS_W: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;

pub fn zip_of(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut z = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let o = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(true);
    for (n, d) in entries {
        z.start_file(*n, o).unwrap();
        z.write_all(d).unwrap();
    }
    z.finish().unwrap().into_inner()
}

pub fn docx(document_xml: Vec<u8>) -> Vec<u8> {
    zip_of(&[
        ("[Content_Types].xml", CT.as_bytes().to_vec()),
        ("word/document.xml", document_xml),
    ])
}

pub fn sample_docx() -> Vec<u8> {
    docx(format!(r#"<?xml version="1.0" encoding="UTF-8"?><w:document {NS_W}><w:body><w:p><w:r><w:t>Hello DOCX world.</w:t></w:r></w:p><w:p><w:r><w:t xml:space="preserve">Second </w:t></w:r><w:r><w:t>paragraph &amp; more.</w:t></w:r></w:p></w:body></w:document>"#).into_bytes())
}

pub fn sample_xlsx() -> Vec<u8> {
    let r = r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;
    zip_of(&[
        ("[Content_Types].xml", CT.as_bytes().to_vec()),
        ("xl/workbook.xml", format!(r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" {r}><sheets><sheet name="Q&amp;A" sheetId="1" r:id="rId7"/><sheet name="Second" sheetId="2" r:id="rId8"/></sheets></workbook>"#).into_bytes()),
        ("xl/_rels/workbook.xml.rels", br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId8" Target="worksheets/sheet2.xml" Type="x"/><Relationship Id="rId7" Target="/xl/worksheets/sheet1.xml" Type="x"/></Relationships>"#.to_vec()),
        ("xl/sharedStrings.xml", br#"<sst><si><t>Name</t></si><si><r><t>Alpha </t></r><r><t>Beta</t></r><rPh><t>IGNORED</t></rPh></si></sst>"#.to_vec()),
        ("xl/worksheets/sheet1.xml", br#"<worksheet><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1"><v>42.5</v></c></row><row r="2"><c r="A2" t="s"><v>1</v></c><c r="B2" t="b"><v>1</v></c><c r="C2" t="inlineStr"><is><t>inline</t></is></c><c r="D2"><f>SUM(B1)</f><v>42.5</v></c></row></sheetData></worksheet>"#.to_vec()),
        ("xl/worksheets/sheet2.xml", br#"<worksheet><sheetData><row r="1"><c r="A1" t="s"><v>999</v></c><c r="B1" t="str"><v>formula text</v></c></row></sheetData></worksheet>"#.to_vec()),
    ])
}

pub fn sample_pptx() -> Vec<u8> {
    let slide = |t: &str| {
        format!(r#"<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>{t}</a:t></a:r></a:p><a:p><a:r><a:t>second line</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#).into_bytes()
    };
    zip_of(&[
        ("[Content_Types].xml", CT.as_bytes().to_vec()),
        // Slide order is from the relationships, not the file numbers: slide2 comes first.
        ("ppt/presentation.xml", br#"<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:sldIdLst><p:sldId id="256" r:id="rId3"/><p:sldId id="257" r:id="rId2"/></p:sldIdLst></p:presentation>"#.to_vec()),
        ("ppt/_rels/presentation.xml.rels", br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId2" Target="slides/slide1.xml" Type="x"/><Relationship Id="rId3" Target="slides/slide2.xml" Type="x"/></Relationships>"#.to_vec()),
        ("ppt/slides/slide1.xml", slide("First file, shown second")),
        ("ppt/slides/slide2.xml", slide("Opening slide")),
    ])
}

/// Patches the declared uncompressed size of `name` in both the local header and the central
/// directory (a lying zip).
pub fn patch_declared_size(z: &mut [u8], name: &str, size: u32) {
    let nb = name.as_bytes();
    let mut i = 0;
    while i + 46 < z.len() {
        if z[i..].starts_with(b"PK\x03\x04") && z[i + 30..].starts_with(nb) {
            z[i + 22..i + 26].copy_from_slice(&size.to_le_bytes());
        }
        if z[i..].starts_with(b"PK\x01\x02") && z[i + 46..].starts_with(nb) {
            z[i + 24..i + 28].copy_from_slice(&size.to_le_bytes());
        }
        i += 1;
    }
}

/// One corpus entry: file name, kind passed to the pool, and what outcomes are acceptable.
#[derive(Debug, Clone)]
pub struct Case {
    pub name: &'static str,
    pub kind: DocKind,
    pub expect: Expect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    /// Ok and the text contains this.
    OkContains(&'static str),
    /// Ok, any text.
    Ok,
    /// This error code.
    Code(sr_common::ErrorCode),
    /// Ok or one of the "unreadable" outcomes (FileUnreadable / ExtractTimeout).
    Contained,
}

/// Writes the corpus into a cached directory and returns it with its expectations.
pub fn corpus() -> (PathBuf, Vec<Case>) {
    use sr_common::ErrorCode as C;
    let dir = tmp_root().join("corpus-v3");
    let done = dir.join(".complete");
    let cases = vec![
        Case {
            name: "sample.pdf",
            kind: DocKind::Pdf,
            expect: Expect::OkContains("Selene retrieval spike page 1"),
        },
        Case {
            name: "big-200p.pdf",
            kind: DocKind::Pdf,
            expect: Expect::OkContains("page 200"),
        },
        Case {
            name: "sample.docx",
            kind: DocKind::Docx,
            expect: Expect::OkContains("Hello DOCX world."),
        },
        Case {
            name: "sample.xlsx",
            kind: DocKind::Xlsx,
            expect: Expect::OkContains("Alpha Beta\tTRUE\tinline"),
        },
        Case {
            name: "sample.pptx",
            kind: DocKind::Pptx,
            expect: Expect::OkContains("Slide 1\nOpening slide"),
        },
        Case {
            name: "sample.html",
            kind: DocKind::Html,
            expect: Expect::OkContains("Hello HTML world & friends."),
        },
        Case {
            name: "sample.md",
            kind: DocKind::Markdown,
            expect: Expect::OkContains("# Title"),
        },
        Case {
            name: "sample-1252.txt",
            kind: DocKind::Text,
            expect: Expect::OkContains("Café crème"),
        },
        Case {
            name: "zero.pdf",
            kind: DocKind::Pdf,
            expect: Expect::Code(C::FileUnreadable),
        },
        Case {
            name: "zero.txt",
            kind: DocKind::Text,
            expect: Expect::Ok,
        },
        Case {
            name: "truncated.pdf",
            kind: DocKind::Pdf,
            expect: Expect::Contained,
        },
        Case {
            name: "binary.txt",
            kind: DocKind::Text,
            expect: Expect::Code(C::FileUnreadable),
        },
        Case {
            name: "binary-nonul.txt",
            kind: DocKind::Text,
            expect: Expect::Code(C::FileUnreadable),
        },
        Case {
            name: "binary-as.html",
            kind: DocKind::Html,
            expect: Expect::Code(C::FileUnreadable),
        },
        Case {
            name: "binary-as.pdf",
            kind: DocKind::Pdf,
            expect: Expect::Code(C::FileUnreadable),
        },
        Case {
            name: "binary-as.docx",
            kind: DocKind::Docx,
            expect: Expect::Code(C::FileUnreadable),
        },
        Case {
            name: "binary-as.xlsx",
            kind: DocKind::Xlsx,
            expect: Expect::Code(C::FileUnreadable),
        },
        Case {
            name: "binary-auto.bin",
            kind: DocKind::Auto,
            expect: Expect::Code(C::FileUnreadable),
        },
        Case {
            name: "zipbomb.docx",
            kind: DocKind::Docx,
            expect: Expect::Code(C::FileUnreadable),
        },
        Case {
            name: "zipbomb-lying.docx",
            kind: DocKind::Docx,
            expect: Expect::Contained,
        },
        Case {
            name: "zip-entries.xlsx",
            kind: DocKind::Xlsx,
            expect: Expect::Code(C::FileUnreadable),
        },
        Case {
            name: "nested-xml.docx",
            kind: DocKind::Docx,
            expect: Expect::Code(C::FileUnreadable),
        },
        Case {
            name: "deep-lowratio.docx",
            kind: DocKind::Docx,
            expect: Expect::Code(C::FileUnreadable),
        },
        Case {
            name: "billion-laughs.docx",
            kind: DocKind::Docx,
            expect: Expect::Ok,
        },
        Case {
            name: "sharedstrings-bomb.xlsx",
            kind: DocKind::Xlsx,
            expect: Expect::Contained,
        },
        Case {
            name: "not-ooxml.docx",
            kind: DocKind::Docx,
            expect: Expect::Code(C::FileUnreadable),
        },
        Case {
            name: "encrypted.docx",
            kind: DocKind::Docx,
            expect: Expect::Code(C::FileUnreadable),
        },
        Case {
            name: "nested.html",
            kind: DocKind::Html,
            expect: Expect::OkContains("deep"),
        },
        Case {
            name: "tags-unterminated.html",
            kind: DocKind::Html,
            expect: Expect::Ok,
        },
        Case {
            name: "nested.md",
            kind: DocKind::Markdown,
            expect: Expect::Ok,
        },
        Case {
            name: "long-line.txt",
            kind: DocKind::Text,
            expect: Expect::Ok,
        },
        Case {
            name: "invalid-utf8.txt",
            kind: DocKind::Text,
            expect: Expect::Ok,
        },
        Case {
            name: "utf16-odd.txt",
            kind: DocKind::Text,
            expect: Expect::OkContains("UTF-16 text"),
        },
        Case {
            name: "bidi-zw.txt",
            kind: DocKind::Text,
            expect: Expect::Ok,
        },
        Case {
            name: "zalgo.txt",
            kind: DocKind::Text,
            expect: Expect::Ok,
        },
        Case {
            name: "pdf-cyclic.pdf",
            kind: DocKind::Pdf,
            expect: Expect::Contained,
        },
        Case {
            name: "pdf-deep-array.pdf",
            kind: DocKind::Pdf,
            expect: Expect::Contained,
        },
        Case {
            name: "pdf-flate-bomb.pdf",
            kind: DocKind::Pdf,
            expect: Expect::Contained,
        },
        Case {
            name: "pdf-huge-objstm.pdf",
            kind: DocKind::Pdf,
            expect: Expect::Contained,
        },
        Case {
            name: "pdf-huge-page.pdf",
            kind: DocKind::Pdf,
            expect: Expect::Ok,
        },
        Case {
            name: "pdf-huge-paths.pdf",
            kind: DocKind::Pdf,
            expect: Expect::Contained,
        },
        Case {
            name: "pdf-as.docx",
            kind: DocKind::Docx,
            expect: Expect::OkContains("page 1"),
        },
    ];
    if done.exists() {
        return (dir, cases);
    }
    std::fs::create_dir_all(&dir).unwrap();
    let w = |n: &str, d: &[u8]| std::fs::write(dir.join(n), d).unwrap();

    let pdf = sample_pdf(3);
    w("sample.pdf", &pdf);
    w("pdf-as.docx", &pdf);
    w("big-200p.pdf", &sample_pdf(200));
    w("sample.docx", &sample_docx());
    w("sample.xlsx", &sample_xlsx());
    w("sample.pptx", &sample_pptx());
    w("sample.html", b"<!doctype html><html><head><title>T</title><style>p{}</style><script>var x=1;</script></head><body><h1>Heading</h1><p>Hello <b>HTML</b> world &amp; friends.</p><ul><li>one</li><li>two</li></ul><table><tr><td>a</td><td>b</td></tr></table></body></html>");
    w("sample.md", b"# Title\n\nSome *markdown* with `code` and a [link](http://x).\n\n- item one\n- item two\n\n```rust\nfn main() {}\n```\n");
    w("sample-1252.txt", b"Caf\xe9 cr\xe8me br\xfbl\xe9e, d\xe9j\xe0 vu. Ceci est un texte fran\xe7ais assez long pour la d\xe9tection.");

    w("zero.pdf", b"");
    w("zero.txt", b"");
    w("truncated.pdf", &pdf[..pdf.len() / 2]);
    w("binary.txt", &rnd(1 << 20, 1));
    let no_nul: Vec<u8> = rnd(1 << 20, 2).into_iter().filter(|&b| b != 0).collect();
    w("binary-nonul.txt", &no_nul);
    w("binary-as.html", &rnd(1 << 20, 3));
    w("binary-as.pdf", &rnd(1 << 20, 4));
    w("binary-as.docx", &rnd(1 << 16, 5));
    w("binary-as.xlsx", &rnd(1 << 16, 6));
    w("binary-auto.bin", &rnd(1 << 18, 7));

    // Zip bomb: word/document.xml inflates to 1 GiB of one <w:t>.
    {
        let mut z = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let o = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .large_file(true);
        z.start_file("[Content_Types].xml", o).unwrap();
        z.write_all(CT.as_bytes()).unwrap();
        z.start_file("word/document.xml", o).unwrap();
        z.write_all(
            format!(r#"<?xml version="1.0"?><w:document {NS_W}><w:body><w:p><w:r><w:t>"#)
                .as_bytes(),
        )
        .unwrap();
        let chunk = vec![b'A'; 1 << 24];
        for _ in 0..64 {
            z.write_all(&chunk).unwrap();
        }
        z.write_all(b"</w:t></w:r></w:p></w:body></w:document>")
            .unwrap();
        let bomb = z.finish().unwrap().into_inner();
        w("zipbomb.docx", &bomb);
        // Same bomb, but both headers claim 4 KiB: only the actual-inflation cap can catch it.
        let mut lying = bomb;
        patch_declared_size(&mut lying, "word/document.xml", 4096);
        w("zipbomb-lying.docx", &lying);
    }
    {
        let entries: Vec<(String, Vec<u8>)> = (0..20_000)
            .map(|i| (format!("x/{i}.xml"), b"a".to_vec()))
            .collect();
        let refs: Vec<(&str, Vec<u8>)> = entries
            .iter()
            .map(|(n, d)| (n.as_str(), d.clone()))
            .collect();
        w("zip-entries.xlsx", &zip_of(&refs));
    }
    let depth = 1_000_000;
    w("nested-xml.docx", &docx(format!(r#"<?xml version="1.0"?><w:document {NS_W}><w:body>{}<w:r><w:t>deep</w:t></w:r>{}</w:body></w:document>"#, "<w:p>".repeat(depth), "</w:p>".repeat(depth)).into_bytes()));
    w("deep-lowratio.docx", &docx(format!(r#"<?xml version="1.0"?><w:document {NS_W}><w:body>{}<w:r><w:t>deep</w:t></w:r>{}</w:body></w:document>"#, "<w:p>".repeat(300), "</w:p>".repeat(300)).into_bytes()));
    let mut lol = String::from(r#"<?xml version="1.0"?><!DOCTYPE lolz [<!ENTITY lol "lol">"#);
    for i in 1..10 {
        let inner = if i > 1 {
            format!("&lol{};", i - 1)
        } else {
            "&lol;".into()
        };
        lol.push_str(&format!(r#"<!ENTITY lol{i} "{}">"#, inner.repeat(10)));
    }
    lol.push_str(&format!(r#"]><w:document {NS_W}><w:body><w:p><w:r><w:t>&lol9;</w:t></w:r></w:p></w:body></w:document>"#));
    w("billion-laughs.docx", &docx(lol.into_bytes()));
    // 3 M empty shared strings (> the 2 M cap) in a 15 MiB part.
    w(
        "sharedstrings-bomb.xlsx",
        &zip_of(&[
            (
                "xl/workbook.xml",
                b"<workbook><sheets/></workbook>".to_vec(),
            ),
            (
                "xl/sharedStrings.xml",
                format!("<sst>{}</sst>", "<si/>".repeat(3_000_000)).into_bytes(),
            ),
        ]),
    );
    w(
        "not-ooxml.docx",
        &zip_of(&[("readme.txt", b"just a zip".to_vec())]),
    );
    let mut cfb = vec![0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
    cfb.extend(rnd(4096, 8));
    w("encrypted.docx", &cfb);

    w(
        "nested.html",
        format!(
            "<html><body>{}deep{}</body></html>",
            "<div>".repeat(200_000),
            "</div>".repeat(200_000)
        )
        .as_bytes(),
    );
    w(
        "tags-unterminated.html",
        format!("<p>start</p>{}", "<a x='".repeat(300_000)).as_bytes(),
    );
    let mut md = ">".repeat(200_000);
    md.push_str(" deep\n");
    md.push_str(&"[".repeat(100_000));
    md.push('x');
    md.push_str(&"](y)".repeat(1000));
    md.push('\n');
    w("nested.md", md.as_bytes());
    w("long-line.txt", &vec![b'a'; 48 << 20]);
    {
        let bad: [&[u8]; 6] = [
            b"\xc0\x80",
            b"\xed\xa0\x80",
            b"\xff",
            b"\xf8\x88\x80\x80\x80",
            b"\xe2\x82",
            b"ok ",
        ];
        let r = rnd(500_000, 9);
        let mut v = Vec::new();
        for x in r {
            v.extend_from_slice(bad[usize::from(x) % 6]);
        }
        w("invalid-utf8.txt", &v);
    }
    {
        let mut v = vec![0xFF, 0xFE];
        for _ in 0..10_000 {
            v.extend(
                "UTF-16 text \u{1F600} "
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes),
            );
        }
        v.push(0);
        w("utf16-odd.txt", &v);
    }
    w(
        "bidi-zw.txt",
        "\u{202e}\u{200b}\u{2066}evil\u{2069}\u{feff}"
            .repeat(200_000)
            .as_bytes(),
    );
    w(
        "zalgo.txt",
        format!("a{}", "\u{301}".repeat(1_000_000)).as_bytes(),
    );

    // Cyclic page tree with a /Count lie and a self-referencing /Prev xref.
    {
        let mut cyc = build_pdf(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [2 0 R 3 0 R] /Count 1000000 >>".to_vec(),
            b"<< /Type /Pages /Parent 2 0 R /Kids [2 0 R 3 0 R] /Count 1000000 >>".to_vec(),
        ]);
        let s = String::from_utf8_lossy(&cyc).into_owned();
        let xref_at = s.rfind("xref").unwrap();
        let s = s.replace("/Root 1 0 R >>", &format!("/Root 1 0 R /Prev {xref_at} >>"));
        cyc = s.into_bytes();
        w("pdf-cyclic.pdf", &cyc);
    }
    {
        let mut cat = b"<< /Type /Catalog /Pages 2 0 R /X ".to_vec();
        cat.extend(std::iter::repeat_n(b'[', 1_000_000));
        cat.extend(std::iter::repeat_n(b']', 1_000_000));
        cat.extend_from_slice(b" >>");
        w(
            "pdf-deep-array.pdf",
            &build_pdf(&[
                cat,
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>".to_vec(),
            ]),
        );
    }
    {
        // Content stream that inflates to 1.5 GiB of text operators (over the 1 GiB cap).
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(9));
        e.write_all(b"BT /F1 12 Tf 72 720 Td ").unwrap();
        let mut blk = b"(A) Tj ".repeat(150_000);
        blk.truncate((1 << 20) - 1);
        blk.push(b' ');
        for _ in 0..1536 {
            e.write_all(&blk).unwrap();
        }
        e.write_all(b" ET").unwrap();
        let bomb = e.finish().unwrap();
        w(
            "pdf-flate-bomb.pdf",
            &page_tree_pdf(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>",
                vec![
                    b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
                    pdf_stream(&bomb, Some("FlateDecode")),
                ],
            ),
        );
    }
    {
        let n = 2_000_000usize;
        let mut hdr = String::with_capacity(n * 16);
        for i in 0..n {
            if i > 0 {
                hdr.push(' ');
            }
            hdr.push_str(&format!("{} {}", 10 + i, i * 4));
        }
        let mut raw = hdr.clone().into_bytes();
        raw.push(b' ');
        raw.extend("[1] ".repeat(n).as_bytes());
        let objstm = zlib(&raw, 6);
        let mut o = format!(
            "<< /Type /ObjStm /N {n} /First {} /Length {} /Filter /FlateDecode >>\nstream\n",
            hdr.len() + 1,
            objstm.len()
        )
        .into_bytes();
        o.extend_from_slice(&objstm);
        o.extend_from_slice(b"\nendstream");
        w(
            "pdf-huge-objstm.pdf",
            &page_tree_pdf(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>",
                vec![o],
            ),
        );
    }
    {
        let mut ops = b"BT /F1 1 Tf ".to_vec();
        for i in 0..300_000u64 {
            ops.extend_from_slice(
                format!(
                    "1 0 0 1 {} {} Tm (W{}) Tj ",
                    i % 14400,
                    (i * 7) % 14400,
                    i % 10
                )
                .as_bytes(),
            );
        }
        ops.extend_from_slice(b"ET");
        w(
            "pdf-huge-page.pdf",
            &page_tree_pdf(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 14400 14400] /UserUnit 75000 /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>",
                vec![
                    b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
                    pdf_stream(&zlib(&ops, 9), Some("FlateDecode")),
                ],
            ),
        );
    }
    {
        let paths = b"0 0 m 1 1 l S ".repeat(3_000_000);
        w(
            "pdf-huge-paths.pdf",
            &page_tree_pdf(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R >>",
                vec![pdf_stream(&zlib(&paths, 9), Some("FlateDecode"))],
            ),
        );
    }
    std::fs::write(&done, b"ok").unwrap();
    (dir, cases)
}
