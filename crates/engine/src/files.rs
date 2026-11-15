//! `read_file` and `grep` (docs/03: `path-utils.ts`, `read-utils.ts`, `read-file-tool.ts`,
//! `lib/ai/ripgrep`). Both run only inside the user's folders, through [`SandboxFs`], with the
//! secret denylist always on.
//!
//! `read_file` parity with Selene:
//! * containment: realpath of the first existing ancestor must be inside a folder root
//!   (`path-utils.ts`), plus the stricter syntax rules of `sr_common::sandbox`;
//! * hard size cap of 5 MiB checked **before** reading (`read-utils.ts`), `FileTooLarge`;
//! * binary detection: a NUL byte in the first 1 KiB (`file-content-utils.ts`) → a soft
//!   "non-text binary file" result, not an error;
//! * line selection: exactly one of `head`, `tail`, or `startLine`/`endLine`; each ≥ 1; `endLine ≥
//!   startLine`; at most 10 000 lines per range; a full read shows the first 5 000 lines;
//! * output lines numbered `"%4d | "`, lines over 2 000 chars cut with `... [truncated]`.
//!
//! Additions: the text is decoded (UTF-8, UTF-8/16 BOMs, else Windows-1252) and the encoding is
//! reported; a `\r` before `\n` is not shown, and a lone `\r` inside a line is shown as a space
//! (line numbers count `\n` only, as the index does, so they stay exact).

use std::io::Read;
use std::path::{Path, PathBuf};

use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, SearcherBuilder, sinks};
use serde::{Deserialize, Serialize};
use sr_common::error::{Error, ErrorCode};
use sr_common::ids::FolderId;

use crate::sandboxfs::{Located, SandboxFs};

/// Hard cap for reads (Selene `READ_SYNCED_FILE_MAX_BYTES`).
pub const READ_MAX_BYTES: u64 = 5 * 1024 * 1024;
/// Lines shown for a full-file read (Selene `MAX_LINE_COUNT`).
pub const MAX_LINE_COUNT: usize = 5000;
/// Longest line shown (Selene `MAX_LINE_WIDTH`).
pub const MAX_LINE_WIDTH: usize = 2000;
/// Longest explicit range (Selene `MAX_RANGE_LINES`).
pub const MAX_RANGE_LINES: u64 = 10_000;
/// Binary sniff window (Selene `isBinaryFile`).
pub const BINARY_SNIFF_BYTES: usize = 1024;

/// `read_file` arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadFileRequest {
    pub path: String,
    pub start_line: Option<u64>,
    pub end_line: Option<u64>,
    pub head: Option<u64>,
    pub tail: Option<u64>,
    /// Restrict to these folders (`None` = every folder).
    pub folder_ids: Option<Vec<FolderId>>,
}

/// `read_file` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadFileResult {
    /// Absolute path (display form).
    pub path: String,
    pub rel_path: String,
    pub folder_id: FolderId,
    /// Numbered lines (`"   1 | text"`), empty for binary files.
    pub content: String,
    pub start_line: u64,
    pub end_line: u64,
    pub total_lines: u64,
    pub truncated: bool,
    pub bytes: u64,
    pub encoding: Option<String>,
    pub is_binary: bool,
    /// Plain one-liner for the agent.
    pub message: String,
}

/// `root` joined with a catalog path (`/`-separated, relative). Windows: one join with `\`
/// separators (the shown path is unchanged); elsewhere: one join per non-empty component, so a
/// leading `/` can never replace `root`.
pub(crate) fn join_rel(root: &Path, rel: &str) -> PathBuf {
    #[cfg(windows)]
    {
        root.join(rel.replace('/', "\\"))
    }
    #[cfg(not(windows))]
    {
        rel.split('/')
            .filter(|c| !c.is_empty())
            .fold(root.to_path_buf(), |p, c| p.join(c))
    }
}

fn invalid(detail: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidInput).with_detail(detail)
}

