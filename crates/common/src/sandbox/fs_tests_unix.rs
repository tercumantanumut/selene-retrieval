//! File-system tests for the sandbox on Unix (macOS first): symbolic links, `..`, case, `\` in
//! names, dangerous roots. Twin of `fs_tests.rs` (Windows).

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use super::*;

fn sandbox_for(root: &Path) -> Sandbox {
    let mut sb = Sandbox::with_policy(DangerousRoots::none());
    sb.add_root(root).unwrap();
    sb
}

fn violation<T: std::fmt::Debug>(r: SResult<T>) -> Option<Violation> {
    r.err().and_then(|e| e.violation())
}

/// Whether the volume holding `dir` tells `a` and `A` apart.
fn case_sensitive(dir: &Path) -> bool {
    let probe = dir.join("case-probe");
    fs::write(&probe, "x").unwrap();
    let sensitive = !dir.join("CASE-PROBE").exists();
    fs::remove_file(&probe).unwrap();
    sensitive
}

#[test]
fn paths_inside_the_root_resolve_to_canonical_paths() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("Docs")).unwrap();
    fs::write(dir.path().join("Docs").join("Report.md"), "x").unwrap();
    let sb = sandbox_for(dir.path());
    let canonical = fs::canonicalize(dir.path()).unwrap();
    assert_eq!(sb.roots()[0].canonical, canonical);

    let r = sb.resolve_relative(0, "Docs/Report.md").unwrap();
    assert_eq!(r.rel, "Docs/Report.md");
    assert_eq!(r.path, canonical.join("Docs").join("Report.md"));
    assert!(r.path.is_file());

    let r = sb.resolve_relative(0, "Docs/not-yet.md").unwrap();
    assert_eq!(r.rel, "Docs/not-yet.md", "a missing tail is allowed");

    // The temp dir is reached through a link on macOS (`/var` -> `/private/var`); both spellings
    // resolve.
    let abs = dir.path().join("Docs").join("Report.md");
    assert_eq!(
        sb.resolve(&abs.to_string_lossy()).unwrap().rel,
        "Docs/Report.md"
    );
    let abs = canonical.join("Docs").join("Report.md");
    assert_eq!(
        sb.resolve(&abs.to_string_lossy()).unwrap().rel,
        "Docs/Report.md"
    );
    assert_eq!(
        sb.resolve_any("Docs/Report.md").unwrap().rel,
        "Docs/Report.md"
    );
    assert_eq!(sb.resolve_relative(0, "").unwrap().rel, "");
    assert_eq!(
        r_display(&sb, "Docs/Report.md"),
        canonical.join("Docs/Report.md")
    );
}

fn r_display(sb: &Sandbox, rel: &str) -> std::path::PathBuf {
    sb.resolve_relative(0, rel).unwrap().display_path()
}

#[test]
fn traversal_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    fs::write(other.path().join("secret.txt"), "x").unwrap();
    let sb = sandbox_for(dir.path());

    for rel in [
        "..",
        "../x",
        "a/../../x",
        "a/../b.txt",
        "./a",
        r"a\..\..\x",
        r"..\x",
    ] {
        let v = violation(sb.resolve_relative(0, rel));
        assert!(
            matches!(v, Some(Violation::ParentTraversal | Violation::CurrentDir)),
            "{rel}: {v:?}"
        );
    }
    assert_eq!(
        violation(sb.resolve_relative(0, "/etc/passwd")),
        Some(Violation::NotRelative)
    );
    assert_eq!(
        violation(sb.resolve_relative(0, "a//b")),
        Some(Violation::Empty)
    );
    let outside = other.path().join("secret.txt");
    assert_eq!(
        violation(sb.resolve(&outside.to_string_lossy())),
        Some(Violation::OutsideRoot)
    );
    let sneaky = format!("{}/../{}", dir.path().display(), "x");
    assert_eq!(
        violation(sb.resolve(&sneaky)),
        Some(Violation::ParentTraversal)
    );
    assert_eq!(
        violation(sb.resolve("relative/x")),
        Some(Violation::NotAbsolute)
    );
    // `\` is a name character: `..\x` is one odd name, never a step up.
    assert_eq!(
        violation(sb.resolve_any(r"..\..\etc\passwd")),
        Some(Violation::ParentTraversal)
    );
}

