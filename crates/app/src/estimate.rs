//! Pre-scan estimate for the wizard ("About 2,300 files … about 10 minutes").
//!
//! The service's `folders.estimate` (M2-A R1, wired by the M2 tester) does the counting with the
//! real walker, filters and the active execution provider's speed; the app only words it
//! ([`from_service`]). The bounded local walk below remains as the fallback for a service that
//! predates the method (N-1 protocol): names and sizes only, file contents are never opened.

use std::path::Path;
use std::time::{Duration, Instant};

use sr_common::protocol::extract::DocKind;
use sr_common::sandbox::SecretFilter;

use crate::dto::Estimate;
use crate::state::{duration_text, thousands};

/// Stop counting here; above this the wizard suggests a smaller folder (R-UX14).
pub const MAX_FILES: u64 = 500_000;
/// Wall-clock budget for the walk; the count is then a lower bound.
pub const TIME_BUDGET: Duration = Duration::from_secs(4);

/// Measured indexing speeds (docs/04 R-UX14, M1 numbers, chunks per second).
const CHUNKS_PER_S_GPU: f64 = 600.0;
const CHUNKS_PER_S_CPU: f64 = 8.0;
/// Roughly 512 tokens per chunk ≈ 2 KB of text.
const TEXT_BYTES_PER_CHUNK: f64 = 2_000.0;

const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "__pycache__",
    "$recycle.bin",
    "system volume information",
];

/// What the walk counts, by extension: the file types of the daemon's `documents_and_code`
/// preset (`DocKind::from_extension`, the presets' own table; 2026-11-09, was a copy). Text kinds
/// count in full; binary containers (PDF, Office) count a tenth of their size as text.
fn counted(ext: &str) -> Option<bool> {
    match DocKind::from_extension(ext)? {
        DocKind::Pdf | DocKind::Docx | DocKind::Xlsx | DocKind::Pptx => Some(false),
        _ => Some(true),
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Count {
    pub files: u64,
    /// Estimated bytes of extractable text.
    pub text_bytes: u64,
    pub capped: bool,
}

/// Bounded walk. Never follows reparse points (junctions, symlinks), never opens files.
pub fn count(root: &Path, max_files: u64, budget: Duration) -> Count {
    let secrets = SecretFilter::default();
    let started = Instant::now();
    let mut c = Count::default();
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0u64;
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            visited += 1;
            if c.files >= max_files || (visited.is_multiple_of(512) && started.elapsed() > budget) {
                c.capped = true;
                return c;
            }
            let name = e.file_name().to_string_lossy().to_lowercase();
            if name.starts_with('.') || secrets.is_denied_name(&name) {
                continue;
            }
            // DirEntry::file_type does not follow links: a symlink/junction is neither dir nor file.
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) {
                    stack.push(e.path());
                }
            } else if ft.is_file() {
                let ext = name.rsplit_once('.').map_or("", |(_, x)| x);
                let len = e.metadata().map_or(0, |m| m.len());
                match counted(ext) {
                    Some(true) => {
                        c.files += 1;
                        c.text_bytes += len.min(10 * 1024 * 1024);
                    }
                    Some(false) => {
                        c.files += 1;
                        c.text_bytes += (len / 10).min(1024 * 1024);
                    }
                    None => {}
                }
            }
        }
    }
    c
}

/// Seconds for a first full index. `gpu`: `None` = unknown (assume the slower CPU path).
pub fn eta_secs(c: &Count, gpu: Option<bool>) -> u64 {
    #[allow(clippy::cast_precision_loss)]
    let chunks = (c.text_bytes as f64 / TEXT_BYTES_PER_CHUNK).max(c.files as f64);
    let rate = if gpu == Some(true) {
        CHUNKS_PER_S_GPU
    } else {
        CHUNKS_PER_S_CPU
    };
    // Scan + extraction overhead: about 2 ms per file.
    #[allow(clippy::cast_precision_loss)]
    let secs = chunks / rate + c.files as f64 * 0.002;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        secs.ceil() as u64
    }
}

/// `folders.estimate` reply → the wizard's sentence (same wording as the local fallback).
pub fn from_service(v: &serde_json::Value) -> Option<Estimate> {
    let c = Count {
        files: v.get("files")?.as_u64()?,
        text_bytes: v
            .get("text_bytes")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        capped: v
            .get("capped")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    };
    let eta = v.get("eta_s")?.as_u64()?;
    Some(estimate_with_eta(&c, eta))
}

pub fn estimate(c: &Count, gpu: Option<bool>) -> Estimate {
    estimate_with_eta(c, eta_secs(c, gpu))
}

