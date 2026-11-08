//! File-system tests for the sandbox (Windows only): real junctions, 8.3 short names, long paths.

use std::fs;
use std::path::Path;
use std::process::Command;

use super::*;

fn sandbox_for(root: &Path) -> Sandbox {
    let mut sb = Sandbox::with_policy(DangerousRoots::none());
    sb.add_root(root).unwrap();
    sb
}

fn violation<T: std::fmt::Debug>(r: SResult<T>) -> Option<Violation> {
    r.err().and_then(|e| e.violation())
}

/// `mklink /J` needs no admin rights or developer mode.
fn junction(link: &Path, target: &Path) -> bool {
    Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn paths_inside_the_root_resolve_to_long_canonical_names() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("Docs")).unwrap();
    fs::write(dir.path().join("Docs").join("Report.md"), "x").unwrap();
    let sb = sandbox_for(dir.path());

    let r = sb.resolve_relative(0, "docs/report.md").unwrap();
    assert_eq!(r.rel, "Docs/Report.md", "on-disk case is reported");
    assert!(
        r.path.to_string_lossy().starts_with(r"\\?\"),
        "verbatim long form"
    );
    assert!(r.path.is_file());

    let r = sb.resolve_relative(0, r"Docs\not-yet.md").unwrap();
    assert_eq!(r.rel, "Docs/not-yet.md", "a missing tail is allowed");

    let abs = dir.path().join("Docs").join("Report.md");
    let r = sb.resolve(&abs.to_string_lossy()).unwrap();
    assert_eq!(r.rel, "Docs/Report.md");
    let upper = abs.to_string_lossy().to_uppercase();
    assert_eq!(sb.resolve(&upper).unwrap().rel, "Docs/Report.md");
    assert_eq!(
        sb.resolve_any("Docs/Report.md").unwrap().rel,
        "Docs/Report.md"
    );
    assert_eq!(sb.resolve_relative(0, "").unwrap().rel, "");
    assert!(!r.display_path().to_string_lossy().starts_with(r"\\?\"));
}

#[test]
fn traversal_and_foreign_forms_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    fs::write(other.path().join("secret.txt"), "x").unwrap();
    let sb = sandbox_for(dir.path());

    for rel in ["..", "../x", r"a\..\..\x", "a/../b.txt", "./a"] {
        let v = violation(sb.resolve_relative(0, rel));
        assert!(
            matches!(v, Some(Violation::ParentTraversal | Violation::CurrentDir)),
            "{rel}: {v:?}"
        );
    }
    assert_eq!(
        violation(sb.resolve_relative(0, r"\Windows")),
        Some(Violation::NotRelative)
    );
    assert_eq!(
        violation(sb.resolve_relative(0, r"C:\Windows")),
        Some(Violation::NotRelative)
    );
    assert_eq!(
        violation(sb.resolve_relative(0, "C:foo")),
        Some(Violation::DriveRelative)
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
    let sneaky = format!(r"{}\..\{}", dir.path().display(), "x");
    assert_eq!(
        violation(sb.resolve(&sneaky)),
        Some(Violation::ParentTraversal)
    );

    // Verbatim / UNC / device forms are refused unless the root itself was given in that form.
    let verbatim = format!(r"\\?\{}\a.txt", dir.path().display());
    assert_eq!(
        violation(sb.resolve(&verbatim)),
        Some(Violation::ForeignPrefix)
    );
    assert_eq!(
        violation(sb.resolve(r"\\localhost\c$\Windows\win.ini")),
        Some(Violation::ForeignPrefix)
    );
    assert_eq!(
        violation(sb.resolve(r"\\.\PhysicalDrive0")),
        Some(Violation::ForeignPrefix)
    );
    assert_eq!(
        violation(sb.resolve(r"\??\C:\Windows")),
        Some(Violation::ForeignPrefix)
    );
}

#[test]
fn device_names_ads_and_trailing_dots_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("b.txt"), "x").unwrap();
    let sb = sandbox_for(dir.path());
    let cases = [
        ("CON", Violation::ReservedName),
        ("CON.txt", Violation::ReservedName),
        ("sub/nul.tar.gz", Violation::ReservedName),
        ("COM1.log", Violation::ReservedName),
        ("lpt\u{b9}", Violation::ReservedName),
        ("b.txt.", Violation::TrailingDotOrSpace),
        ("b.txt ", Violation::TrailingDotOrSpace),
        ("b.txt::$DATA", Violation::AlternateDataStream),
        ("b.txt:hidden", Violation::AlternateDataStream),
        ("b?.txt", Violation::InvalidChar),
    ];
    for (rel, want) in cases {
        assert_eq!(violation(sb.resolve_relative(0, rel)), Some(want), "{rel}");
    }
}

