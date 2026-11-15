//! Path sandbox (docs/02-architecture.md §2.6 "Path traversal"; P1.7: always on, invisible).
//!
//! Every path that reaches the file system from a client (app-RPC `files.read` / `files.grep`,
//! an answer's `read_file` steps, the watcher, the walker) is resolved through [`Sandbox`]:
//!
//! 1. **Syntax** ([`validate_component`], [`parse_relative`], [`parse_absolute`]): reject `..` and
//!    `.`, ADS `:` streams, trailing dots/spaces, reserved device names by stem (`CON`,
//!    `con.txt`, `NUL.tar.gz`, `COM¹`, `CONIN$` ...), control and wildcard characters, drive-relative
//!    (`C:foo`) and root-relative (`\foo`) paths, and UNC / device / verbatim prefixes (`\\server`,
//!    `\\?\`, `\\.\`, `\??\`) unless the input is under an allowed root configured in that form.
//! 2. **Realpath containment** ([`Sandbox::resolve`]): canonicalise the longest existing ancestor
//!    (this expands 8.3 short names to long names and resolves junctions, symlinks and mount
//!    points), append the not-yet-existing tail, and require the result to be inside the
//!    canonical root (component-wise, case-insensitive). A junction or symlink inside a root that
//!    points outside is therefore refused.
//! 3. **Long paths**: roots are kept in their canonical verbatim (`\\?\C:\…`) form, so joined paths
//!    work beyond `MAX_PATH`. [`ResolvedPath::display_path`] gives the simplified form for UI.
//!
//! 4. **Policy**: every component of the resolved root-relative path is checked against the
//!    [`SecretFilter`] (after 8.3 expansion), and [`Sandbox::add_root`] refuses
//!    [`DangerousRoots`]. Neither can be turned off.
//!
//! Residual TOCTOU (a directory swapped for a junction between resolve and open) is closed in M1 by
//! `SandboxFs` opening handles relative to a cap-std `Dir` per root; this module is the policy.
//!
//! **Unix (macOS, Amendment 18)** keeps steps 2 and 4 and swaps the syntax for POSIX rules:
//! * Absolute means a leading `/`; components are split on `/` only. There are no drive, UNC,
//!   verbatim, device or 8.3 forms, so [`Violation::DriveRelative`] and
//!   [`Violation::ForeignPrefix`] never occur.
//! * `..` and `.` are refused as on Windows. `\` is an ordinary file name character, but a name
//!   that would read as `..`, `.` or an absolute path if `\` were taken as a separator is refused,
//!   so no consumer can turn it into a traversal.
//! * The Win32 aliasing rules do not apply: `CON`, `a:b`, `file.`, `a?b` are plain names. The
//!   macOS resource-fork path `..namedfork` is refused as [`Violation::AlternateDataStream`].
//! * Containment and the catalog [`path_key`] compare exact bytes (no case folding), so a
//!   sibling `/Vol/docs` is never inside root `/Vol/Docs` on a case-sensitive volume. On the
//!   default case-insensitive APFS, `canonicalize` returns the on-disk case, so canonical paths
//!   still match; a path that does not exist yet keeps the case it was given, and a
//!   differently cased duplicate of it is refused or treated as a new name (fails closed).
//! * A dangling symbolic link is refused ([`Violation::OutsideRoot`]): its target is unknown,
//!   so a write through it could land outside the root.

use std::fmt;
use std::io;
#[cfg(windows)]
use std::path::Prefix;
use std::path::{Component, Path, PathBuf};

use crate::error::{Error, ErrorCode};

mod roots;
mod secrets;

pub use roots::DangerousRoots;
pub use secrets::SecretFilter;