/// Selene's argument checks (`read-file-tool.ts`).
pub fn validate_selection(r: &ReadFileRequest) -> Result<(), Error> {
    for (name, v) in [
        ("startLine", r.start_line),
        ("endLine", r.end_line),
        ("head", r.head),
        ("tail", r.tail),
    ] {
        if v == Some(0) {
            return Err(invalid(format!("{name} must be a positive integer")));
        }
    }
    if let (Some(s), Some(e)) = (r.start_line, r.end_line)
        && e < s
    {
        return Err(invalid(format!("endLine ({e}) must be >= startLine ({s})")));
    }
    if (r.head.is_some() || r.tail.is_some()) && (r.start_line.is_some() || r.end_line.is_some()) {
        return Err(invalid(
            "use either head/tail or startLine/endLine, not both",
        ));
    }
    if r.head.is_some() && r.tail.is_some() {
        return Err(invalid("use either head or tail, not both"));
    }
    for (name, v) in [("head", r.head), ("tail", r.tail)] {
        if v.is_some_and(|n| n > MAX_RANGE_LINES) {
            return Err(invalid(format!(
                "{name} exceeds the maximum range of {MAX_RANGE_LINES} lines"
            )));
        }
    }
    if let (Some(s), Some(e)) = (r.start_line, r.end_line)
        && e - s + 1 > MAX_RANGE_LINES
    {
        return Err(invalid(format!(
            "range exceeds the maximum of {MAX_RANGE_LINES} lines"
        )));
    }
    Ok(())
}

/// Decode text: BOMs first, then UTF-8, else Windows-1252 (never fails).
pub fn decode(bytes: &[u8]) -> (String, &'static str) {
    if let Some((enc, bom_len)) = encoding_rs::Encoding::for_bom(bytes) {
        let (text, _) = enc.decode_without_bom_handling(&bytes[bom_len..]);
        return (text.into_owned(), enc.name());
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_string(), "UTF-8"),
        Err(_) => {
            let (text, _, _) = encoding_rs::WINDOWS_1252.decode(bytes);
            (text.into_owned(), "windows-1252")
        }
    }
}

/// Selene `isBinaryFile`: a NUL byte in the first 1 KiB, unless the file has a UTF-16/32 BOM.
pub fn looks_binary(bytes: &[u8]) -> bool {
    if encoding_rs::Encoding::for_bom(bytes).is_some() {
        return false;
    }
    bytes.iter().take(BINARY_SNIFF_BYTES).any(|b| *b == 0)
}

/// Selene `selectLines`: returns (selected, first line no, last line no).
fn select<'a>(all: &'a [&'a str], r: &ReadFileRequest) -> (&'a [&'a str], u64, u64) {
    let n = all.len() as u64;
    if let Some(h) = r.head {
        let end = n.min(h);
        return (&all[..end as usize], 1, end);
    }
    if let Some(t) = r.tail {
        let start = n.saturating_sub(t).saturating_add(1).max(1);
        return (&all[(start - 1) as usize..], start, n);
    }
    if r.start_line.is_some() || r.end_line.is_some() {
        let start = r.start_line.unwrap_or(1).max(1);
        let end = n.min(r.end_line.unwrap_or(n));
        if start > end {
            return (&[], start, end);
        }
        return (&all[(start - 1) as usize..end as usize], start, end);
    }
    if n > MAX_LINE_COUNT as u64 {
        return (&all[..MAX_LINE_COUNT], 1, MAX_LINE_COUNT as u64);
    }
    (all, 1, n)
}

/// Passage text as sent to clients and models: Windows (`\r\n`) and old Mac (lone `\r`) line
/// ends become `\n`. A CR is a wasted token on every line of a Windows file. `line_start` /
/// `line_end` are unchanged (they come from the index, which counts `\n`).
pub fn normalize_newlines(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('\r') {
        return std::borrow::Cow::Borrowed(text);
    }
    std::borrow::Cow::Owned(text.replace("\r\n", "\n").replace('\r', "\n"))
}

/// One numbered line (`read_file`, `grep`): its line end dropped, and a lone `\r` inside it shown
/// as a space, so one output line stays one file line and the numbers stay exact.
pub fn one_line(line: &str) -> std::borrow::Cow<'_, str> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.contains('\r') {
        std::borrow::Cow::Owned(line.replace('\r', " "))
    } else {
        std::borrow::Cow::Borrowed(line)
    }
}