#[test]
fn junction_pointing_out_of_the_root_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    fs::write(other.path().join("secret.txt"), "x").unwrap();
    fs::create_dir(dir.path().join("inner")).unwrap();
    fs::write(dir.path().join("inner").join("ok.txt"), "x").unwrap();
    if !junction(&dir.path().join("escape"), other.path())
        || !junction(&dir.path().join("alias"), &dir.path().join("inner"))
    {
        eprintln!("skipped: mklink /J unavailable");
        return;
    }
    let sb = sandbox_for(dir.path());
    for rel in [
        "escape",
        "escape/secret.txt",
        "escape/new.txt",
        r"escape\sub\deeper.txt",
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
    // A junction that stays inside the root is fine and resolves to its target.
    assert_eq!(
        sb.resolve_relative(0, "alias/ok.txt").unwrap().rel,
        "inner/ok.txt"
    );
}

#[test]
fn short_names_are_expanded_before_policy_checks() {
    let dir = tempfile::tempdir().unwrap();
    let long = dir.path().join("LongDirectoryName");
    fs::create_dir(&long).unwrap();
    fs::write(long.join("ordinary_long_name.txt"), "x").unwrap();
    fs::write(long.join("credentials_backup.json"), "x").unwrap();
    if !dir.path().join("LONGDI~1").exists() {
        eprintln!("skipped: 8.3 short names are disabled on this volume");
        return;
    }
    let sb = sandbox_for(dir.path());
    let r = sb.resolve_relative(0, "LONGDI~1/ORDINA~1.TXT").unwrap();
    assert_eq!(r.rel, "LongDirectoryName/ordinary_long_name.txt");
    assert_eq!(
        violation(sb.resolve_relative(0, "LONGDI~1/CREDEN~1.JSO")),
        Some(Violation::SecretDenied),
        "short name must not bypass the secret denylist"
    );
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
    assert!(sb.resolve_relative(0, "notes.txt").is_ok());
    let e: Error = sb.resolve_relative(0, ".env").unwrap_err().into();
    assert_eq!(e.code, ErrorCode::PathDenied);
}

#[test]
fn paths_beyond_max_path_work() {
    let dir = tempfile::tempdir().unwrap();
    let sb = sandbox_for(dir.path());
    let seg = "d".repeat(60);
    let rel = [seg.as_str(); 6].join("/");
    let resolved_dir = sb.resolve_relative(0, &rel).unwrap();
    fs::create_dir_all(&resolved_dir.path).unwrap();
    let file_rel = format!("{rel}/file.txt");
    let f = sb.resolve_relative(0, &file_rel).unwrap();
    assert!(f.path.as_os_str().len() > 260);
    fs::write(&f.path, "long").unwrap();
    let again = sb.resolve_relative(0, &file_rel).unwrap();
    assert_eq!(fs::read_to_string(&again.path).unwrap(), "long");
}

#[test]
fn roots_must_be_safe_existing_directories() {
    let mut sb = Sandbox::new();
    assert_eq!(
        violation(sb.add_root(r"C:\")),
        Some(Violation::DangerousRoot)
    );
    if let Some(win) = std::env::var_os("SystemRoot") {
        assert_eq!(violation(sb.add_root(&win)), Some(Violation::DangerousRoot));
        assert_eq!(
            violation(sb.add_root(Path::new(&win).join("System32"))),
            Some(Violation::DangerousRoot)
        );
    }
    if let Some(home) = std::env::var_os("USERPROFILE") {
        assert_eq!(
            violation(sb.add_root(&home)),
            Some(Violation::DangerousRoot)
        );
    }
    assert_eq!(violation(sb.add_root(r"\\.\C:\")), Some(Violation::BadRoot));
    assert_eq!(
        violation(sb.add_root("relative")),
        Some(Violation::NotAbsolute)
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
        "a temp folder is a normal root"
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

    let data = tempfile::tempdir().unwrap();
    let mut guarded = Sandbox::with_policy(DangerousRoots::none().with_never_inside(data.path()));
    assert_eq!(
        violation(guarded.add_root(data.path())),
        Some(Violation::DangerousRoot)
    );
}