#[test]
fn backslash_is_an_ordinary_name_character() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join(r"a\b.txt"), "x").unwrap();
    fs::create_dir(dir.path().join("a")).unwrap();
    let sb = sandbox_for(dir.path());
    let r = sb.resolve_relative(0, r"a\b.txt").unwrap();
    assert_eq!(r.rel, r"a\b.txt");
    assert!(r.path.is_file());
    assert_eq!(path_key(&r.rel), r"a\b.txt");
    // `a/b.txt` is another file (missing), not the one above.
    let r2 = sb.resolve_relative(0, "a/b.txt").unwrap();
    assert_ne!(r2.path, r.path);
    assert!(!r2.path.exists());
}

#[test]
fn posix_names_that_windows_refuses_are_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let sb = sandbox_for(dir.path());
    for name in ["CON", "nul.txt", "a:b.md", "notes.", "What next?.md", "x "] {
        fs::write(dir.path().join(name), "x").unwrap();
        let r = sb.resolve_relative(0, name).unwrap();
        assert_eq!(r.rel, name);
        assert!(r.path.is_file(), "{name}");
    }
    fs::write(dir.path().join("b.txt"), "x").unwrap();
    assert_eq!(
        violation(sb.resolve_relative(0, "b.txt/..namedfork/rsrc")),
        Some(Violation::AlternateDataStream)
    );
}

#[test]
fn symlink_pointing_out_of_the_root_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    fs::write(other.path().join("secret.txt"), "x").unwrap();
    fs::create_dir(dir.path().join("inner")).unwrap();
    fs::write(dir.path().join("inner").join("ok.txt"), "x").unwrap();
    symlink(other.path(), dir.path().join("escape")).unwrap();
    symlink(
        other.path().join("secret.txt"),
        dir.path().join("file-escape"),
    )
    .unwrap();
    symlink(dir.path().join("inner"), dir.path().join("alias")).unwrap();
    symlink("../../../../../../etc", dir.path().join("relative-escape")).unwrap();
    let sb = sandbox_for(dir.path());
    for rel in [
        "escape",
        "escape/secret.txt",
        "escape/new.txt",
        "escape/sub/deeper.txt",
        "file-escape",
        "relative-escape",
        "relative-escape/passwd",
    ] {
        assert_eq!(
            violation(sb.resolve_relative(0, rel)),
            Some(Violation::OutsideRoot),
            "{rel}"
        );
    }
    let abs = dir.path().join("escape").join("secret.txt");
    assert_eq!(
        violation(sb.resolve(&abs.to_string_lossy())),
        Some(Violation::OutsideRoot)
    );
    // A link that stays inside the root is fine and resolves to its target.
    assert_eq!(
        sb.resolve_relative(0, "alias/ok.txt").unwrap().rel,
        "inner/ok.txt"
    );
}

#[test]
fn dangling_symlink_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    symlink(
        other.path().join("not-yet.txt"),
        dir.path().join("dangling"),
    )
    .unwrap();
    symlink(
        dir.path().join("missing"),
        dir.path().join("dangling-inside"),
    )
    .unwrap();
    let sb = sandbox_for(dir.path());
    for rel in ["dangling", "dangling-inside", "dangling-inside/x.txt"] {
        assert_eq!(
            violation(sb.resolve_relative(0, rel)),
            Some(Violation::OutsideRoot),
            "{rel}"
        );
    }
}

#[test]
fn a_differently_cased_sibling_is_outside_the_root() {
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("Docs");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.md"), "x").unwrap();
    let sb = sandbox_for(&root);
    let canonical_base = fs::canonicalize(base.path()).unwrap();
    let variant = canonical_base.join("docs").join("a.md");
    if case_sensitive(base.path()) {
        // `docs` is another folder here: never inside `Docs`.
        fs::create_dir(canonical_base.join("docs")).unwrap();
        fs::write(&variant, "secret").unwrap();
        assert_eq!(
            violation(sb.resolve(&variant.to_string_lossy())),
            Some(Violation::OutsideRoot)
        );
        assert_eq!(
            violation(sb.resolve(&canonical_base.join("docs/new.md").to_string_lossy())),
            Some(Violation::OutsideRoot)
        );
    } else {
        // Same folder: the canonical (on-disk) case is reported.
        let r = sb.resolve(&variant.to_string_lossy()).unwrap();
        assert_eq!(r.path, canonical_base.join("Docs").join("a.md"));
        assert_eq!(r.rel, "a.md");
        let r = sb.resolve_relative(0, "A.MD").unwrap();
        assert_eq!(r.rel, "a.md", "on-disk case is reported");
    }
    // Without the file system: an exact-component check, so a case variant is not inside.
    assert!(!path_within_root(
        Path::new("/Vol/docs/a.md"),
        Path::new("/Vol/Docs")
    ));
    assert!(path_within_root(
        Path::new("/Vol/Docs/a.md"),
        Path::new("/Vol/Docs")
    ));
    assert!(!path_within_root(
        Path::new("/Vol/Docs2/a.md"),
        Path::new("/Vol/Docs")
    ));
}