/// Why a path was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Violation {
    Empty,
    /// A `..` component.
    ParentTraversal,
    /// A `.` component (refused to keep path keys canonical).
    CurrentDir,
    /// An absolute or root-relative path where a relative one was expected.
    NotRelative,
    /// A relative or drive-relative path where an absolute one was expected.
    NotAbsolute,
    /// `C:foo` (relative to the current directory of drive C).
    DriveRelative,
    /// UNC / `\\?\` / `\\.\` / `\??\` path not under an allowed root given in the same form.
    ForeignPrefix,
    /// `:` inside a component (NTFS alternate data stream).
    AlternateDataStream,
    /// Component ends with `.` or ` ` (Win32 strips these, so `secret.` aliases `secret`).
    TrailingDotOrSpace,
    /// Reserved device name (by stem: `CON`, `con.txt`, `LPT1.log`, ...).
    ReservedName,
    /// Control character or one of `<>"|?*`.
    InvalidChar,
    /// Component longer than 255 UTF-16 units.
    ComponentTooLong,
    /// Resolves (via junction, symlink, short name, ...) outside every allowed root.
    OutsideRoot,
    /// The root itself is unusable (missing, not a directory, device path).
    BadRoot,
    /// The folder is a drive root, contains private system or account settings, or is inside a
    /// protected location.
    DangerousRoot,
    /// The file or one of its folders is on the secret denylist (`.env`, `*.pem`, `.ssh/`, ...).
    SecretDenied,
    /// M5-E: the folder is outside the roots an administrator's policy allows.
    NotAllowedByPolicy,
}

impl Violation {
    /// Error code for the user-facing message.
    pub const fn code(self) -> ErrorCode {
        match self {
            Violation::ParentTraversal
            | Violation::NotRelative
            | Violation::NotAbsolute
            | Violation::DriveRelative
            | Violation::ForeignPrefix
            | Violation::OutsideRoot
            // Path forms that name something other than a plain file in the folder (a hidden
            // stream, a device, a Win32 alias): refused as "outside your folders", not as a
            // malformed request an agent would simply retry (M1 tester P2 finding).
            | Violation::AlternateDataStream
            | Violation::TrailingDotOrSpace
            | Violation::ReservedName => ErrorCode::PathOutsideSandbox,
            Violation::BadRoot => ErrorCode::FolderUnavailable,
            Violation::DangerousRoot => ErrorCode::PathRefused,
            Violation::SecretDenied => ErrorCode::PathDenied,
            Violation::NotAllowedByPolicy => ErrorCode::ManagedByPolicy,
            Violation::Empty
            | Violation::CurrentDir
            | Violation::InvalidChar
            | Violation::ComponentTooLong => ErrorCode::InvalidInput,
        }
    }
}

/// Sandbox failure: a policy violation or an I/O error while resolving.
#[derive(Debug)]
pub enum SandboxError {
    Violation(Violation),
    Io(io::Error),
}

impl SandboxError {
    pub fn violation(&self) -> Option<Violation> {
        match self {
            SandboxError::Violation(v) => Some(*v),
            SandboxError::Io(_) => None,
        }
    }
}

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SandboxError::Violation(v) => write!(f, "path refused: {v:?}"),
            SandboxError::Io(e) => write!(f, "path could not be resolved: {e}"),
        }
    }
}

impl std::error::Error for SandboxError {}

impl From<Violation> for SandboxError {
    fn from(v: Violation) -> Self {
        SandboxError::Violation(v)
    }
}

impl From<SandboxError> for Error {
    fn from(e: SandboxError) -> Self {
        match e {
            SandboxError::Violation(v) => Error::new(v.code()).with_detail(format!("{v:?}")),
            SandboxError::Io(io) => Error::from(io),
        }
    }
}

type SResult<T> = Result<T, SandboxError>;

const RESERVED: &[&str] = &[
    "CON",
    "PRN",
    "AUX",
    "NUL",
    "CONIN$",
    "CONOUT$",
    "CLOCK$",
    "COM0",
    "COM1",
    "COM2",
    "COM3",
    "COM4",
    "COM5",
    "COM6",
    "COM7",
    "COM8",
    "COM9",
    "COM\u{b9}",
    "COM\u{b2}",
    "COM\u{b3}",
    "LPT0",
    "LPT1",
    "LPT2",
    "LPT3",
    "LPT4",
    "LPT5",
    "LPT6",
    "LPT7",
    "LPT8",
    "LPT9",
    "LPT\u{b9}",
    "LPT\u{b2}",
    "LPT\u{b3}",
];

/// Whether `name` is a reserved device name, matched by stem (text before the first `.`, with
/// trailing spaces removed) case-insensitively: `CON`, `con.txt`, `Nul.tar.gz`, `COM1 .log`.
pub fn is_reserved_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    let upper = stem.to_uppercase();
    RESERVED.contains(&upper.as_str())
}

