//! M0-C spike: `sr-extract` worker. Parsers only. Reads framed requests on stdin,
//! writes framed responses on stdout, never opens input files itself (bytes come over the pipe).
//! Usage: sr-extract --pdfium <absolute path to pdfium.dll>
//! Spike-only: request kinds "probe:*" exercise the sandbox; enabled only when SR_EXTRACT_PROBES=1.

#[path = "../proto.rs"]
mod proto;

use std::io::{BufRead, BufReader, Cursor, Read};
use std::sync::OnceLock;
use std::time::Instant;

use pdfium_render::prelude::*;
use proto::*;

static PDFIUM: OnceLock<Pdfium> = OnceLock::new();

const MAX_PAGES: usize = 10_000;
const MAX_XML_INFLATED: u64 = 64 * 1024 * 1024; // per zip entry, actual inflated bytes
const MAX_ZIP_ENTRIES: usize = 10_000;
const MAX_ZIP_RATIO: u64 = 200;
const MAX_XML_DEPTH: usize = 256;
const PARSE_STACK: usize = 64 * 1024 * 1024;

type Ex = Result<(String, Option<u32>), (&'static str, String)>;

fn main() {
    unsafe {
        windows_sys::Win32::System::LibraryLoader::SetDefaultDllDirectories(
            windows_sys::Win32::System::LibraryLoader::LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    };
    let args: Vec<String> = std::env::args().collect();
    let dll = args.iter().position(|a| a == "--pdfium").and_then(|i| args.get(i + 1)).expect("--pdfium <path>");
    let t = Instant::now();
    let pdfium = Pdfium::new(Pdfium::bind_to_library(dll).expect("bind pdfium"));
    let _ = PDFIUM.set(pdfium);
    let pdfium_load_ms = t.elapsed().as_millis() as u64;
    if std::env::var("SR_EXTRACT_W32K_SELF").as_deref() == Ok("1") {
        // Self-lockdown after pdfium (imports user32/gdi32) is loaded and initialised.
        // PROCESS_MITIGATION_SYSTEM_CALL_DISABLE_POLICY { DisallowWin32kSystemCalls: 1 }
        let flags: u32 = 1;
        let ok = unsafe {
            windows_sys::Win32::System::Threading::SetProcessMitigationPolicy(
                windows_sys::Win32::System::Threading::ProcessSystemCallDisablePolicy,
                (&flags as *const u32).cast(),
                4,
            )
        };
        eprintln!("W32K_SELF {}", if ok == 0 { format!("failed: {}", std::io::Error::last_os_error()) } else { "applied".into() });
    }

    let probes = std::env::var("SR_EXTRACT_PROBES").as_deref() == Ok("1");
    let stdin = std::io::stdin();
    let mut rin = stdin.lock();
    let stdout = std::io::stdout();
    let mut wout = stdout.lock();
    let (integrity, app_container) = token_info();
    let ready = Ready { ready: true, pid: std::process::id(), integrity, app_container, pdfium_load_ms };
    write_frame(&mut wout, &serde_json::to_vec(&ready).unwrap()).unwrap();

    loop {
        let Some(h) = read_frame(&mut rin, MAX_HEADER).expect("read header") else { return };
        let req: Request = serde_json::from_slice(&h).expect("header json");
        let Some(payload) = read_frame(&mut rin, MAX_PAYLOAD).expect("read payload") else { return };
        let t = Instant::now();
        let kind = req.kind.clone();
        let res = std::thread::Builder::new()
            .stack_size(PARSE_STACK)
            .spawn(move || {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dispatch(&kind, &payload, probes)))
                    .unwrap_or_else(|_| Err(("ParserPanic", "parser panicked".into())))
            })
            .unwrap()
            .join()
            .unwrap_or_else(|_| Err(("ParserPanic", "join failed".into())));
        let mut resp = Response { id: req.id, worker_ms: t.elapsed().as_millis() as u64, ..Default::default() };
        match res {
            Ok((mut text, pages)) => {
                resp.truncated = truncate(&mut text, MAX_TEXT_BYTES);
                resp.ok = true;
                resp.chars = text.chars().count() as u64;
                resp.pages = pages;
                resp.text = Some(text);
            }
            Err((code, msg)) => {
                resp.code = Some(code.into());
                resp.msg = Some(msg.chars().take(500).collect());
            }
        }
        write_frame(&mut wout, &serde_json::to_vec(&resp).unwrap()).expect("write response");
    }
}

