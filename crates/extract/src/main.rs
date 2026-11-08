//! `sr-extract`: the sandboxed parser worker (docs/02-architecture.md §2.1, Amendment 3.11).
//!
//! Started only by the supervisor in the daemon (`sr-extract-client`), inside an AppContainer with
//! zero capabilities (fallback: low-integrity restricted token) and a Job Object (1 GiB, kill on
//! close, no child processes). It never opens a file: requests arrive as frames on stdin, results
//! leave as frames on stdout (`sr_common::protocol::extract`). One request at a time; the parser
//! runs on a scoped thread with a large stack under `catch_unwind`.
//!
//! Usage (supervisor only): `sr-extract --pdfium <absolute path to pdfium.dll>`

#[cfg(feature = "test-probes")]
mod probes;
mod win;

use std::ffi::OsString;
use std::io::{self, BufWriter, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use sr_common::protocol::extract::{
    EXTRACT_PROTO, MAX_JSON_FRAME, MAX_PAYLOAD_BYTES, MAX_TEXT_BYTES, WorkerFailure, WorkerReady,
    WorkerRequest, WorkerResponse, WorkerResult, read_frame, read_frame_len, write_frame,
};
use sr_extract::{Extracted, Failure, PdfEngine, extract};

const EXIT_PROTOCOL: u8 = 3;
const EXIT_IO: u8 = 4;

fn main() -> ExitCode {
    let t0 = Instant::now();
    win::harden_process();
    if win::stdin_is_console() {
        eprintln!(
            "sr-extract is an internal helper of Selene Retrieval and is started automatically. \
             There is nothing to do here."
        );
        return ExitCode::from(2);
    }
    let pdfium_path = pdfium_arg(std::env::args_os().collect());
    let (engine, engine_error) = match pdfium_path {
        Some(p) => match PdfEngine::load(&p) {
            Ok(e) => (Some(e), None),
            Err(e) => (None, Some(e)),
        },
        None => (None, Some("no --pdfium argument".to_string())),
    };
    let (integrity, app_container) = win::token_info();
    let ready = WorkerReady {
        proto: EXTRACT_PROTO,
        version: env!("CARGO_PKG_VERSION").to_string(),
        pid: std::process::id(),
        integrity,
        app_container,
        pdf_engine: engine.is_some(),
        pdf_engine_error: engine_error,
        startup_ms: u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX),
    };
    match serve(&ready, engine.as_ref()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
            eprintln!("sr-extract: protocol error: {e}");
            ExitCode::from(EXIT_PROTOCOL)
        }
        Err(e) => {
            eprintln!("sr-extract: i/o error: {e}");
            ExitCode::from(EXIT_IO)
        }
    }
}

fn pdfium_arg(args: Vec<OsString>) -> Option<PathBuf> {
    let i = args.iter().position(|a| a == "--pdfium")?;
    args.get(i + 1).map(PathBuf::from)
}

fn serve(ready: &WorkerReady, engine: Option<&PdfEngine>) -> io::Result<()> {
    let mut rin = io::stdin().lock();
    let mut out = BufWriter::with_capacity(64 * 1024, io::stdout().lock());
    write_json(&mut out, ready)?;
    out.flush()?;
    loop {
        let Some(header) = read_frame(&mut rin, MAX_JSON_FRAME.into())? else {
            return Ok(()); // supervisor closed stdin: exit cleanly
        };
        let req: WorkerRequest = serde_json::from_slice(&header)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let Some(len) = read_frame_len(&mut rin, MAX_PAYLOAD_BYTES)? else {
            return Err(io::ErrorKind::UnexpectedEof.into());
        };
        let t = Instant::now();
        let payload = read_payload(&mut rin, len)?;
        #[cfg(feature = "test-probes")]
        let payload = match payload {
            Some(p) if p.starts_with(probes::PREFIX) => {
                let result = match probes::run(&p[probes::PREFIX.len()..], req.id, &mut out)? {
                    probes::Probe::Done => {
                        out.flush()?;
                        continue;
                    }
                    probes::Probe::Reply(r) => r,
                    probes::Probe::Parse(f) => run_parser(f),
                };
                respond(&mut out, req.id, result, t)?;
                continue;
            }
            other => other,
        };
        let result = match payload {
            Some(payload) => {
                let max_text =
                    usize::try_from(req.max_text_bytes.min(MAX_TEXT_BYTES)).unwrap_or(usize::MAX);
                parse(req.kind, payload, max_text, engine)
            }
            None => Err(Failure::new(
                WorkerFailure::Internal,
                "payload allocation refused by the memory limit",
            )),
        };
        respond(&mut out, req.id, result, t)?;
    }
}

fn respond(
    out: &mut impl Write,
    id: u64,
    result: Result<Extracted, Failure>,
    started: Instant,
) -> io::Result<()> {
    let parse_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    match result {
        Ok(ex) => {
            let resp = WorkerResponse {
                id,
                result: WorkerResult::Ok {
                    text_bytes: ex.text.len() as u64,
                    detected: ex.detected,
                    encoding: ex.encoding.map(str::to_string),
                    pages: ex.pages,
                    truncated: ex.truncated,
                    parse_ms,
                },
            };
            write_json(out, &resp)?;
            write_frame(out, ex.text.as_bytes())?;
        }
        Err(f) => {
            let resp = WorkerResponse {
                id,
                result: WorkerResult::Failed {
                    code: f.code,
                    detail: f.detail,
                },
            };
            write_json(out, &resp)?;
        }
    }
    out.flush()
}

/// Reads exactly `len` payload bytes. `Ok(None)` (after draining the bytes, so the stream stays
/// in sync) when the allocation is refused.
fn read_payload(r: &mut impl Read, len: u64) -> io::Result<Option<Vec<u8>>> {
    let n = usize::try_from(len).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
    let mut buf = Vec::new();
    if buf.try_reserve_exact(n).is_err() {
        io::copy(&mut r.take(len), &mut io::sink())?;
        return Ok(None);
    }
    buf.resize(n, 0);
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

/// Runs the parser, catching panics. The payload is dropped before
/// returning so its memory is free while the text is written out.
fn parse(
    kind: sr_common::protocol::extract::DocKind,
    payload: Vec<u8>,
    max_text: usize,
    engine: Option<&PdfEngine>,
) -> Result<Extracted, Failure> {
    let res = run_parser(|| extract(kind, &payload, max_text, engine));
    drop(payload);
    res
}

/// Runs a parser on this (64 MiB-stack, see build.rs) thread, turning a panic into a typed
/// failure. No thread hop per request: every handoff costs a scheduler wake-up.
fn run_parser<F>(f: F) -> Result<Extracted, Failure>
where
    F: FnOnce() -> Result<Extracted, Failure>,
{
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| Err(panic_failure()))
}

fn panic_failure() -> Failure {
    Failure::new(WorkerFailure::ParserPanic, "parser panicked")
}

fn write_json<T: serde::Serialize>(w: &mut impl Write, v: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(v).map_err(io::Error::other)?;
    write_frame(w, &bytes)
}