/// Validate one path component (a file or directory name).
pub fn validate_component(c: &str) -> Result<(), Violation> {
    if c.is_empty() {
        return Err(Violation::Empty);
    }
    if c == "." {
        return Err(Violation::CurrentDir);
    }
    if c == ".." {
        return Err(Violation::ParentTraversal);
    }
    platform_component_rules(c)?;
    if c.encode_utf16().count() > 255 {
        return Err(Violation::ComponentTooLong);
    }
    Ok(())
}

/// Win32 rules: ADS streams, wildcard characters, trailing dots/spaces, device names.
#[cfg(windows)]
fn platform_component_rules(c: &str) -> Result<(), Violation> {
    if c.contains(':') {
        return Err(Violation::AlternateDataStream);
    }
    if c.chars()
        .any(|ch| (ch as u32) < 0x20 || matches!(ch, '<' | '>' | '"' | '|' | '?' | '*'))
    {
        return Err(Violation::InvalidChar);
    }
    // Win32 strips trailing dots and spaces, so "a..", "...", "secret." alias other names.
    if c.ends_with('.') || c.ends_with(' ') {
        return Err(Violation::TrailingDotOrSpace);
    }
    if is_reserved_device_name(c) {
        return Err(Violation::ReservedName);
    }
    Ok(())
}

/// POSIX rules: every byte but `/` and NUL is legal, so only what changes meaning is refused.
#[cfg(not(windows))]
fn platform_component_rules(c: &str) -> Result<(), Violation> {
    // `file/..namedfork/rsrc` opens the resource fork on macOS: a hidden stream, like NTFS ADS.
    if c.eq_ignore_ascii_case("..namedfork") {
        return Err(Violation::AlternateDataStream);
    }
    if c.chars().any(|ch| (ch as u32) < 0x20) {
        return Err(Violation::InvalidChar);
    }
    // `\` is a plain character here. Refuse names that would become `..`, `.` or an absolute
    // path if anything ever read `\` as a separator.
    if c.starts_with('\\') {
        return Err(Violation::InvalidChar);
    }
    for piece in c.split('\\') {
        if piece == ".." {
            return Err(Violation::ParentTraversal);
        }
        if piece == "." {
            return Err(Violation::CurrentDir);
        }
    }
    Ok(())
}

#[cfg(windows)]
fn split_components(s: &str) -> impl Iterator<Item = &str> {
    s.split(['/', '\\'])
}

/// Unix: only `/` separates; `\` is part of a name.
#[cfg(not(windows))]
fn split_components(s: &str) -> impl Iterator<Item = &str> {
    s.split('/')
}

/// Parse a path relative to a root into validated components. `""` means the root itself.
#[cfg(windows)]
pub fn parse_relative(rel: &str) -> Result<Vec<String>, Violation> {
    if rel.is_empty() {
        return Ok(Vec::new());
    }
    if rel.starts_with(['/', '\\']) {
        return Err(Violation::NotRelative);
    }
    let b = rel.as_bytes();
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        return Err(if b.len() >= 3 && matches!(b[2], b'/' | b'\\') {
            Violation::NotRelative
        } else {
            Violation::DriveRelative
        });
    }
    let n = split_components(rel).count();
    let mut out = Vec::new();
    for (i, c) in split_components(rel).enumerate() {
        // A single trailing separator ("dir/") is tolerated; empty inner components are not.
        if c.is_empty() && i + 1 == n {
            continue;
        }
        validate_component(c)?;
        out.push(c.to_string());
    }
    Ok(out)
}

/// Parse a path relative to a root into validated components. `""` means the root itself.
#[cfg(not(windows))]
pub fn parse_relative(rel: &str) -> Result<Vec<String>, Violation> {
    if rel.is_empty() {
        return Ok(Vec::new());
    }
    if rel.starts_with('/') {
        return Err(Violation::NotRelative);
    }
    let n = split_components(rel).count();
    let mut out = Vec::new();
    for (i, c) in split_components(rel).enumerate() {
        // A single trailing separator ("dir/") is tolerated; empty inner components are not.
        if c.is_empty() && i + 1 == n {
            continue;
        }
        validate_component(c)?;
        out.push(c.to_string());
    }
    Ok(out)
}

