//! O2: DirectML.dll minimum version (M2-G). One test function = one ordered scenario in its own
//! process (an uncached load maps the DLL for the rest of the process).
#![allow(clippy::expect_used, clippy::unwrap_used)]

use sr_providers::dml::{
    self, DmlError, MIN_DIRECTML_VERSION, default_search_dirs, file_version, version_at_least,
};

#[test]
fn version_floor_refuses_older_directml_and_accepts_1_15() {
    // Pure comparison.
    assert!(version_at_least([1, 15, 0, 0], MIN_DIRECTML_VERSION));
    assert!(version_at_least([1, 15, 4, 0], MIN_DIRECTML_VERSION));
    assert!(version_at_least([2, 0, 0, 0], MIN_DIRECTML_VERSION));
    assert!(!version_at_least([1, 14, 9, 9], MIN_DIRECTML_VERSION));
    assert!(!version_at_least([1, 9, 99, 0], MIN_DIRECTML_VERSION));
    assert!(!version_at_least([0, 99, 0, 0], MIN_DIRECTML_VERSION));

    let require = std::env::var("SR_REQUIRE_DML").is_ok_and(|v| v == "1");
    let dirs = default_search_dirs();
    let present: Vec<_> = dirs
        .iter()
        .map(|d| d.join("DirectML.dll"))
        .filter(|p| p.is_file())
        .collect();
    if present.is_empty() {
        assert!(
            !require,
            "SR_REQUIRE_DML=1 but no DirectML.dll candidate in {dirs:?}"
        );
        println!("SKIP: no DirectML.dll");
        return;
    }
    for p in &present {
        let v = file_version(p);
        println!("{} → {v:?}", p.display());
        assert!(v.is_some(), "version resource readable");
    }
    // A non-PE file has no version.
    let tmp = std::env::temp_dir().join(format!("sr-m2g-nover-{}.dll", std::process::id()));
    std::fs::write(&tmp, b"not a PE file").unwrap();
    assert_eq!(file_version(&tmp), None);
    let _ = std::fs::remove_file(&tmp);

    // Raise the floor above every real copy: each Microsoft-signed candidate is refused as too
    // old, and nothing gets mapped.
    let r = dml::load_directml_with_floor(&dirs, [1, 16, 0, 0]);
    match &r {
        Err(DmlError::TooOld { version, .. }) => {
            println!("refused: {}", r.clone().unwrap_err());
            assert!(version.is_some_and(|v| v[0] == 1 && v[1] == 15));
        }
        other => panic!("expected TooOld, got {other:?}"),
    }
    assert!(
        dml::loaded_module_path("DirectML.dll").is_none(),
        "a refused DirectML.dll is never mapped"
    );

    // Real floor: the shipped 1.15.x loads.
    let m = dml::load_directml(&dirs).expect("1.15.x DirectML loads");
    println!(
        "loaded {} v{:?} ({})",
        m.path.display(),
        m.version,
        m.signer
    );
    assert!(version_at_least(m.version, MIN_DIRECTML_VERSION));
    assert_eq!(m.version[..2], [1, 15]);
}
