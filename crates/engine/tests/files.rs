//! `read_file` / `grep` stay inside the user's folders: traversal, absolute paths outside, a
//! junction pointing outside, 8.3 short names of denied files, ADS, device names, trailing dots,
//! UNC, the secret denylist, size cap, binary files and Selene's line-selection semantics.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::process::Command;

use common::*;
use sr_common::error::ErrorCode;
use sr_engine::files::{GrepRequest, ReadFileRequest};
use sr_engine::{Engine, EngineHandle, ExtractSetup};

fn rq(path: &str) -> ReadFileRequest {
    ReadFileRequest {
        path: path.into(),
        ..ReadFileRequest::default()
    }
}

async fn code(e: &EngineHandle, path: &str) -> Option<ErrorCode> {
    e.read_file(rq(path)).await.err().map(|e| e.code)
}

fn junction(link: &Path, target: &Path) -> bool {
    Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The 8.3 short name of `file` (None when short names are disabled on the volume).
fn short_name(file: &Path) -> Option<String> {
    let out = Command::new("cmd")
        .args(["/C", "dir", "/X", "/A"])
        .arg(file)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let long = file.file_name()?.to_string_lossy().to_string();
    for line in text.lines() {
        if line.trim_end().ends_with(&long) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            // date time [AM|PM] size SHORT LONG…
            let i = cols
                .iter()
                .position(|c| *c == long.split(' ').next().unwrap_or(""))?;
            if i >= 1 {
                let cand = cols[i - 1];
                if cand.contains('~') {
                    return Some(cand.to_string());
                }
            }
        }
    }
    None
}

struct Fixture {
    _t: tempfile::TempDir,
    root: std::path::PathBuf,
    outside: std::path::PathBuf,
    e: EngineHandle,
}