/// Catalog path key of a root-relative path: `/`-separated and case-folded, so `Docs\A.md` and
/// `docs/a.md` collide in `UNIQUE(folder_id, path_key)` exactly as they do on NTFS.
#[cfg(windows)]
pub fn path_key(rel: &str) -> String {
    split_components(rel)
        .filter(|c| !c.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join("/")
}

/// Catalog path key of a root-relative path (Unix): `/`-separated, exact case, so `A.md` and
/// `a.md` on a case-sensitive volume are two files. On case-insensitive APFS the walker and
/// [`Sandbox::resolve_relative`] report the on-disk case, so one file keeps one key.
#[cfg(not(windows))]
pub fn path_key(rel: &str) -> String {
    split_components(rel)
        .filter(|c| !c.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

/// Form of an absolute input path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbsoluteForm {
    /// `C:\…` (Windows), or `/…` (Unix).
    Drive,
    /// `\\server\share\…`, `\\?\…`, `\\.\…`, `\??\…` — allowed only under a root given in that form.
    Special,
}

/// Syntactic check of an absolute path. Returns its form and the validated components after the
/// prefix (`C:\` or the special prefix and, for UNC, server and share).
#[cfg(windows)]
pub fn parse_absolute(path: &str) -> Result<(AbsoluteForm, Vec<String>), Violation> {
    if path.is_empty() {
        return Err(Violation::Empty);
    }
    let norm = path.replace('/', "\\");
    let b = norm.as_bytes();
    if norm.starts_with("\\\\") || norm.starts_with("\\??\\") {
        // Special forms: validate everything after the prefix kind marker, component by
        // component, except the leading "?", "." or "??" marker and a drive-letter / UNC marker.
        let rest = norm.trim_start_matches('\\');
        let mut comps: Vec<&str> = rest.split('\\').collect();
        if matches!(comps.first(), Some(&"?" | &"." | &"??")) {
            comps.remove(0);
            if let Some(first) = comps.first() {
                let fb = first.as_bytes();
                let drive = fb.len() == 2 && fb[1] == b':' && fb[0].is_ascii_alphabetic();
                if drive || first.eq_ignore_ascii_case("UNC") {
                    comps.remove(0);
                }
            }
        }
        let mut out = Vec::new();
        let n = comps.len();
        for (i, c) in comps.into_iter().enumerate() {
            if c.is_empty() && i + 1 == n {
                continue;
            }
            validate_component(c)?;
            out.push(c.to_string());
        }
        return Ok((AbsoluteForm::Special, out));
    }
    if norm.starts_with('\\') {
        return Err(Violation::NotAbsolute);
    }
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        if b.len() == 2 || b[2] != b'\\' {
            return Err(Violation::DriveRelative);
        }
        let comps = parse_relative(&norm[3..]).map_err(|v| match v {
            Violation::NotRelative => Violation::Empty, // `C:\\x`: empty component
            other => other,
        })?;
        return Ok((AbsoluteForm::Drive, comps));
    }
    Err(Violation::NotAbsolute)
}

/// Syntactic check of an absolute path (Unix): a leading `/`, then validated components split on
/// `/` only. The form is always [`AbsoluteForm::Drive`]; `//x` is an empty component.
#[cfg(not(windows))]
pub fn parse_absolute(path: &str) -> Result<(AbsoluteForm, Vec<String>), Violation> {
    if path.is_empty() {
        return Err(Violation::Empty);
    }
    let Some(rest) = path.strip_prefix('/') else {
        return Err(Violation::NotAbsolute);
    };
    let comps = parse_relative(rest).map_err(|v| match v {
        Violation::NotRelative => Violation::Empty, // `//x`: empty component
        other => other,
    })?;
    Ok((AbsoluteForm::Drive, comps))
}

/// How input paths are spelled before parsing: `\` on Windows, unchanged on Unix.
#[cfg(windows)]
fn normalize_input(s: &str) -> String {
    s.replace('/', "\\")
}

#[cfg(not(windows))]
fn normalize_input(s: &str) -> String {
    s.to_string()
}

/// Whether a user path looks absolute (else it is taken relative to the only root).
#[cfg(windows)]
fn looks_absolute(path: &str) -> bool {
    let b = path.as_bytes();
    path.starts_with(['/', '\\']) || (b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic())
}

#[cfg(not(windows))]
fn looks_absolute(path: &str) -> bool {
    path.starts_with('/')
}

/// Whether canonical `path` is `root` or inside it: case-insensitive on Windows (NTFS), exact
/// components on Unix (a case-sensitive volume has distinct `Docs` and `docs`).
#[cfg(windows)]
fn path_within_root(path: &Path, root: &Path) -> bool {
    path_starts_with_ci(path, root)
}

#[cfg(not(windows))]
fn path_within_root(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

/// Component-wise, case-insensitive `starts_with` on canonical paths.
#[cfg(windows)]
fn path_starts_with_ci(path: &Path, root: &Path) -> bool {
    let mut p = path.components();
    for r in root.components() {
        match p.next() {
            Some(pc) if eq_component(pc, r) => {}
            _ => return false,
        }
    }
    true
}

#[cfg(windows)]
fn eq_component(a: Component<'_>, b: Component<'_>) -> bool {
    match (a, b) {
        (Component::Prefix(x), Component::Prefix(y)) => prefix_eq(x.kind(), y.kind()),
        (Component::Normal(x), Component::Normal(y)) => {
            x.to_string_lossy().to_lowercase() == y.to_string_lossy().to_lowercase()
        }
        (x, y) => x == y,
    }
}

#[cfg(windows)]
fn prefix_eq(a: Prefix<'_>, b: Prefix<'_>) -> bool {
    use Prefix::*;
    let lc = |s: &std::ffi::OsStr| s.to_string_lossy().to_lowercase();
    match (a, b) {
        (VerbatimDisk(x) | Disk(x), VerbatimDisk(y) | Disk(y)) => x.eq_ignore_ascii_case(&y),
        (VerbatimUNC(s1, h1) | UNC(s1, h1), VerbatimUNC(s2, h2) | UNC(s2, h2)) => {
            lc(s1) == lc(s2) && lc(h1) == lc(h2)
        }
        (x, y) => x == y,
    }
}

/// Canonicalise the longest existing ancestor of `p` and re-append the missing tail.
fn realpath_existing_ancestor(p: &Path) -> SResult<PathBuf> {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p.to_path_buf();
    loop {
        match std::fs::canonicalize(&cur) {
            Ok(mut real) => {
                for t in tail.iter().rev() {
                    real.push(t);
                }
                return Ok(real);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Unix: a link whose target is missing also reports NotFound. Its target is
                // unknown, so it is never treated as a new name inside the root.
                #[cfg(not(windows))]
                if std::fs::symlink_metadata(&cur).is_ok() {
                    return Err(Violation::OutsideRoot.into());
                }
                let Some(name) = cur.file_name().map(ToOwned::to_owned) else {
                    return Err(SandboxError::Io(e));
                };
                tail.push(name);
                if !cur.pop() {
                    return Err(SandboxError::Io(e));
                }
            }
            Err(e) => return Err(SandboxError::Io(e)),
        }
    }
}

/// One allowed root.
#[derive(Debug, Clone)]
pub struct SandboxRoot {
    /// As given (picker / catalog), normalised to backslashes (Windows) or unchanged (Unix).
    pub configured: String,
    /// `std::fs::canonicalize` result (verbatim form, long names, links resolved).
    pub canonical: PathBuf,
    /// Whether `configured` is a UNC / verbatim / device form.
    special: bool,
}

impl SandboxRoot {
    #[cfg(windows)]
    fn textually_contains(&self, input_norm: &str) -> bool {
        let root = self.configured.trim_end_matches('\\').to_lowercase();
        let inp = input_norm.to_lowercase();
        inp == root || inp.starts_with(&format!("{root}\\"))
    }

    /// Unix has no special forms (`special` is always false); exact, `/`-separated.
    #[cfg(not(windows))]
    fn textually_contains(&self, input_norm: &str) -> bool {
        let root = self.configured.trim_end_matches('/');
        input_norm == root || input_norm.starts_with(&format!("{root}/"))
    }
}

/// A resolved, contained path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPath {
    /// Index of the containing root in [`Sandbox::roots`].
    pub root: usize,
    /// Canonical absolute path (verbatim form; safe beyond MAX_PATH).
    pub path: PathBuf,
    /// Path relative to the root, `/`-separated, as it exists on disk where it exists.
    pub rel: String,
}

impl ResolvedPath {
    /// Simplified (`C:\…`) form for display when it is representable.
    pub fn display_path(&self) -> PathBuf {
        dunce::simplified(&self.path).to_path_buf()
    }
}

/// The set of allowed roots plus the always-on policy.
#[derive(Debug, Clone)]
pub struct Sandbox {
    roots: Vec<SandboxRoot>,
    secrets: SecretFilter,
    dangerous: DangerousRoots,
}

impl Default for Sandbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Sandbox {
    /// Sandbox with the default secret denylist and the current user's protected locations.
    pub fn new() -> Self {
        Self::with_policy(DangerousRoots::from_env(None))
    }

    /// Sandbox with an explicit dangerous-root policy (e.g. one that includes the app data
    /// directory). The secret denylist is always the default and cannot be disabled.
    pub fn with_policy(dangerous: DangerousRoots) -> Self {
        Self {
            roots: Vec::new(),
            secrets: SecretFilter::default(),
            dangerous,
        }
    }

    pub fn secret_filter(&self) -> &SecretFilter {
        &self.secrets
    }

    pub fn roots(&self) -> &[SandboxRoot] {
        &self.roots
    }

    /// Add an allowed root. It must exist, be a directory, not be a [`DangerousRoots`] entry and
    /// not be (or be inside) a [`SecretFilter`] name such as `.ssh`. `\\.\` / `\??\` device paths
    /// and `\\?\` forms other than `\\?\X:\` and `\\?\UNC\` are refused.
    pub fn add_root(&mut self, root: impl AsRef<Path>) -> SResult<usize> {
        let raw = normalize_input(&root.as_ref().to_string_lossy());
        let (form, _) = parse_absolute(&raw)?;
        let special = form == AbsoluteForm::Special;
        if special {
            let lower = raw.to_lowercase();
            let ok_verbatim = lower.starts_with("\\\\?\\unc\\")
                || (lower.starts_with("\\\\?\\")
                    && lower.as_bytes().get(5) == Some(&b':')
                    && lower.as_bytes().get(4).is_some_and(u8::is_ascii_alphabetic));
            let is_unc = !lower.starts_with("\\\\?\\")
                && !lower.starts_with("\\\\.\\")
                && !lower.starts_with("\\??\\");
            if !(ok_verbatim || is_unc) {
                return Err(Violation::BadRoot.into());
            }
        }
        let canonical = std::fs::canonicalize(&raw).map_err(SandboxError::Io)?;
        if !canonical.is_dir() {
            return Err(Violation::BadRoot.into());
        }
        if self.dangerous.is_refused(&canonical) {
            return Err(Violation::DangerousRoot.into());
        }
        if !self.dangerous.is_allowed_by_policy(&canonical) {
            return Err(Violation::NotAllowedByPolicy.into());
        }
        // A root that is (or is inside) a denied name such as `.ssh` or `.aws` would expose
        // everything below it, because only root-relative components are checked on resolve.
        let denied = canonical.components().any(|c| match c {
            Component::Normal(n) => self.secrets.is_denied_name(&n.to_string_lossy()),
            _ => false,
        });
        if denied {
            return Err(Violation::SecretDenied.into());
        }
        self.roots.push(SandboxRoot {
            configured: raw,
            canonical,
            special,
        });
        Ok(self.roots.len() - 1)
    }

    /// Resolve `rel` (relative to root `root`) to a contained canonical path.
    pub fn resolve_relative(&self, root: usize, rel: &str) -> SResult<ResolvedPath> {
        let r = self.roots.get(root).ok_or(Violation::BadRoot)?;
        let comps = parse_relative(rel)?;
        let mut candidate = r.canonical.clone();
        for c in &comps {
            candidate.push(c);
        }
        self.contain(root, &candidate)
    }

    /// Resolve an absolute path; it must be inside one of the roots.
    pub fn resolve(&self, path: &str) -> SResult<ResolvedPath> {
        let norm = normalize_input(path);
        let (form, _) = parse_absolute(&norm)?;
        let mut last_err: SandboxError = Violation::OutsideRoot.into();
        for (i, r) in self.roots.iter().enumerate() {
            if form == AbsoluteForm::Special && !(r.special && r.textually_contains(&norm)) {
                last_err = Violation::ForeignPrefix.into();
                continue;
            }
            match self.contain(i, Path::new(&norm)) {
                Ok(p) => return Ok(p),
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }

    /// Resolve a user-supplied path: absolute (any root) or relative to the only root.
    pub fn resolve_any(&self, path: &str) -> SResult<ResolvedPath> {
        if looks_absolute(path) {
            return self.resolve(path);
        }
        match self.roots.len() {
            1 => self.resolve_relative(0, path),
            _ => Err(Violation::NotAbsolute.into()),
        }
    }

    fn contain(&self, root: usize, candidate: &Path) -> SResult<ResolvedPath> {
        let r = self.roots.get(root).ok_or(Violation::BadRoot)?;
        let real = realpath_existing_ancestor(candidate)?;
        if !path_within_root(&real, &r.canonical) {
            return Err(Violation::OutsideRoot.into());
        }
        // Re-validate what the file system resolved to (a junction target could be named "CON.").
        let rel_parts: Vec<String> = real
            .components()
            .skip(r.canonical.components().count())
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        for c in &rel_parts {
            validate_component(c)?;
            if self.secrets.is_denied_name(c) {
                return Err(Violation::SecretDenied.into());
            }
        }
        Ok(ResolvedPath {
            root,
            path: real,
            rel: rel_parts.join("/"),
        })
    }
}

#[cfg(test)]
#[cfg(windows)]
mod fs_tests;

#[cfg(test)]
#[cfg(unix)]
mod fs_tests_unix;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn path_keys_fold_case_and_separators() {
        assert_eq!(path_key(r"Docs\Sub/A.MD"), "docs/sub/a.md");
        assert_eq!(path_key("docs/sub/a.md/"), "docs/sub/a.md");
        assert_eq!(path_key(""), "");
    }

    #[test]
    fn violation_codes() {
        assert_eq!(Violation::SecretDenied.code(), ErrorCode::PathDenied);
        assert_eq!(Violation::DangerousRoot.code(), ErrorCode::PathRefused);
        assert_eq!(Violation::OutsideRoot.code(), ErrorCode::PathOutsideSandbox);
        assert_eq!(
            Violation::ReservedName.code(),
            ErrorCode::PathOutsideSandbox
        );
        assert_eq!(
            Violation::AlternateDataStream.code(),
            ErrorCode::PathOutsideSandbox
        );
        assert_eq!(
            Violation::TrailingDotOrSpace.code(),
            ErrorCode::PathOutsideSandbox
        );
        assert_eq!(Violation::InvalidChar.code(), ErrorCode::InvalidInput);
    }

    #[test]
    fn device_names_by_stem() {
        for n in [
            "CON",
            "con",
            "CON.txt",
            "nul.tar.gz",
            "COM1",
            "lpt9.log",
            "AUX ",
            "CON .txt",
            "COM\u{b9}",
            "conin$",
            "CONOUT$.x",
        ] {
            assert!(is_reserved_device_name(n), "{n}");
        }
        for n in [
            "CONSOLE",
            "con1",
            "COM10",
            "LPT",
            "nulls.txt",
            "icon.txt",
            "acon",
        ] {
            assert!(!is_reserved_device_name(n), "{n}");
        }
    }

    #[test]
    #[cfg(windows)]
    fn component_rules() {
        assert_eq!(
            validate_component("a:b"),
            Err(Violation::AlternateDataStream)
        );
        assert_eq!(validate_component("x."), Err(Violation::TrailingDotOrSpace));
        assert_eq!(validate_component("x "), Err(Violation::TrailingDotOrSpace));
        assert_eq!(
            validate_component("..."),
            Err(Violation::TrailingDotOrSpace)
        );
        assert_eq!(validate_component(".."), Err(Violation::ParentTraversal));
        assert_eq!(validate_component("a*"), Err(Violation::InvalidChar));
        assert_eq!(
            validate_component(&"a".repeat(256)),
            Err(Violation::ComponentTooLong)
        );
        assert!(validate_component(".gitignore").is_ok());
        assert!(validate_component("résumé (final).docx").is_ok());
    }

    #[test]
    #[cfg(windows)]
    fn absolute_forms() {
        assert_eq!(parse_absolute("C:foo"), Err(Violation::DriveRelative));
        assert_eq!(parse_absolute("\\foo"), Err(Violation::NotAbsolute));
        assert_eq!(parse_absolute("foo"), Err(Violation::NotAbsolute));
        assert!(matches!(
            parse_absolute("C:\\a\\b"),
            Ok((AbsoluteForm::Drive, _))
        ));
        assert!(matches!(
            parse_absolute("\\\\?\\C:\\a"),
            Ok((AbsoluteForm::Special, _))
        ));
        assert_eq!(
            parse_absolute("C:\\a\\..\\b"),
            Err(Violation::ParentTraversal)
        );
        assert_eq!(
            parse_absolute("\\\\?\\C:\\a\\..\\b"),
            Err(Violation::ParentTraversal)
        );
    }

    /// Unix keys keep case and `\`: `A.md` and `a.md` are two files on a case-sensitive volume.
    #[test]
    #[cfg(not(windows))]
    fn unix_path_keys_keep_case_and_backslashes() {
        assert_eq!(path_key("Docs/Sub/A.MD"), "Docs/Sub/A.MD");
        assert_ne!(path_key("A.md"), path_key("a.md"));
        assert_eq!(path_key(r"docs/a\b.md/"), r"docs/a\b.md");
        assert_eq!(path_key(""), "");
    }

    #[test]
    #[cfg(not(windows))]
    fn unix_component_rules() {
        // Win32-only rules do not apply: these are ordinary POSIX names.
        for ok in [
            "a:b",
            "x.",
            "x ",
            "...",
            "CON",
            "nul.txt",
            "a*",
            "a?b",
            "<x>",
            "a|b",
            r"a\b",
            r"a\",
            ".gitignore",
            "résumé (final).docx",
        ] {
            assert_eq!(validate_component(ok), Ok(()), "{ok}");
        }
        assert_eq!(validate_component(".."), Err(Violation::ParentTraversal));
        assert_eq!(validate_component("."), Err(Violation::CurrentDir));
        assert_eq!(validate_component(""), Err(Violation::Empty));
        assert_eq!(validate_component("a\nb"), Err(Violation::InvalidChar));
        assert_eq!(validate_component("a\0b"), Err(Violation::InvalidChar));
        assert_eq!(
            validate_component(&"a".repeat(256)),
            Err(Violation::ComponentTooLong)
        );
        // macOS resource fork access (`file/..namedfork/rsrc`).
        assert_eq!(
            validate_component("..namedfork"),
            Err(Violation::AlternateDataStream)
        );
        assert_eq!(
            validate_component("..NamedFork"),
            Err(Violation::AlternateDataStream)
        );
        // `\` never becomes traversal or an absolute path for a consumer that splits on it.
        for (bad, want) in [
            (r"..\x", Violation::ParentTraversal),
            (r"a\..", Violation::ParentTraversal),
            (r"a\..\..\b", Violation::ParentTraversal),
            (r".\x", Violation::CurrentDir),
            (r"\etc", Violation::InvalidChar),
            (r"\", Violation::InvalidChar),
        ] {
            assert_eq!(validate_component(bad), Err(want), "{bad}");
        }
    }

    #[test]
    #[cfg(not(windows))]
    fn unix_relative_and_absolute_forms() {
        assert_eq!(parse_relative("a/b/").unwrap(), vec!["a", "b"]);
        assert_eq!(parse_relative(r"a\b").unwrap(), vec![r"a\b"]);
        assert_eq!(parse_relative("C:foo").unwrap(), vec!["C:foo"]);
        assert_eq!(parse_relative("/etc"), Err(Violation::NotRelative));
        assert_eq!(parse_relative("a//b"), Err(Violation::Empty));
        assert_eq!(parse_relative("a/../b"), Err(Violation::ParentTraversal));
        assert_eq!(
            parse_relative(r"a\..\..\b"),
            Err(Violation::ParentTraversal)
        );
        assert_eq!(parse_relative("./a"), Err(Violation::CurrentDir));

        assert_eq!(
            parse_absolute("/Users/me/Docs"),
            Ok((
                AbsoluteForm::Drive,
                vec!["Users".into(), "me".into(), "Docs".into()]
            ))
        );
        assert_eq!(parse_absolute("/"), Ok((AbsoluteForm::Drive, vec![])));
        assert_eq!(parse_absolute(""), Err(Violation::Empty));
        assert_eq!(parse_absolute("Users/me"), Err(Violation::NotAbsolute));
        assert_eq!(parse_absolute(r"C:\a\b"), Err(Violation::NotAbsolute));
        assert_eq!(parse_absolute(r"\\?\C:\a"), Err(Violation::NotAbsolute));
        assert_eq!(parse_absolute("//server/share"), Err(Violation::Empty));
        assert_eq!(
            parse_absolute("/Users/me/../other"),
            Err(Violation::ParentTraversal)
        );
        assert_eq!(parse_absolute("/a/./b"), Err(Violation::CurrentDir));
        assert_eq!(parse_absolute("/a//b"), Err(Violation::Empty));
        assert!(looks_absolute("/x"));
        assert!(!looks_absolute(r"\x"));
        assert!(!looks_absolute(r"C:\x"));
    }
}
