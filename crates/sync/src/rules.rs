//! A folder's ignore files outside the walk (docs/06 change 5).
//!
//! The walker lets the `ignore` crate apply `.seleneignore`, `.ignore`, `.gitignore` and
//! `.git/info/exclude` while it walks. Two places need the same rules without a walk:
//!
//! * **Built-in defaults** ([`crate::filter`]: `node_modules`, `*.min.js`, `*.jsonl`, …) can be
//!   re-included by a `!pattern` in a folder's `.seleneignore` (and only there: a `.gitignore`
//!   whitelist keeps today's behaviour). [`FolderRules::reincluded`] answers that per path. The
//!   sandbox and the secret denylist are checked before any of this and can never be re-included.
//! * **Watcher events** ([`crate::reconcile`]): a saved file is checked against the same ignore
//!   files the walk honours ([`FolderRules::ignored`]), so a file in a git-ignored directory is no
//!   longer indexed by its event and swept again by the next scan (docs/06 §2.4 gap).
//! * **Cargo packages** ([`FolderRules::cargo_source`]): the built-in defaults treat `build`, `out`,
//!   `env`, … as output directories. Cargo writes only to `target/`, so inside a package's sources
//!   those names are code (build-script modules in `build/`, `src/env/mod.rs`).
//!
//! Precedence follows the `ignore` crate: `.seleneignore` over `.ignore` over `.gitignore` over
//! `.git/info/exclude`; within one kind the deepest directory's file wins; within one file the
//! last matching line wins. Matching is case-insensitive (Windows paths). Ignore files are read
//! lazily, once per directory, and cached until the next scan builds a fresh [`FolderRules`]
//! (an edited ignore file triggers that scan).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// Selene Retrieval's own ignore file (gitignore syntax, nested files allowed).
pub const SELENEIGNORE: &str = ".seleneignore";

/// Ignore files whose change re-applies a folder's rules (a scan).
pub const IGNORE_FILE_NAMES: [&str; 3] = [SELENEIGNORE, ".ignore", ".gitignore"];

/// The directory (root-relative, `/`-separated, `""` = the root) whose [`CARGO_MANIFEST`] the
/// path `rel` (any separator) names, if it names one. Case-insensitive, like Windows.
pub fn cargo_manifest_dir(rel: &str) -> Option<String> {
    let rel = rel.replace('\\', "/");
    let rel = rel.trim_matches('/');
    let (dir, name) = rel.rsplit_once('/').unwrap_or(("", rel));
    name.eq_ignore_ascii_case(CARGO_MANIFEST)
        .then(|| dir.to_string())
}

/// Whether `rel` (root-relative, `/`-separated) names an ignore file.
pub fn is_ignore_file(rel: &str) -> bool {
    let name = rel.rsplit(['/', '\\']).next().unwrap_or(rel);
    IGNORE_FILE_NAMES
        .iter()
        .any(|n| n.eq_ignore_ascii_case(name))
}

/// Verdict of the ignore files for one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    None,
    Ignore,
    Whitelist,
}

fn verdict<T>(m: Match<T>) -> Verdict {
    match m {
        Match::None => Verdict::None,
        Match::Ignore(_) => Verdict::Ignore,
        Match::Whitelist(_) => Verdict::Whitelist,
    }
}

/// What one directory contributes: its ignore files, and whether it is a Cargo package root.
#[derive(Debug, Default)]
struct DirRules {
    selene: Option<Gitignore>,
    ignore: Option<Gitignore>,
    git: Option<Gitignore>,
    /// The directory holds a [`CARGO_MANIFEST`].
    cargo: bool,
}

/// Cargo's package manifest.
pub const CARGO_MANIFEST: &str = "Cargo.toml";

/// The directories of a Cargo package that hold its sources (`build` = the build script's own
/// modules next to `build.rs`; Cargo's output goes to `target/`).
const CARGO_SOURCE_ROOTS: [&str; 5] = ["src", "build", "tests", "benches", "examples"];

/// One folder's ignore rules (cheap to share; built per scan, cached for watcher events).
#[derive(Debug)]
pub struct FolderRules {
    root: PathBuf,
    dirs: Mutex<HashMap<String, Arc<DirRules>>>,
    exclude: Option<Gitignore>,
}

/// Parses one ignore file; `None` when it is missing, unreadable or empty. Lines that don't
/// parse are skipped (the `ignore` crate's partial result), like the walker does.
fn load(dir: &Path, path: &Path) -> Option<Gitignore> {
    if !path.is_file() {
        return None;
    }
    let mut b = GitignoreBuilder::new(dir);
    let _ = b.case_insensitive(true);
    if let Some(e) = b.add(path) {
        tracing::debug!(error = %e, "ignore file: some lines were skipped");
    }
    b.build().ok().filter(|g| !g.is_empty())
}

