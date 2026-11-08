//! Test-only probes (`--features test-probes`; never in release builds). A payload starting with
//! `SR-PROBE:` makes the worker misbehave on purpose so the supervisor's timeout, crash,
//! memory-limit and protocol-violation handling, and the sandbox's denials, can be tested
//! against the real binary.
#![allow(unsafe_code)]

use std::io::{self, Write};

use sr_common::protocol::extract::{
    DocKind, MAX_TEXT_BYTES, WorkerResponse, WorkerResult, write_frame, write_frame_len,
};
use sr_extract::{Extracted, Failure};

pub const PREFIX: &[u8] = b"SR-PROBE:";

/// What the main loop should do after a probe.
pub enum Probe {
    /// Send this as a normal result.
    Reply(Result<Extracted, Failure>),
    /// Run this closure as the parser (exercises the panic path).
    Parse(fn() -> Result<Extracted, Failure>),
    /// The probe already wrote whatever it wanted.
    Done,
}

fn ok(text: String) -> Probe {
    Probe::Reply(Ok(Extracted {
        text,
        detected: DocKind::Text,
        encoding: None,
        pages: None,
        truncated: false,
    }))
}

fn json(w: &mut dyn Write, id: u64, result: WorkerResult) -> io::Result<()> {
    let b = serde_json::to_vec(&WorkerResponse { id, result }).map_err(io::Error::other)?;
    write_frame(w, &b)
}

fn ok_header(text_bytes: u64) -> WorkerResult {
    WorkerResult::Ok {
        text_bytes,
        detected: DocKind::Text,
        encoding: None,
        pages: None,
        truncated: false,
        parse_ms: 0,
    }
}

pub fn run(spec: &[u8], id: u64, out: &mut dyn Write) -> io::Result<Probe> {
    let (name, arg) = match spec.iter().position(|&b| b == b':') {
        Some(i) => (&spec[..i], &spec[i + 1..]),
        None => (spec, &[][..]),
    };
    let arg_s = String::from_utf8_lossy(arg).into_owned();
    Ok(match name {
        b"hang" => loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        },
        b"spin" => {
            let mut x = 0u64;
            loop {
                x = std::hint::black_box(x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1));
            }
        }
        b"crash" => {
            // SAFETY: deliberately not safe: an access violation for the crash test.
            unsafe { std::ptr::write_volatile(std::ptr::null_mut::<u64>(), 1) };
            Probe::Done
        }
        b"stack" => {
            fn r(n: u64) -> u64 {
                let a = std::hint::black_box([n; 512]);
                if n == u64::MAX { 0 } else { a[3] + r(n + 1) }
            }
            ok(r(0).to_string())
        }
        b"abort" => std::process::abort(),
        b"exit" => std::process::exit(0),
        b"alloc" => {
            // Infallible allocations until the job's commit cap kills the process.
            let mut hold: Vec<Vec<u8>> = Vec::new();
            loop {
                hold.push(vec![1u8; 64 << 20]);
                std::hint::black_box(&hold);
            }
        }
        b"panic" => Probe::Parse(|| panic!("probe panic")),
        b"garbage" => {
            out.write_all(arg)?;
            out.flush()?;
            std::process::exit(0)
        }
        b"badlen" => {
            out.write_all(&u32::MAX.to_le_bytes())?;
            out.flush()?;
            std::thread::sleep(std::time::Duration::from_secs(3600));
            Probe::Done
        }
        b"badid" => {
            json(out, id.wrapping_add(1), ok_header(0))?;
            write_frame(out, b"")?;
            Probe::Done
        }
        b"badutf8" => {
            json(out, id, ok_header(2))?;
            write_frame(out, &[0xFF, 0xFE])?;
            Probe::Done
        }
        b"lielen" => {
            json(out, id, ok_header(10))?;
            write_frame(out, b"short")?;
            Probe::Done
        }
        b"bigtext" => {
            json(out, id, ok_header(MAX_TEXT_BYTES + 1))?;
            write_frame_len(out, MAX_TEXT_BYTES + 1)?;
            Probe::Done
        }
        b"badjson" => {
            write_frame(out, b"{not json")?;
            Probe::Done
        }
        b"read" => ok(match std::fs::read(&arg_s) {
            Ok(v) => format!("READ_ALLOWED {}", v.len()),
            Err(e) => format!("READ_DENIED {e}"),
        }),
        b"write" => ok(match std::fs::write(&arg_s, b"probe") {
            Ok(()) => {
                let _ = std::fs::remove_file(&arg_s);
                "WRITE_ALLOWED".into()
            }
            Err(e) => format!("WRITE_DENIED {e}"),
        }),
        b"net" => ok(
            match arg_s.parse().map_err(io::Error::other).and_then(|a| {
                std::net::TcpStream::connect_timeout(&a, std::time::Duration::from_secs(2))
            }) {
                Ok(_) => "NET_ALLOWED".into(),
                Err(e) => format!("NET_DENIED {e}"),
            },
        ),
        b"spawn" => ok(
            match std::process::Command::new(r"C:\Windows\System32\cmd.exe")
                .args(["/c", "exit", "0"])
                .spawn()
            {
                Ok(mut c) => {
                    let _ = c.kill();
                    "SPAWN_ALLOWED".into()
                }
                Err(e) => format!("SPAWN_DENIED {e}"),
            },
        ),
        _ => Probe::Reply(Err(Failure::new(
            sr_common::protocol::extract::WorkerFailure::Internal,
            "unknown probe",
        ))),
    })
}
