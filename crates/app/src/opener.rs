//! The two things the app may open outside itself (OQ-40, page 21 "Where do I get a key?"),
//! both through Windows Explorer started by absolute path (no shell, no new capability, no
//! unsafe code):
//!
//! * [`open_link`]: an `https://` page, **only** if it is one of the provider list's key pages
//!   (the WebView can't open arbitrary addresses);
//! * [`show_in_folder`]: selects a file in Explorer (`/select,`), **only** for an existing file
//!   inside a folder the user added. Selecting never runs or opens the file, so a script or
//!   program among the search results can't be started from the app.
//!
//! Opening a file in its default app ("Open" on a search result) is deliberately not offered: it
//! would run `.bat`/`.ps1`/`.js` files with their default handler. That needs the opener-plugin
//! security review OQ-40 asks for.
//!
//! **macOS (port phase 1).** The same checks apply; the launcher is `/usr/bin/open` by absolute
//! path. A link is `open <url>`, "show in folder" is `open -R` (reveals, never opens), and a folder
//! is `open <dir>`, or `open -R` when the folder could be an app bundle, so nothing is launched.
//! Paths compare case-sensitively with `/` separators.
//!
//! **M5-C (the "Sign in opened my Documents folder" bug).** The removed OAuth sign-in handed
//! Explorer its 379-character `https://claude.ai/oauth/authorize?...` address. `explorer.exe`
//! parses its argument as a location of at most `MAX_PATH` (260) characters, with `,` as its own
//! switch separator; anything it can't parse is dropped silently and Explorer opens its default
//! folder instead (Documents / Home). The same happens for an empty or relative argument. Every
//! launch now goes through [`explorer_arg_ok`]: a target that isn't an absolute `https://`
//! address or an absolute path, is empty, longer than Explorer can parse or contains `,` is
//! refused, and Explorer is never started without one. The sign-in itself no longer opens any
//! address from the app (the CLI's own sign-in opens the browser).

use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

/// Why nothing was opened (no path or address is echoed back).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenError {
    /// Not on the allow list / not inside an added folder / not an existing file.
    NotAllowed,
    /// Explorer couldn't be started.
    Failed,
}

/// `C:\Windows\explorer.exe` (the parent of System32), never a PATH lookup.
#[cfg(windows)]
fn explorer() -> Option<PathBuf> {
    let sys = sr_client::sys::system_dir().ok()?;
    let exe = sys.parent()?.join("explorer.exe");
    exe.is_file().then_some(exe)
}

/// macOS: `/usr/bin/open`, never a PATH lookup.
#[cfg(not(windows))]
fn explorer() -> Option<PathBuf> {
    let exe = PathBuf::from("/usr/bin/open");
    exe.is_file().then_some(exe)
}

/// Path separator used by [`norm`].
#[cfg(windows)]
const SEP: char = '\\';
#[cfg(not(windows))]
const SEP: char = '/';

/// A plain `https://host/...` address: no credentials, no spaces or quotes, sane length.
pub fn is_plain_https(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    url.len() <= 2048
        && !host.is_empty()
        && !host.contains('@')
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == ':')
        && url
            .chars()
            .all(|c| c.is_ascii_graphic() && c != '"' && c != '\\')
}

/// Longest argument Explorer parses as a location (`MAX_PATH` minus the terminator). Longer
/// ones make it open its default folder instead.
pub const EXPLORER_MAX_ARG: usize = 259;

/// Whether `arg` is something Explorer will open as given: an absolute `https://` address
/// ([`is_plain_https`]) or an absolute path without `..`, not empty, no longer than
/// [`EXPLORER_MAX_ARG`], without `,` (Explorer's switch separator) or `"`. Anything else would
/// make Explorer silently open its default folder.
pub fn explorer_arg_ok(arg: &str) -> bool {
    if arg.trim().is_empty()
        || arg.trim() != arg
        || arg.len() > EXPLORER_MAX_ARG
        || arg.contains([',', '"'])
    {
        return false;
    }
    if arg.starts_with("https://") {
        return is_plain_https(arg);
    }
    let p = Path::new(arg);
    p.is_absolute() && !p.components().any(|c| matches!(c, Component::ParentDir))
}