async fn fixture() -> Fixture {
    let t = tmp();
    let root = t.path().join("root");
    let outside = t.path().join("outside");
    write_files(
        &root,
        &[
            (
                "notes.txt",
                "alpha\r\nbeta SEARCHMARK\r\ngamma\r\ndelta\r\n",
            ),
            (
                "src/main.rs",
                "fn main() { println!(\"SEARCHMARK in code\"); }\n",
            ),
            (".env", "TOKEN=SEARCHMARK-secret\n"),
            (".ssh/config", "Host x SEARCHMARK\n"),
            ("id_rsa", "SEARCHMARK private key\n"),
            ("credentials-production.json", "{\"k\":\"SEARCHMARK\"}\n"),
            ("latin1.txt", ""),
        ],
    );
    std::fs::write(root.join("latin1.txt"), b"caf\xE9 cr\xE8me\n").unwrap();
    std::fs::write(root.join("blob.bin"), [0u8, 1, 2, 3, b'S', b'E']).unwrap();
    std::fs::write(root.join("big.txt"), vec![b'x'; 6 * 1024 * 1024]).unwrap();
    let long: String = (1..=6000).map(|i| format!("line {i}\n")).collect();
    std::fs::write(root.join("long.txt"), long).unwrap();
    write_files(&outside, &[("outside.txt", "OUTSIDEMARK SEARCHMARK\n")]);
    let data = t.path().join("d");
    let e = Engine::start(config(&data, ExtractSetup::Disabled))
        .await
        .unwrap();
    e.add_folder(&root.to_string_lossy(), Some("Root".into()))
        .await
        .unwrap();
    Fixture {
        _t: t,
        root,
        outside,
        e,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_file_parity_and_line_selection() {
    let f = fixture().await;
    let e = &f.e;
    let r = e.read_file(rq("notes.txt")).await.unwrap();
    assert!(
        r.content
            .starts_with("   1 | alpha\n   2 | beta SEARCHMARK"),
        "{}",
        r.content
    );
    assert_eq!((r.start_line, r.end_line, r.total_lines), (1, 5, 5));
    assert!(!r.truncated && !r.is_binary);
    let abs = f.root.join("notes.txt");
    let r = e.read_file(rq(&abs.to_string_lossy())).await.unwrap();
    assert_eq!(r.rel_path, "notes.txt");
    let r = e
        .read_file(ReadFileRequest {
            head: Some(2),
            ..rq("notes.txt")
        })
        .await
        .unwrap();
    assert_eq!((r.start_line, r.end_line, r.truncated), (1, 2, true));
    let r = e
        .read_file(ReadFileRequest {
            tail: Some(2),
            ..rq("notes.txt")
        })
        .await
        .unwrap();
    assert_eq!((r.start_line, r.end_line), (4, 5));
    let r = e
        .read_file(ReadFileRequest {
            start_line: Some(2),
            end_line: Some(3),
            ..rq("notes.txt")
        })
        .await
        .unwrap();
    assert_eq!(r.content, "   2 | beta SEARCHMARK\n   3 | gamma");
    let r = e.read_file(rq("long.txt")).await.unwrap();
    assert_eq!((r.end_line, r.total_lines, r.truncated), (5000, 6001, true));
    for bad in [
        ReadFileRequest {
            start_line: Some(0),
            ..rq("notes.txt")
        },
        ReadFileRequest {
            start_line: Some(3),
            end_line: Some(2),
            ..rq("notes.txt")
        },
        ReadFileRequest {
            head: Some(1),
            start_line: Some(1),
            ..rq("notes.txt")
        },
        ReadFileRequest {
            tail: Some(10_001),
            ..rq("notes.txt")
        },
    ] {
        assert_eq!(
            e.read_file(bad).await.err().map(|e| e.code),
            Some(ErrorCode::InvalidInput)
        );
    }
    let r = e.read_file(rq("latin1.txt")).await.unwrap();
    assert_eq!(r.encoding.as_deref(), Some("windows-1252"));
    assert!(r.content.contains("café crème"));
    let r = e.read_file(rq("blob.bin")).await.unwrap();
    assert!(r.is_binary && r.content.is_empty());
    let err = e.read_file(rq("big.txt")).await.err().unwrap();
    assert_eq!(err.code, ErrorCode::FileTooLarge);
    assert!(
        err.user_message().body.contains("5 MB"),
        "{}",
        err.user_message().body
    );
    assert_eq!(code(e, "missing.txt").await, Some(ErrorCode::NotFound));
    f.e.drain().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_file_refuses_everything_outside_or_private() {
    let f = fixture().await;
    let e = &f.e;
    let outside_abs = f.outside.join("outside.txt");
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut cases: Vec<(String, ErrorCode)> = vec![
        (
            "../outside/outside.txt".into(),
            ErrorCode::PathOutsideSandbox,
        ),
        (
            "src/../../outside/outside.txt".into(),
            ErrorCode::PathOutsideSandbox,
        ),
        (
            outside_abs.to_string_lossy().into_owned(),
            ErrorCode::PathOutsideSandbox,
        ),
        (
            format!(r"{}\..\outside\outside.txt", f.root.display()),
            ErrorCode::PathOutsideSandbox,
        ),
        (".env".into(), ErrorCode::PathDenied),
        (".ENV".into(), ErrorCode::PathDenied),
        (".ssh/config".into(), ErrorCode::PathDenied),
        ("id_rsa".into(), ErrorCode::PathDenied),
        ("credentials-production.json".into(), ErrorCode::PathDenied),
    ];
    // Windows namespaces (UNC, `\\?\`, drive-relative, NTFS streams, device names, trailing
    // dots/spaces). On Unix these are ordinary file names inside the folder.
    #[cfg(windows)]
    cases.extend([
        (
            r"\\localhost\c$\Windows\win.ini".into(),
            ErrorCode::PathOutsideSandbox,
        ),
        (
            r"\\?\C:\Windows\win.ini".into(),
            ErrorCode::PathOutsideSandbox,
        ),
        ("C:Windows\\win.ini".into(), ErrorCode::PathOutsideSandbox),
        ("notes.txt:hidden".into(), ErrorCode::PathOutsideSandbox),
        (
            format!(r"{}\notes.txt:hidden:$DATA", f.root.display()),
            ErrorCode::PathOutsideSandbox,
        ),
        ("CON".into(), ErrorCode::PathOutsideSandbox),
        ("nul.txt".into(), ErrorCode::PathOutsideSandbox),
        ("src/COM1.rs".into(), ErrorCode::PathOutsideSandbox),
        ("notes.txt.".into(), ErrorCode::PathOutsideSandbox),
        ("notes.txt ".into(), ErrorCode::PathOutsideSandbox),
    ]);
    for (p, want) in &cases {
        assert_eq!(code(e, p).await, Some(*want), "{p}");
    }
    // A junction inside the folder that points outside it.
    if junction(&f.root.join("jlink"), &f.outside) {
        assert_eq!(
            code(e, "jlink/outside.txt").await,
            Some(ErrorCode::PathOutsideSandbox)
        );
        let abs = f.root.join("jlink").join("outside.txt");
        assert_eq!(
            code(e, &abs.to_string_lossy()).await,
            Some(ErrorCode::PathOutsideSandbox)
        );
    } else {
        eprintln!("SKIP junction case: mklink /J failed");
    }
    // The 8.3 alias of a denied file is expanded before the denylist check.
    match short_name(&f.root.join("credentials-production.json")) {
        Some(s) => assert_eq!(code(e, &s).await, Some(ErrorCode::PathDenied), "{s}"),
        None => eprintln!("SKIP 8.3 case: short names disabled on this volume"),
    }
    // Plain, actionable text for the agent.
    let err = e
        .read_file(rq("../outside/outside.txt"))
        .await
        .err()
        .unwrap();
    let m = err.user_message();
    assert_eq!(m.title, "That file is outside your folders");
    assert!(!m.agent_text().contains("PathOutsideSandbox"));
    f.e.drain().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grep_stays_inside_and_skips_private_files() {
    let f = fixture().await;
    let e = &f.e;
    let _ = junction(&f.root.join("jlink"), &f.outside);
    let g = |pattern: &str| GrepRequest {
        pattern: pattern.into(),
        ..GrepRequest::default()
    };
    let r = e.grep(g("SEARCHMARK")).await.unwrap();
    let rels: Vec<&str> = r.matches.iter().map(|m| m.rel_path.as_str()).collect();
    assert!(
        rels.contains(&"notes.txt") && rels.contains(&"src/main.rs"),
        "{rels:?}"
    );
    for denied in [
        ".env",
        ".ssh/config",
        "id_rsa",
        "credentials-production.json",
    ] {
        assert!(!rels.contains(&denied), "{denied} grepped: {rels:?}");
    }
    assert!(
        r.matches.iter().all(|m| !m.text.contains("OUTSIDEMARK")),
        "junction followed"
    );
    let m = r
        .matches
        .iter()
        .find(|m| m.rel_path == "notes.txt")
        .unwrap();
    assert_eq!(m.line, 2);
    assert!(
        m.path
            .ends_with(&format!("root{}notes.txt", std::path::MAIN_SEPARATOR))
    );
    // Scoped by path and glob, literal and case-insensitive.
    let r = e
        .grep(GrepRequest {
            path: Some("src".into()),
            ..g("searchmark")
        })
        .await
        .unwrap();
    assert!(r.matches.is_empty(), "case-sensitive by default");
    let r = e
        .grep(GrepRequest {
            glob: Some("*.rs".into()),
            case_insensitive: true,
            ..g("searchmark")
        })
        .await
        .unwrap();
    assert_eq!(r.matches.len(), 1);
    let r = e
        .grep(GrepRequest {
            fixed_strings: true,
            ..g("println!(\"SEARCHMARK")
        })
        .await
        .unwrap();
    assert_eq!(r.matches.len(), 1);
    let r = e
        .grep(GrepRequest {
            max_results: Some(1),
            ..g("SEARCHMARK")
        })
        .await
        .unwrap();
    assert!(r.truncated && r.matches.len() == 1);
    // Refusals.
    for (req, want) in [
        (
            GrepRequest {
                path: Some(".env".into()),
                ..g("x")
            },
            ErrorCode::PathDenied,
        ),
        (
            GrepRequest {
                path: Some("../outside".into()),
                ..g("x")
            },
            ErrorCode::PathOutsideSandbox,
        ),
        (g("("), ErrorCode::InvalidInput),
        (g(""), ErrorCode::InvalidInput),
    ] {
        assert_eq!(e.grep(req).await.err().map(|e| e.code), Some(want));
    }
    f.e.drain().await;
}

/// M1 tester regression: a `.gitignore` *above* the folder root (e.g. the parent repo of a
/// subfolder the user added) must not hide files from grep. The indexer ignores it too, so those
/// files are searchable; grep has to agree.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grep_ignores_gitignore_files_above_the_folder() {
    let t = tmp();
    std::fs::write(t.path().join(".gitignore"), "docs/\n*.md\n").unwrap();
    let root = t.path().join("root");
    write_files(
        &root,
        &[
            ("docs/guide.md", "PARENTIGNOREMARK in docs\n"),
            ("notes.txt", "PARENTIGNOREMARK in notes\n"),
            (".gitignore", "skipped/\n"),
            ("skipped/x.txt", "PARENTIGNOREMARK skipped\n"),
        ],
    );
    let e = Engine::start(config(&t.path().join("d"), ExtractSetup::Disabled))
        .await
        .unwrap();
    e.add_folder(&root.to_string_lossy(), Some("Root".into()))
        .await
        .unwrap();
    let r = e
        .grep(GrepRequest {
            pattern: "PARENTIGNOREMARK".into(),
            ..GrepRequest::default()
        })
        .await
        .unwrap();
    let mut rels: Vec<&str> = r.matches.iter().map(|m| m.rel_path.as_str()).collect();
    rels.sort_unstable();
    assert_eq!(
        rels,
        ["docs/guide.md", "notes.txt"],
        "the folder's own .gitignore still applies"
    );
    e.drain().await;
}