fn truncate(s: &mut String, max: usize) -> bool {
    if s.len() <= max {
        return false;
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    true
}

fn dispatch(kind: &str, b: &[u8], probes: bool) -> Ex {
    match kind {
        "pdf" => pdf(b),
        "docx" => docx(b),
        "html" => html(b),
        "md" => md(b),
        "txt" => txt(b),
        k if k.starts_with("probe:") && probes => probe(&k[6..], b),
        _ => Err(("UnsupportedKind", kind.into())),
    }
}

// ---------------- parsers ----------------

fn pdf(b: &[u8]) -> Ex {
    if b.is_empty() {
        return Err(("Empty", "zero-byte file".into()));
    }
    let pdfium = PDFIUM.get().unwrap();
    let doc = pdfium.load_pdf_from_byte_slice(b, None).map_err(|e| {
        let s = format!("{e:?}");
        (if s.contains("Password") { "PdfEncrypted" } else { "PdfInvalid" }, s)
    })?;
    let n = doc.pages().len() as usize;
    let mut out = String::new();
    for (i, page) in doc.pages().iter().enumerate() {
        if i >= MAX_PAGES || out.len() > MAX_TEXT_BYTES {
            break;
        }
        match page.text() {
            Ok(t) => out.push_str(&t.all()),
            Err(e) => return Err(("PdfInvalid", format!("page {i}: {e:?}"))),
        }
        out.push('\u{c}');
    }
    Ok((out, Some(n as u32)))
}

/// Reader that fails (instead of silently stopping) once more than `left` bytes come out.
struct Capped<R> {
    inner: R,
    left: u64,
}
impl<R: Read> Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.left == 0 {
            let mut one = [0u8; 1];
            return match self.inner.read(&mut one)? {
                0 => Ok(0),
                _ => Err(std::io::Error::other("ZIPBOMB inflated size exceeds cap")),
            };
        }
        let n = buf.len().min(self.left as usize);
        let got = self.inner.read(&mut buf[..n])?;
        self.left -= got as u64;
        Ok(got)
    }
}

fn docx(b: &[u8]) -> Ex {
    use quick_xml::events::Event;
    let mut z = zip::ZipArchive::new(Cursor::new(b)).map_err(|e| ("DocxInvalid", e.to_string()))?;
    if z.len() > MAX_ZIP_ENTRIES {
        return Err(("ZipBomb", format!("{} entries", z.len())));
    }
    let f = z.by_name("word/document.xml").map_err(|e| ("DocxInvalid", e.to_string()))?;
    let (declared, comp) = (f.size(), f.compressed_size().max(1));
    if declared > MAX_XML_INFLATED || (declared > 1 << 20 && declared / comp > MAX_ZIP_RATIO) {
        return Err(("ZipBomb", format!("declared {declared} B from {comp} B")));
    }
    let rd = BufReader::new(Capped { inner: f, left: MAX_XML_INFLATED });
    let mut r = quick_xml::Reader::from_reader(rd);
    let (mut buf, mut out, mut depth, mut in_t) = (Vec::new(), String::new(), 0usize, false);
    loop {
        match r.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                depth += 1;
                if depth > MAX_XML_DEPTH {
                    return Err(("XmlTooDeep", format!("depth > {MAX_XML_DEPTH}")));
                }
                in_t = e.local_name().as_ref() == "t";
            }
            Ok(Event::End(e)) => {
                depth = depth.saturating_sub(1);
                match e.local_name().as_ref() {
                    "t" => in_t = false,
                    "p" => out.push('\n'),
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) => match e.local_name().as_ref() {
                "tab" => out.push('\t'),
                "br" => out.push('\n'),
                _ => {}
            },
            Ok(Event::Text(t)) if in_t => out.push_str(&t.xml_content(quick_xml::XmlVersion::Implicit1_0)),
            Ok(Event::GeneralRef(g)) if in_t => {
                if let Ok(Some(c)) = g.resolve_char_ref() {
                    out.push(c);
                } else if let Some(s) = quick_xml::escape::resolve_predefined_entity(&g) {
                    out.push_str(s);
                } // custom entities are never expanded: billion-laughs is inert
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => {
                let s = e.to_string();
                return Err((if s.contains("ZIPBOMB") { "ZipBomb" } else { "XmlInvalid" }, s));
            }
        }
        if out.len() > MAX_TEXT_BYTES {
            break;
        }
        buf.clear();
    }
    Ok((out, None))
}

fn html(b: &[u8]) -> Ex {
    html2text::config::plain().string_from_read(b, 400).map(|s| (s, None)).map_err(|e| ("HtmlInvalid", e.to_string()))
}

fn md(b: &[u8]) -> Ex {
    use pulldown_cmark::{Event, Parser, TagEnd};
    let s = decode_text(b)?;
    let mut out = String::new();
    for ev in Parser::new(&s) {
        match ev {
            Event::Text(t) | Event::Code(t) => out.push_str(&t),
            Event::SoftBreak | Event::HardBreak | Event::End(TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item | TagEnd::CodeBlock) => out.push('\n'),
            _ => {}
        }
        if out.len() > MAX_TEXT_BYTES {
            break;
        }
    }
    Ok((out, None))
}

fn txt(b: &[u8]) -> Ex {
    decode_text(b).map(|s| (s, None))
}