/// Starts Explorer with exactly one argument that passed [`explorer_arg_ok`].
fn launch_explorer(arg: &str) -> Result<(), OpenError> {
    if !explorer_arg_ok(arg) {
        return Err(OpenError::NotAllowed);
    }
    let exe = explorer().ok_or(OpenError::Failed)?;
    let mut cmd = Command::new(exe);
    #[cfg(not(windows))]
    cmd.arg("--"); // `open`: everything after is a target, never an option
    cmd.arg(arg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|_| OpenError::Failed)
}

/// Opens `url` in the default browser if it is on `allowed` (exact match).
pub fn open_link(url: &str, allowed: &[String]) -> Result<(), OpenError> {
    if !is_plain_https(url) || !allowed.iter().any(|a| a == url) {
        return Err(OpenError::NotAllowed);
    }
    launch_explorer(url)
}

/// Lower-cased, `\`-separated, without a trailing separator (Windows paths compare
/// case-insensitively).
#[cfg(windows)]
fn norm(p: &Path) -> String {
    let s = p.to_string_lossy().replace('/', "\\").to_lowercase();
    let s = s.strip_prefix(r"\\?\").map(str::to_owned).unwrap_or(s);
    s.trim_end_matches('\\').to_owned()
}

/// As given, without a trailing `/` (unix paths compare case-sensitively; on a case-insensitive
/// volume a differently-cased path is refused, which fails closed).
#[cfg(not(windows))]
fn norm(p: &Path) -> String {
    let s = p.to_string_lossy();
    let t = s.trim_end_matches('/');
    if t.is_empty() {
        s.into_owned()
    } else {
        t.to_owned()
    }
}

/// `path` is absolute, has no `..`, and lies inside one of `roots` (the added folders).
pub fn inside_added_folder(path: &Path, roots: &[PathBuf]) -> bool {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return false;
    }
    let p = norm(path);
    roots.iter().any(|r| {
        let r = norm(r);
        !r.is_empty() && p.len() > r.len() && p.starts_with(&r) && p[r.len()..].starts_with(SEP)
    })
}

/// Selects `path` in a new Explorer window, if it is an existing file inside an added folder.
pub fn show_in_folder(path: &Path, roots: &[PathBuf]) -> Result<(), OpenError> {
    if !inside_added_folder(path, roots) || !path.is_file() {
        return Err(OpenError::NotAllowed);
    }
    // Also check where the path really points (a junction inside the folder could lead out).
    let real = std::fs::canonicalize(path).map_err(|_| OpenError::NotAllowed)?;
    let real_roots: Vec<PathBuf> = roots
        .iter()
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .collect();
    if !inside_added_folder(&real, &real_roots) {
        return Err(OpenError::NotAllowed);
    }
    let s = path.to_string_lossy();
    // Never empty or relative (checked above: an existing absolute file); quoted below, so `,`
    // and long paths are fine here.
    if s.is_empty() || s.contains('"') {
        return Err(OpenError::NotAllowed);
    }
    let exe = explorer().ok_or(OpenError::Failed)?;
    let mut cmd = Command::new(exe);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Explorer parses its own command line: `/select,"<path>"` must reach it unquoted as a
        // whole (a quoted `"/select,…"` is not understood).
        cmd.raw_arg(format!("/select,\"{s}\""));
    }
    // `open -R` reveals the file in Finder; it never opens or runs it.
    #[cfg(not(windows))]
    cmd.arg("-R").arg("--").arg(path);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|_| OpenError::Failed)
}

/// `path` is one of `roots` itself or inside one (same rules as [`inside_added_folder`]).
pub fn is_or_inside_added_folder(path: &Path, roots: &[PathBuf]) -> bool {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return false;
    }
    let p = norm(path);
    roots.iter().any(|r| {
        let r = norm(r);
        !r.is_empty() && (p == r || (p.starts_with(&r) && p[r.len()..].starts_with(SEP)))
    })
}