fn estimate_with_eta(c: &Count, eta: u64) -> Estimate {
    let text = if c.files == 0 && !c.capped {
        "We didn't find documents or code files in this folder. You can still add it; new files will be picked up automatically.".to_owned()
    } else if c.capped && c.files >= MAX_FILES {
        format!(
            "This folder has more than {} files. Indexing it will take a long time; choosing a smaller folder is usually better.",
            thousands(MAX_FILES)
        )
    } else {
        let files = if c.capped {
            format!("More than {} files", thousands(c.files))
        } else if c.files == 1 {
            "1 file".to_owned()
        } else {
            format!("About {} files", thousands(c.files))
        };
        format!(
            "{files}. The first pass should take {} on this computer. Search works while it runs.",
            duration_text(eta)
        )
    };
    Estimate {
        files: u32::try_from(c.files).unwrap_or(u32::MAX),
        capped: c.capped,
        eta_s: u32::try_from(eta).unwrap_or(u32::MAX),
        text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn counts_supported_files_and_skips_hidden_secret_and_tool_dirs() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        fs::write(r.join("a.md"), "x".repeat(4000)).unwrap();
        fs::write(r.join("b.rs"), "fn main() {}").unwrap();
        fs::write(r.join("photo.jpg"), [0u8; 100]).unwrap();
        fs::write(r.join(".env"), "SECRET=1").unwrap();
        fs::write(r.join("server.pem"), "-----").unwrap();
        fs::create_dir_all(r.join(".git")).unwrap();
        fs::write(r.join(".git").join("x.txt"), "x").unwrap();
        fs::create_dir_all(r.join("node_modules").join("p")).unwrap();
        fs::write(r.join("node_modules").join("p").join("i.js"), "x").unwrap();
        fs::create_dir_all(r.join("sub").join("deep")).unwrap();
        fs::write(r.join("sub").join("deep").join("r.pdf"), [0u8; 10_000]).unwrap();
        let c = count(r, MAX_FILES, TIME_BUDGET);
        assert_eq!(c.files, 3, "{c:?}");
        assert!(!c.capped);
        assert_eq!(c.text_bytes, 4000 + 12 + 1000);
    }

    /// 2026-11-09: the walk counts the presets' file types (GDScript too), not a copied list.
    #[test]
    fn counts_the_presets_file_types() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        fs::write(r.join("temple_maps.gd"), "extends Node\n").unwrap();
        fs::write(r.join("arena.tscn"), "[gd_scene]\n").unwrap();
        fs::write(r.join("icon.png.import"), "[remap]\n").unwrap();
        fs::write(r.join("deck.pptx"), [0u8; 2_000]).unwrap();
        let c = count(r, MAX_FILES, TIME_BUDGET);
        assert_eq!(c.files, 3, "{c:?}");
        assert_eq!(c.text_bytes, 13 + 11 + 200);
        assert_eq!(counted("GD"), Some(true));
        assert_eq!(counted("xlsx"), Some(false));
        assert_eq!(counted("exe"), None);
    }

    #[test]
    fn caps_and_suggests_smaller_folder() {
        let t = tempfile::tempdir().unwrap();
        for i in 0..20 {
            fs::write(t.path().join(format!("{i}.txt")), "x").unwrap();
        }
        let c = count(t.path(), 5, TIME_BUDGET);
        assert!(c.capped);
        assert_eq!(c.files, 5);
        let e = estimate(&c, None);
        assert!(e.text.starts_with("More than 5 files"), "{}", e.text);
        let big = Count {
            files: MAX_FILES,
            text_bytes: 0,
            capped: true,
        };
        assert!(
            estimate(&big, None)
                .text
                .contains("choosing a smaller folder")
        );
    }

    #[test]
    fn time_depends_on_gpu() {
        let c = Count {
            files: 2_000,
            text_bytes: 20_000_000,
            capped: false,
        };
        let cpu = eta_secs(&c, Some(false));
        let gpu = eta_secs(&c, Some(true));
        assert!(cpu > 50 * gpu, "cpu {cpu} gpu {gpu}");
        assert_eq!(eta_secs(&c, None), cpu, "unknown = conservative");
        let e = estimate(&c, Some(true));
        assert_eq!(
            e.text,
            "About 2,000 files. The first pass should take a minute or two on this computer. Search works while it runs."
        );
        assert_eq!(
            estimate(&Count::default(), None).text,
            "We didn't find documents or code files in this folder. You can still add it; new files will be picked up automatically."
        );
    }
}