fn decode_text(b: &[u8]) -> Result<String, (&'static str, String)> {
    if b.len() >= 2 && (b[..2] == [0xFF, 0xFE] || b[..2] == [0xFE, 0xFF]) {
        let le = b[0] == 0xFF;
        let u: Vec<u16> = b[2..].chunks_exact(2).map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) }).collect();
        return Ok(String::from_utf16_lossy(&u));
    }
    let head = &b[..b.len().min(8192)];
    let nul = head.iter().filter(|&&c| c == 0).count();
    if nul * 100 > head.len().max(1) {
        return Err(("Binary", format!("{nul} NUL bytes in first {} B", head.len())));
    }
    let b = b.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(b);
    Ok(String::from_utf8_lossy(b).into_owned())
}

// ---------------- spike-only sandbox probes ----------------

fn probe(name: &str, b: &[u8]) -> Ex {
    let arg = String::from_utf8_lossy(b).to_string();
    match name {
        "hang" => loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        },
        "spin" => {
            let mut x = 0u64;
            loop {
                x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
            }
        }
        "alloc" => {
            // Commit memory in 16 MiB steps until the job limit refuses it.
            let mut hold: Vec<Vec<u8>> = vec![];
            loop {
                let mut v: Vec<u8> = Vec::new();
                if v.try_reserve_exact(16 << 20).is_err() {
                    break;
                }
                v.resize(16 << 20, 1);
                hold.push(v);
                if hold.len() > 1024 {
                    break;
                }
            }
            Ok((format!("allocation refused after {} MiB", hold.len() * 16), None))
        }
        "alloc_abort" => {
            let v = vec![1u8; 2 << 30]; // infallible alloc → abort when the job refuses it
            Ok((format!("{}", std::hint::black_box(v).len()), None))
        }
        "crash" => unsafe {
            std::ptr::write_volatile(std::ptr::null_mut::<u64>(), 1);
            unreachable!()
        },
        "stack" => {
            fn r(n: u64) -> u64 {
                let a = std::hint::black_box([n; 512]);
                if n == u64::MAX { 0 } else { a[3] + r(n + 1) }
            }
            Ok((r(0).to_string(), None))
        }
        "spawn" => match std::process::Command::new(r"C:\Windows\System32\cmd.exe").args(["/c", "exit", "0"]).spawn() {
            Ok(mut c) => {
                let _ = c.kill();
                Ok(("SPAWN_ALLOWED".into(), None))
            }
            Err(e) => Ok((format!("SPAWN_DENIED {e}"), None)),
        },
        "write" => match std::fs::write(&arg, b"probe") {
            Ok(()) => {
                let _ = std::fs::remove_file(&arg);
                Ok((format!("WRITE_ALLOWED {arg}"), None))
            }
            Err(e) => Ok((format!("WRITE_DENIED {e}"), None)),
        },
        "read" => match std::fs::read(&arg) {
            Ok(v) => Ok((format!("READ_ALLOWED {} B", v.len()), None)),
            Err(e) => Ok((format!("READ_DENIED {e}"), None)),
        },
        "net" => match std::net::TcpStream::connect_timeout(&arg.parse().map_err(|_| ("Probe", "addr".into()))?, std::time::Duration::from_secs(2)) {
            Ok(_) => Ok(("NET_ALLOWED".into(), None)),
            Err(e) => Ok((format!("NET_DENIED {e}"), None)),
        },
        "dyncode" => {
            use windows_sys::Win32::System::Memory::*;
            let p = unsafe { VirtualAlloc(std::ptr::null(), 4096, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE) };
            if p.is_null() {
                Ok((format!("DYNCODE_DENIED {}", std::io::Error::last_os_error()), None))
            } else {
                Ok(("DYNCODE_ALLOWED".into(), None))
            }
        }
        _ => Err(("Probe", format!("unknown probe {name}"))),
    }
}

fn token_info() -> (String, bool) {
    use windows_sys::Win32::Security::*;
    use windows_sys::Win32::System::Threading::*;
    unsafe {
        let mut tok = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) == 0 {
            return ("unknown".into(), false);
        }
        let mut buf = vec![0u8; 256];
        let mut n = 0u32;
        let mut il = "unknown".to_string();
        if GetTokenInformation(tok, TokenIntegrityLevel, buf.as_mut_ptr().cast(), buf.len() as u32, &mut n) != 0 {
            let lab = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
            let cnt = *GetSidSubAuthorityCount(lab.Label.Sid);
            let rid = *GetSidSubAuthority(lab.Label.Sid, (cnt - 1) as u32);
            il = match rid {
                0x0000 => "untrusted".into(),
                0x1000 => "low".into(),
                0x2000 => "medium".into(),
                0x2100 => "medium+".into(),
                0x3000 => "high".into(),
                0x4000 => "system".into(),
                r => format!("rid 0x{r:x}"),
            };
        }
        let mut ac = 0u32;
        GetTokenInformation(tok, TokenIsAppContainer, (&mut ac as *mut u32).cast(), 4, &mut n);
        (il, ac != 0)
    }
}

#[allow(dead_code)]
fn _assert_bufread<T: BufRead>() {}