/// Amendment 13 [Open folder]: opens an added folder (or a folder inside one, e.g. the default
/// folder inside an added Documents) in a new Explorer window. Only an existing **directory**,
/// re-checked after canonicalisation (a junction can't lead out); opening a folder never runs
/// anything. Longer than Explorer can parse → refused (it would open Documents instead, M5-C).
pub fn open_folder(path: &Path, roots: &[PathBuf]) -> Result<(), OpenError> {
    if !is_or_inside_added_folder(path, roots) || !path.is_dir() {
        return Err(OpenError::NotAllowed);
    }
    let real = std::fs::canonicalize(path).map_err(|_| OpenError::NotAllowed)?;
    let real_roots: Vec<PathBuf> = roots
        .iter()
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .collect();
    if !is_or_inside_added_folder(&real, &real_roots) {
        return Err(OpenError::NotAllowed);
    }
    let s = path.to_string_lossy();
    if s.is_empty() || s.contains('"') || (cfg!(windows) && s.len() > EXPLORER_MAX_ARG) {
        return Err(OpenError::NotAllowed);
    }
    let exe = explorer().ok_or(OpenError::Failed)?;
    let mut cmd = Command::new(exe);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Quoted as one argument, so spaces and `,` stay part of the path.
        cmd.raw_arg(format!("\"{s}\""));
    }
    // `open <dir>` would launch a folder that is an app bundle (`X.app`, a folder with a
    // `Contents` folder or a flat bundle's `Info.plist`), so such a folder is only revealed
    // (`-R`), never opened. Checked on, and passed as, the resolved path: a link without an
    // extension can't hide a bundle behind it.
    #[cfg(not(windows))]
    {
        let maybe_bundle = real.extension().is_some()
            || real.join("Contents").is_dir()
            || real.join("Info.plist").exists();
        if maybe_bundle {
            cmd.arg("-R");
        }
        cmd.arg("--").arg(&real);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|_| OpenError::Failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn open_folder_only_for_added_folders_and_their_subfolders() {
        let roots = vec![PathBuf::from(r"C:\Users\Ana\Documents")];
        assert!(is_or_inside_added_folder(
            Path::new(r"C:\Users\Ana\Documents"),
            &roots
        ));
        assert!(is_or_inside_added_folder(
            Path::new(r"c:\users\ana\documents\Selene Retrieval\"),
            &roots
        ));
        assert!(!is_or_inside_added_folder(
            Path::new(r"C:\Users\Ana"),
            &roots
        ));
        assert!(!is_or_inside_added_folder(
            Path::new(r"C:\Users\Ana\DocumentsOld"),
            &roots
        ));
        assert!(!is_or_inside_added_folder(
            Path::new(r"C:\Users\Ana\Documents\..\Desktop"),
            &roots
        ));
        assert!(!is_or_inside_added_folder(Path::new("Documents"), &roots));
        assert!(!is_or_inside_added_folder(Path::new(r"C:\Windows"), &roots));
        // Not added / not a directory / missing: never opened.
        let d = std::env::temp_dir();
        assert_eq!(open_folder(&d, &roots), Err(OpenError::NotAllowed));
        assert_eq!(
            open_folder(Path::new(r"C:\Users\Ana\Documents\missing"), &roots),
            Err(OpenError::NotAllowed)
        );
        let exe = std::env::current_exe().unwrap_or_default();
        let parent = exe.parent().map(Path::to_path_buf).unwrap_or_default();
        assert_eq!(
            open_folder(&exe, &[parent]),
            Err(OpenError::NotAllowed),
            "a file isn't a folder"
        );
    }

    #[test]
    fn only_plain_https_addresses() {
        assert!(is_plain_https("https://console.example.com/settings/keys"));
        assert!(!is_plain_https("http://example.com"));
        assert!(!is_plain_https("file:///C:/Windows/System32/calc.exe"));
        assert!(!is_plain_https("https://user:pw@example.com/"));
        assert!(!is_plain_https("https://example.com/a b"));
        assert!(!is_plain_https("https://example.com/\"&calc"));
        assert!(!is_plain_https("https:///nohost"));
    }

    /// M5-C: the "Sign in opened my Documents folder" bug. Explorer opens its default folder for
    /// any argument it can't parse; such arguments are refused before Explorer is started.
    #[test]
    fn explorer_never_gets_an_argument_it_would_turn_into_documents() {
        // The address the removed OAuth sign-in handed to Explorer (379 characters).
        let oauth = format!(
            "https://claude.ai/oauth/authorize?code=true&client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e&response_type=code&redirect_uri=https%3A%2F%2Fconsole.anthropic.com%2Foauth%2Fcode%2Fcallback&scope=org%3Acreate_api_key+user%3Aprofile+user%3Ainference&code_challenge={}&code_challenge_method=S256&state={}",
            "A".repeat(43),
            "B".repeat(43)
        );
        assert_eq!(oauth.len(), 379);
        assert!(is_plain_https(&oauth), "it was a valid https address");
        assert!(!explorer_arg_ok(&oauth), "but too long for Explorer");
        for bad in [
            "",
            " ",
            " https://claude.com/claude-code",
            "Documents",
            r"..\x",
            r"C:\Users\Ana\..\Public",
            "claude.com/claude-code",
            "https://example.com/a,b",
            "http://claude.com/",
            "file:///C:/Windows",
            "shell:Personal",
        ] {
            assert!(!explorer_arg_ok(bad), "{bad:?}");
        }
        assert!(explorer_arg_ok("https://claude.com/claude-code"));
        assert!(explorer_arg_ok("https://developers.openai.com/codex/cli"));
        #[cfg(windows)]
        assert!(explorer_arg_ok(r"C:\Users\Ana\Documents\a.txt"));
        #[cfg(not(windows))]
        assert!(explorer_arg_ok("/Users/ana/Documents/a.txt"));
        // Empty / relative / over-long links never reach Explorer, even if "allowed".
        let allowed = vec![String::new(), "Documents".to_owned(), oauth.clone()];
        for u in ["", "Documents", oauth.as_str()] {
            assert_eq!(open_link(u, &allowed), Err(OpenError::NotAllowed), "{u:?}");
        }
        assert_eq!(launch_explorer(""), Err(OpenError::NotAllowed));
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_paths_compare_exactly_and_stay_inside_added_folders() {
        let roots = vec![PathBuf::from("/Users/ana/Documents")];
        assert!(is_or_inside_added_folder(
            Path::new("/Users/ana/Documents"),
            &roots
        ));
        assert!(is_or_inside_added_folder(
            Path::new("/Users/ana/Documents/"),
            &roots
        ));
        assert!(inside_added_folder(
            Path::new("/Users/ana/Documents/a/b.txt"),
            &roots
        ));
        assert!(!inside_added_folder(
            Path::new("/Users/ana/Documents"),
            &roots
        ));
        assert!(!inside_added_folder(
            Path::new("/Users/ana/DocumentsOld/x.txt"),
            &roots
        ));
        assert!(!inside_added_folder(
            Path::new("/Users/ana/documents/x.txt"),
            &roots
        ));
        assert!(!inside_added_folder(
            Path::new("/Users/ana/Documents/../x.txt"),
            &roots
        ));
        assert!(!inside_added_folder(Path::new("Documents/x.txt"), &roots));
        assert!(!is_or_inside_added_folder(Path::new("/"), &roots));
        assert_eq!(
            show_in_folder(Path::new("/Users/ana/Documents/missing.txt"), &roots),
            Err(OpenError::NotAllowed)
        );
        let d = std::env::temp_dir();
        assert_eq!(open_folder(&d, &roots), Err(OpenError::NotAllowed));
    }

    #[test]
    fn links_must_be_on_the_allow_list() {
        let allowed = vec!["https://keys.example.com/".to_owned()];
        assert_eq!(
            open_link("https://evil.example.com/", &allowed),
            Err(OpenError::NotAllowed)
        );
        assert_eq!(
            open_link("ms-settings:privacy", &allowed),
            Err(OpenError::NotAllowed)
        );
    }

    #[cfg(windows)]
    #[test]
    fn show_in_folder_stays_inside_added_folders() {
        let roots = vec![PathBuf::from(r"C:\Users\Ana\Documents")];
        assert!(inside_added_folder(
            Path::new(r"C:\Users\Ana\Documents\a\b.txt"),
            &roots
        ));
        assert!(inside_added_folder(
            Path::new(r"c:\users\ana\documents\B.TXT"),
            &roots
        ));
        assert!(!inside_added_folder(
            Path::new(r"C:\Users\Ana\Documents"),
            &roots
        ));
        assert!(!inside_added_folder(
            Path::new(r"C:\Users\Ana\DocumentsOld\x.txt"),
            &roots
        ));
        assert!(!inside_added_folder(
            Path::new(r"C:\Users\Ana\Documents\..\secret.txt"),
            &roots
        ));
        assert!(!inside_added_folder(Path::new(r"Documents\x.txt"), &roots));
        assert!(!inside_added_folder(
            Path::new(r"C:\Windows\System32\calc.exe"),
            &roots
        ));
        let unc = vec![PathBuf::from(r"\\nas\team\handbook")];
        assert!(inside_added_folder(
            Path::new(r"\\nas\team\handbook\a.md"),
            &unc
        ));
        // A file that doesn't exist is never shown.
        assert_eq!(
            show_in_folder(Path::new(r"C:\Users\Ana\Documents\missing.txt"), &roots),
            Err(OpenError::NotAllowed)
        );
    }
}