fn format_lines(lines: &[&str], first: u64) -> String {
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let line = one_line(line);
        let line = line.as_ref();
        out.push_str(&format!("{:>4} | ", first + i as u64));
        if line.chars().count() > MAX_LINE_WIDTH {
            out.extend(line.chars().take(MAX_LINE_WIDTH));
            out.push_str("... [truncated]");
        } else {
            out.push_str(line);
        }
    }
    out
}

fn file_name(l: &Located) -> String {
    Path::new(l.rel())
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Read a file inside the user's folders (blocking).
pub fn read_file(fs: &SandboxFs, r: &ReadFileRequest) -> Result<ReadFileResult, Error> {
    validate_selection(r)?;
    let loc = fs.resolve_client(&r.path, r.folder_ids.as_deref())?;
    let name = file_name(&loc);
    // M5-E: never download an online-only OneDrive file just to read it (opening it would).
    if crate::win::is_online_only(&loc.resolved.path) {
        return Err(Error::new(ErrorCode::FileOnlineOnly).with_path(PathBuf::from(&name)));
    }
    let mut f = fs.open(&loc)?;
    let bytes = f
        .metadata()
        .map_err(|e| Error::from(e).with_name(&name))?
        .len();
    if bytes > READ_MAX_BYTES {
        let mut e = Error::new(ErrorCode::FileTooLarge).with_name(&name);
        e.ctx.limit = Some("5 MB".into());
        return Err(e);
    }
    let mut buf = Vec::with_capacity(usize::try_from(bytes).unwrap_or(0));
    // Never read more than the cap even if the file grew after the size check.
    (&mut f)
        .take(READ_MAX_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| Error::from(e).with_name(&name))?;
    if buf.len() as u64 > READ_MAX_BYTES {
        let mut e = Error::new(ErrorCode::FileTooLarge).with_name(&name);
        e.ctx.limit = Some("5 MB".into());
        return Err(e);
    }
    let path = loc.display_path().to_string_lossy().into_owned();
    if looks_binary(&buf) {
        return Ok(ReadFileResult {
            path,
            rel_path: loc.rel().to_string(),
            folder_id: loc.folder_id,
            content: String::new(),
            start_line: 0,
            end_line: 0,
            total_lines: 0,
            truncated: false,
            bytes,
            encoding: None,
            is_binary: true,
            message: format!(
                "\"{name}\" is a non-text binary file (compiled, archive, etc.); only text files can be read."
            ),
        });
    }
    let (text, encoding) = decode(&buf);
    let all: Vec<&str> = text.split('\n').collect();
    let (sel, first, last) = select(&all, r);
    let truncated = sel.len() < all.len();
    let total = all.len() as u64;
    let message = if truncated {
        format!("Showing lines {first}-{last} of {total} total lines")
    } else {
        format!("Read {total} lines from {name}")
    };
    Ok(ReadFileResult {
        path,
        rel_path: loc.rel().to_string(),
        folder_id: loc.folder_id,
        content: format_lines(sel, first),
        start_line: first,
        end_line: last,
        total_lines: total,
        truncated,
        bytes,
        encoding: Some(encoding.to_string()),
        is_binary: false,
        message,
    })
}

// ------------------------------------------------------------------------------------------------
// grep

/// Default and maximum number of matches returned.
pub const GREP_DEFAULT_MAX: u32 = 100;
pub const GREP_MAX: u32 = 500;
/// Longest pattern accepted.
pub const GREP_MAX_PATTERN: usize = 1000;
/// Longest line text returned per match.
pub const GREP_MAX_LINE: usize = 500;
/// S-11: one grep reads at most this many file bytes in total …
pub const GREP_MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
/// … and runs at most this long; then it stops with `truncated: true`.
pub const GREP_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// S-11: the wall-clock and bytes budget of one grep. Pure, for tests.
#[derive(Debug, Clone, Copy)]
pub struct GrepBudget {
    pub started: std::time::Instant,
    pub deadline: std::time::Duration,
    pub max_bytes: u64,
}

impl GrepBudget {
    pub fn new() -> Self {
        Self {
            started: std::time::Instant::now(),
            deadline: GREP_DEADLINE,
            max_bytes: GREP_MAX_TOTAL_BYTES,
        }
    }

    /// May a file of `next` bytes still be searched after `used` bytes, at `now`?
    pub fn allows(&self, used: u64, next: u64, now: std::time::Instant) -> bool {
        now.saturating_duration_since(self.started) < self.deadline
            && used.saturating_add(next) <= self.max_bytes
    }
}

impl Default for GrepBudget {
    fn default() -> Self {
        Self::new()
    }
}

/// `grep` arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepRequest {
    pub pattern: String,
    /// A file or directory inside a folder (absolute or folder-relative); `None` = every folder.
    pub path: Option<String>,
    pub folder_ids: Option<Vec<FolderId>>,
    /// Only files matching this glob (e.g. `*.rs`).
    pub glob: Option<String>,
    pub case_insensitive: bool,
    /// Treat the pattern as a literal string.
    pub fixed_strings: bool,
    pub max_results: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepMatch {
    pub path: String,
    pub rel_path: String,
    pub folder_id: FolderId,
    pub line: u64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepResult {
    pub matches: Vec<GrepMatch>,
    pub truncated: bool,
    pub files_searched: u64,
    /// M5-E: online-only OneDrive files that were not searched (reading would download them).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub online_only_skipped: u64,
    /// Plain note when `online_only_skipped > 0` (`sr_common::api::ONLINE_ONLY_NOTE`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

struct Target {
    folder_id: FolderId,
    root_display: std::path::PathBuf,
    canonical_root: std::path::PathBuf,
    /// Root-relative start (file or directory), `/`-separated; "" = the root.
    start: String,
}

/// Search file contents inside the user's folders (blocking).
pub fn grep(fs: &SandboxFs, r: &GrepRequest) -> Result<GrepResult, Error> {
    grep_with_budget(fs, r, GrepBudget::new())
}

/// [`grep`] with an explicit budget (S-11: stops with `truncated` when time or bytes run out).
pub fn grep_with_budget(
    fs: &SandboxFs,
    r: &GrepRequest,
    budget: GrepBudget,
) -> Result<GrepResult, Error> {
    let mut bytes_used: u64 = 0;
    if r.pattern.is_empty() {
        return Err(invalid("pattern must not be empty"));
    }
    if r.pattern.len() > GREP_MAX_PATTERN {
        return Err(invalid("pattern is too long"));
    }
    let max = r.max_results.unwrap_or(GREP_DEFAULT_MAX).clamp(1, GREP_MAX) as usize;
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(r.case_insensitive)
        .fixed_strings(r.fixed_strings)
        .line_terminator(Some(b'\n'))
        .size_limit(10 * (1 << 20))
        .dfa_size_limit(10 * (1 << 20))
        .build(&r.pattern)
        .map_err(|e| invalid(format!("bad pattern: {e}")))?;
    let mut targets = Vec::new();
    match &r.path {
        Some(p) => {
            let loc = fs.resolve_client(p, r.folder_ids.as_deref())?;
            if !loc.resolved.path.exists() {
                return Err(Error::new(ErrorCode::NotFound).with_name(file_name(&loc)));
            }
            let canonical_root = fs
                .canonical_root(loc.folder_id)
                .ok_or_else(|| Error::new(ErrorCode::FolderUnavailable))?;
            targets.push(Target {
                folder_id: loc.folder_id,
                root_display: loc.root_path.clone(),
                canonical_root,
                start: loc.rel().to_string(),
            });
        }
        None => {
            for f in fs.folder_ids() {
                if r.folder_ids.as_ref().is_some_and(|s| !s.contains(&f)) {
                    continue;
                }
                let (Some(canonical_root), Some((_, root))) =
                    (fs.canonical_root(f), fs.root_dir(f))
                else {
                    continue;
                };
                targets.push(Target {
                    folder_id: f,
                    root_display: root.to_path_buf(),
                    canonical_root,
                    start: String::new(),
                });
            }
        }
    }
    let glob = match &r.glob {
        Some(g) => Some(
            globset::GlobBuilder::new(g)
                .case_insensitive(true)
                .literal_separator(false)
                .build()
                .map_err(|e| invalid(format!("bad glob: {e}")))?
                .compile_matcher(),
        ),
        None => None,
    };
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(0))
        .line_number(true)
        .build();
    let mut out = GrepResult {
        matches: Vec::new(),
        truncated: false,
        files_searched: 0,
        online_only_skipped: 0,
        note: None,
    };
    'targets: for t in &targets {
        let Some((dir, _)) = fs.root_dir(t.folder_id) else {
            continue;
        };
        let start_abs = if t.start.is_empty() {
            t.canonical_root.clone()
        } else {
            t.canonical_root.join(&t.start)
        };
        let walk = ignore::WalkBuilder::new(&start_abs)
            .follow_links(false)
            .hidden(true)
            .git_ignore(true)
            .git_global(false)
            .git_exclude(false)
            .require_git(false)
            // Only ignore files inside the folder count (as in sr-sync's walker): a `.gitignore`
            // above the user's folder must not hide indexed files from grep (M1 tester fix).
            .parents(false)
            .max_filesize(Some(READ_MAX_BYTES))
            .build();
        for entry in walk {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue; // directories, symlinks, junctions
            }
            let Ok(rel) = entry.path().strip_prefix(&t.canonical_root) else {
                continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            if fs.is_denied_rel(&rel) || sr_common::sandbox::parse_relative(&rel).is_err() {
                continue;
            }
            if let Some(g) = &glob {
                let name = Path::new(&rel)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned());
                if !(g.is_match(&rel) || name.is_some_and(|n| g.is_match(n))) {
                    continue;
                }
            }
            // M5-E: the walker's metadata comes from the directory listing (no open), so an
            // online-only OneDrive file is skipped without being downloaded.
            if entry
                .metadata()
                .is_ok_and(|m| sr_sync::fsid::is_online_only(&m))
            {
                out.online_only_skipped += 1;
                continue;
            }
            // S-11: a same-user caller can't make one grep run for ever or read the whole disk.
            let size = entry.metadata().map_or(0, |m| m.len());
            if !budget.allows(bytes_used, size, std::time::Instant::now()) {
                out.truncated = true;
                break 'targets;
            }
            bytes_used = bytes_used.saturating_add(size);
            // Open through the root's capability handle (never the walker's path).
            let Ok(file) = dir.open(&rel) else { continue };
            let file = file.into_std();
            out.files_searched += 1;
            let display = join_rel(&t.root_display, &rel);
            let mut hit_limit = false;
            let res = searcher.search_file(
                &matcher,
                &file,
                sinks::Lossy(|line, text| {
                    let mut text = one_line(text).into_owned();
                    if text.chars().count() > GREP_MAX_LINE {
                        text = text.chars().take(GREP_MAX_LINE).collect::<String>() + "…";
                    }
                    out.matches.push(GrepMatch {
                        path: display.to_string_lossy().into_owned(),
                        rel_path: rel.clone(),
                        folder_id: t.folder_id,
                        line,
                        text,
                    });
                    if out.matches.len() >= max {
                        hit_limit = true;
                        return Ok(false);
                    }
                    Ok(true)
                }),
            );
            if res.is_err() {
                continue;
            }
            if hit_limit {
                out.truncated = true;
                break 'targets;
            }
        }
    }
    if out.online_only_skipped > 0 {
        out.note = Some(sr_common::api::ONLINE_ONLY_NOTE.to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(s: Option<u64>, e: Option<u64>, h: Option<u64>, t: Option<u64>) -> ReadFileRequest {
        ReadFileRequest {
            path: "x".into(),
            start_line: s,
            end_line: e,
            head: h,
            tail: t,
            folder_ids: None,
        }
    }

    #[test]
    fn grep_budget_stops_on_time_or_bytes() {
        let t0 = std::time::Instant::now();
        let b = GrepBudget {
            started: t0,
            deadline: std::time::Duration::from_secs(30),
            max_bytes: 100,
        };
        assert!(b.allows(0, 100, t0));
        assert!(!b.allows(60, 41, t0), "bytes budget");
        assert!(!b.allows(u64::MAX, 1, t0), "no overflow");
        assert!(
            !b.allows(0, 1, t0 + std::time::Duration::from_secs(31)),
            "deadline"
        );
        assert_eq!(GrepBudget::new().max_bytes, GREP_MAX_TOTAL_BYTES);
    }

    #[test]
    fn selection_rules_match_selene() {
        assert!(validate_selection(&req(Some(0), None, None, None)).is_err());
        assert!(validate_selection(&req(Some(5), Some(4), None, None)).is_err());
        assert!(validate_selection(&req(Some(1), None, Some(3), None)).is_err());
        assert!(validate_selection(&req(None, None, Some(10_001), None)).is_err());
        assert!(validate_selection(&req(Some(1), Some(10_001), None, None)).is_err());
        assert!(validate_selection(&req(Some(1), Some(10_000), None, None)).is_ok());
        let lines: Vec<&str> = (0..10).map(|_| "l").collect();
        assert_eq!(
            select(&lines, &req(None, None, Some(3), None)).1
                ..=select(&lines, &req(None, None, Some(3), None)).2,
            1..=3
        );
        let (s, a, b) = select(&lines, &req(None, None, None, Some(3)));
        assert_eq!((s.len(), a, b), (3, 8, 10));
        let (s, a, b) = select(&lines, &req(Some(4), Some(99), None, None));
        assert_eq!((s.len(), a, b), (7, 4, 10));
        let many: Vec<&str> = (0..6000).map(|_| "l").collect();
        let (s, a, b) = select(&many, &req(None, None, None, None));
        assert_eq!((s.len(), a, b), (5000, 1, 5000));
    }

    #[test]
    fn numbering_and_long_lines() {
        let long = "x".repeat(2100);
        let lines = ["a\r", long.as_str()];
        let s = format_lines(&lines, 9);
        assert!(s.starts_with("   9 | a\n  10 | "), "{s}");
        assert!(s.ends_with("... [truncated]"));
    }

    /// CRLF files: no `\r` reaches a client, and the numbers stay those of the file.
    #[test]
    fn line_ends_are_normalised_without_moving_line_numbers() {
        assert_eq!(normalize_newlines("a\r\nb\r\n\r\nc"), "a\nb\n\nc");
        assert_eq!(normalize_newlines("old\rmac\r"), "old\nmac\n");
        assert!(matches!(
            normalize_newlines("plain\ntext"),
            std::borrow::Cow::Borrowed(_)
        ));
        assert_eq!(one_line("a\r"), "a");
        assert_eq!(one_line("x\ry\r\n"), "x y");
        let text = "one\r\ntwo\rstill two\r\nthree\r\n";
        let all: Vec<&str> = text.split('\n').collect();
        let s = format_lines(&all[1..3], 2);
        assert_eq!(s, "   2 | two still two\n   3 | three");
        assert!(!s.contains('\r'));
    }

    #[test]
    fn decoding_and_binary_sniff() {
        assert_eq!(decode(b"hi").1, "UTF-8");
        assert_eq!(decode(b"\xEF\xBB\xBFhi").0, "hi");
        let utf16: Vec<u8> = [0xFF, 0xFE, b'h', 0, b'i', 0].to_vec();
        assert_eq!(decode(&utf16).0, "hi");
        assert!(!looks_binary(&utf16));
        assert_eq!(decode(b"caf\xE9").0, "café");
        assert!(looks_binary(b"MZ\x00\x01"));
        assert!(!looks_binary(b"plain text"));
    }
}