#[test]
fn secrets_are_denied_existing_or_not() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join(".env"), "KEY=1").unwrap();
    fs::create_dir(dir.path().join(".ssh")).unwrap();
    fs::write(dir.path().join(".ssh").join("config"), "x").unwrap();
    let sb = sandbox_for(dir.path());
    for rel in [
        ".env",
        ".ENV",
        ".ssh",
        ".ssh/config",
        ".ssh/new",
        "certs/server.pem",
        "id_rsa",
    ] {
        assert_eq!(
            violation(sb.resolve_relative(0, rel)),
            Some(Violation::SecretDenied),
            "{rel}"
        );
    }
    // A link with an innocent name to a secret inside the root is judged by its target.
    symlink(dir.path().join(".env"), dir.path().join("notes.txt")).unwrap();
    assert_eq!(
        violation(sb.resolve_relative(0, "notes.txt")),
        Some(Violation::SecretDenied)
    );
    let e: Error = sb.resolve_relative(0, ".env").unwrap_err().into();
    assert_eq!(e.code, ErrorCode::PathDenied);
}

#[test]
fn roots_must_be_safe_existing_directories() {
    let mut sb = Sandbox::new();
    for r in [
        "/",
        "/System",
        "/Library",
        "/usr/bin",
        "/etc",
        "/private/etc",
        "/Applications",
    ] {
        if Path::new(r).exists() {
            assert_eq!(
                violation(sb.add_root(r)),
                Some(Violation::DangerousRoot),
                "{r}"
            );
        }
    }
    if let Some(home) = roots::home_dir() {
        assert_eq!(
            violation(sb.add_root(&home)),
            Some(Violation::DangerousRoot),
            "the home folder contains Library and .ssh"
        );
        let library = home.join("Library");
        if library.is_dir() {
            assert_eq!(
                violation(sb.add_root(&library)),
                Some(Violation::DangerousRoot)
            );
        }
        if home.join("Library/Preferences").is_dir() {
            assert_eq!(
                violation(sb.add_root(home.join("Library/Preferences"))),
                Some(Violation::DangerousRoot)
            );
        }
    }
    assert_eq!(
        violation(sb.add_root("relative")),
        Some(Violation::NotAbsolute)
    );
    assert_eq!(
        violation(sb.add_root("/tmp/../etc")),
        Some(Violation::ParentTraversal)
    );

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f.txt");
    fs::write(&file, "x").unwrap();
    assert_eq!(violation(sb.add_root(&file)), Some(Violation::BadRoot));
    assert!(matches!(
        sb.add_root(dir.path().join("missing")),
        Err(SandboxError::Io(_))
    ));
    assert_eq!(
        sb.add_root(dir.path()).unwrap(),
        0,
        "a folder in the per-user temp dir is a normal root"
    );

    // A folder that is, or is inside, a secret location can never become a root.
    let ssh = dir.path().join("backup").join(".SSH");
    fs::create_dir_all(ssh.join("keys")).unwrap();
    assert_eq!(violation(sb.add_root(&ssh)), Some(Violation::SecretDenied));
    assert_eq!(
        violation(sb.add_root(ssh.join("keys"))),
        Some(Violation::SecretDenied)
    );
    assert!(sb.add_root(dir.path().join("backup")).is_ok());

    // A link to a protected place is judged by its target.
    let link = dir.path().join("sys");
    symlink("/System", &link).unwrap();
    if Path::new("/System").is_dir() {
        assert_eq!(
            violation(sb.add_root(&link)),
            Some(Violation::DangerousRoot)
        );
    }

    let data = tempfile::tempdir().unwrap();
    let mut guarded = Sandbox::with_policy(DangerousRoots::none().with_never_inside(data.path()));
    assert_eq!(
        violation(guarded.add_root(data.path())),
        Some(Violation::DangerousRoot)
    );
    // `from_env(Some(data_dir))` protects the data dir even though it is in the temp dir.
    fs::create_dir(data.path().join("sub")).unwrap();
    let mut prod = Sandbox::with_policy(DangerousRoots::from_env(Some(data.path())));
    assert_eq!(
        violation(prod.add_root(data.path().join("sub"))),
        Some(Violation::DangerousRoot)
    );
}