/// `C:\root` + `a/b` with one separator style (the matchers strip their directory by bytes).
fn native(root: &Path, rel: &str) -> PathBuf {
    if rel.is_empty() {
        root.to_path_buf()
    } else {
        #[cfg(windows)]
        let rel = rel.replace('/', "\\");
        root.join(rel)
    }
}

impl FolderRules {
    /// Rules of the folder rooted at `root` (the canonical root of the scan).
    pub fn new(root: &Path) -> Self {
        let git = root.join(".git");
        Self {
            root: root.to_path_buf(),
            dirs: Mutex::new(HashMap::new()),
            exclude: load(root, &git.join("info").join("exclude")),
        }
    }

    fn dir(&self, rel_dir: &str) -> Arc<DirRules> {
        let key = rel_dir.to_lowercase();
        if let Some(d) = self
            .dirs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
        {
            return Arc::clone(d);
        }
        let abs = native(&self.root, rel_dir);
        let d = Arc::new(DirRules {
            selene: load(&abs, &abs.join(SELENEIGNORE)),
            ignore: load(&abs, &abs.join(".ignore")),
            git: load(&abs, &abs.join(".gitignore")),
            cargo: abs.join(CARGO_MANIFEST).is_file(),
        });
        self.dirs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, Arc::clone(&d));
        d
    }

    /// The directories whose ignore files apply to `rel`, deepest first (`""` = the root).
    fn ancestors(rel: &str) -> Vec<&str> {
        let mut out = vec![""];
        for (i, ch) in rel.char_indices() {
            if ch == '/' {
                out.push(&rel[..i]);
            }
        }
        out.reverse();
        out
    }

    /// One kind of ignore file's verdict for `rel` itself (the deepest file that matches).
    fn kind(&self, rel: &str, is_dir: bool, pick: fn(&DirRules) -> Option<&Gitignore>) -> Verdict {
        let abs = native(&self.root, rel);
        for dir in Self::ancestors(rel) {
            let d = self.dir(dir);
            if let Some(g) = pick(&d) {
                let v = verdict(g.matched(&abs, is_dir));
                if v != Verdict::None {
                    return v;
                }
            }
        }
        Verdict::None
    }

    /// The ignore files' verdict for `rel` itself (not its parents), in the crate's precedence.
    fn own(&self, rel: &str, is_dir: bool) -> Verdict {
        for pick in [
            (|d: &DirRules| d.selene.as_ref()) as fn(&DirRules) -> Option<&Gitignore>,
            |d: &DirRules| d.ignore.as_ref(),
            |d: &DirRules| d.git.as_ref(),
        ] {
            let v = self.kind(rel, is_dir, pick);
            if v != Verdict::None {
                return v;
            }
        }
        match &self.exclude {
            Some(g) => verdict(g.matched(native(&self.root, rel), is_dir)),
            None => Verdict::None,
        }
    }

    /// A `!pattern` in a `.seleneignore` re-includes `rel` (lifts the built-in defaults).
    pub fn reincluded(&self, rel: &str, is_dir: bool) -> bool {
        self.kind(rel, is_dir, |d| d.selene.as_ref()) == Verdict::Whitelist
    }

    /// Directory `rel` (root-relative, `/`-separated) is part of a Cargo package's sources: it is a
    /// package root itself, or the nearest package root above it (at most the folder root, never a
    /// directory outside the folder) reaches it through `src`, `build`, `tests`, `benches` or
    /// `examples`. A `dist/`, `target/` or `coverage/` next to the `Cargo.toml` is not (Trunk,
    /// Cargo and coverage tools write there).
    pub fn cargo_source(&self, rel: &str) -> bool {
        let comps: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
        if comps.is_empty() {
            return false;
        }
        if self.dir(&comps.join("/")).cargo {
            return true;
        }
        for k in (0..comps.len()).rev() {
            if self.dir(&comps[..k].join("/")).cargo {
                return CARGO_SOURCE_ROOTS
                    .iter()
                    .any(|s| s.eq_ignore_ascii_case(comps[k]));
            }
        }
        false
    }

    /// The `Cargo.toml` of directory `rel_dir` appeared or went away since these rules looked at
    /// that directory, so [`Self::cargo_source`] answers may be stale. A directory they never
    /// looked at has nothing stale.
    pub fn cargo_changed(&self, rel_dir: &str) -> bool {
        let cached = self
            .dirs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&rel_dir.to_lowercase())
            .map(|d| d.cargo);
        cached.is_some_and(|was| was != native(&self.root, rel_dir).join(CARGO_MANIFEST).is_file())
    }

    /// The ignore files exclude `rel` or a directory above it: what the walk would skip.
    pub fn ignored(&self, rel: &str, is_dir: bool) -> bool {
        let rel = rel.trim_matches('/');
        if rel.is_empty() {
            return false;
        }
        for (i, ch) in rel.char_indices() {
            if ch == '/' && self.own(&rel[..i], true) == Verdict::Ignore {
                return true;
            }
        }
        self.own(rel, is_dir) == Verdict::Ignore
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(files: &[(&str, &str)]) -> (tempfile::TempDir, FolderRules) {
        let t = tempfile::tempdir().unwrap();
        for (p, body) in files {
            let p = t.path().join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        let r = FolderRules::new(t.path());
        (t, r)
    }

    #[test]
    fn precedence_and_parents() {
        let (_t, r) = rules(&[
            (".gitignore", "build/*\nout/\n*.tmp\n!keep.tmp\n"),
            (".seleneignore", "!build/docs/\nnotes/private/\n"),
            ("sub/.gitignore", "local.md\n"),
            (".git/info/exclude", "scratch/\n"),
        ]);
        assert!(r.ignored("out", true));
        assert!(
            r.ignored("out/docs/a.md", false),
            "parent directory ignored"
        );
        assert!(r.ignored("build/out.js", false));
        // `build/*` leaves the directory walkable, so `.seleneignore` can re-include inside it
        // (git's rule: nothing below an excluded directory can be re-included).
        assert!(
            !r.ignored("build/docs", true),
            ".seleneignore beats .gitignore"
        );
        assert!(!r.ignored("build/docs/a.md", false));
        assert!(r.ignored("a/b.tmp", false));
        assert!(!r.ignored("a/keep.tmp", false), "last line wins");
        assert!(r.ignored("sub/local.md", false));
        assert!(
            !r.ignored("local.md", false),
            "nested file applies below it only"
        );
        assert!(r.ignored("notes/private/x.md", false));
        assert!(
            r.ignored("Scratch/x.md", false),
            "git exclude, case-insensitive"
        );
        assert!(!r.ignored("src/main.rs", false));
        assert!(r.reincluded("build/docs", true));
        assert!(!r.reincluded("build", true));
        assert!(is_ignore_file("a/.SeleneIgnore") && is_ignore_file(".gitignore"));
        assert!(!is_ignore_file("a/gitignore.md"));
    }

    /// The re-include pattern of this repo's `.seleneignore` (only the patched files of a
    /// vendored crate): contents excluded with `/**`, directories re-included, then the files.
    #[test]
    fn re_include_single_files_below_an_excluded_tree() {
        let (_t, r) = rules(&[(
            ".seleneignore",
            "vendor-crate/**\n!vendor-crate/**/\n!vendor-crate/src/table/merge.rs\n",
        )]);
        assert!(!r.ignored("vendor-crate/src", true));
        assert!(!r.ignored("vendor-crate/src/table", true));
        assert!(!r.ignored("vendor-crate/src/table/merge.rs", false));
        assert!(r.ignored("vendor-crate/src/table/other.rs", false));
        assert!(r.ignored("vendor-crate/README.md", false));
        assert!(r.reincluded("vendor-crate/src/table/merge.rs", false));
    }

    /// The nearest `Cargo.toml` decides; only its source directories count, and a JavaScript app
    /// inside the workspace keeps its own output directories.
    #[test]
    fn cargo_sources_are_found_through_the_nearest_manifest() {
        let (t, r) = rules(&[
            ("Cargo.toml", "[workspace]\n"),
            ("crates/common/Cargo.toml", "[package]\n"),
            ("crates/common/build/static_crt.rs", "x"),
            ("crates/build/Cargo.toml", "[package]\n"),
            ("ui/package.json", "{}"),
        ]);
        for d in [
            "crates/common/build",
            "crates/common/src/env",
            "crates/common/src/a/out",
            "crates/common/tests/out",
            "crates/common/build/tmp",
            "crates/build",
            "src/target",
            "benches/temp",
        ] {
            assert!(r.cargo_source(d), "{d}");
        }
        for d in [
            "",
            "dist",
            "target",
            "env",
            "crates/common/target",
            "crates/common/dist",
            "crates/common/docs/build",
            "crates/build/target",
            "ui/dist",
            "ui/src/build",
        ] {
            assert!(!r.cargo_source(d), "{d}");
        }
        // Without any `Cargo.toml` nothing is a Cargo source.
        let (_t2, plain) = rules(&[("src/main.rs", "x"), ("build/a.md", "x")]);
        assert!(!plain.cargo_source("build"));
        assert!(!plain.cargo_source("src/env"));

        // A manifest that appears in a directory the rules looked at makes them stale.
        assert!(!r.cargo_changed("ui") && !r.cargo_changed("crates/common"));
        std::fs::write(t.path().join("ui").join("Cargo.toml"), "[package]\n").unwrap();
        assert!(r.cargo_changed("ui"));
        assert!(!r.cargo_changed("never/looked/at"));
        assert_eq!(cargo_manifest_dir("Cargo.toml").as_deref(), Some(""));
        assert_eq!(
            cargo_manifest_dir(r"crates\x\CARGO.TOML").as_deref(),
            Some("crates/x")
        );
        assert_eq!(cargo_manifest_dir("crates/x/Cargo.lock"), None);
        assert_eq!(cargo_manifest_dir("crates/Cargo.toml.bak"), None);
    }
}
