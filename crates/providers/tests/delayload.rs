//! Own process (own test binary) because the DirectML load result is process-wide.
//! Proves: (1) the delay-load link args work — no GPU DLL is mapped at start or on the CPU path;
//! (2) a planted, unsigned `DirectML.dll` is refused and never mapped; (3) the provider then runs
//! on the CPU with no error (reason `DllUntrusted`).
#![cfg(all(feature = "local", windows))]
#![allow(clippy::expect_used, clippy::unwrap_used)] // test code (clippy.toml allows both in tests)

mod common;

use std::sync::Arc;

use sr_common::tier::{EpActive, EpPlan};
use sr_providers::dml::{self, DmlError};
use sr_providers::local::LocalEmbedder;
use sr_providers::{EmbedKind, EpReason, NoopSink};

#[test]
fn delay_load_cpu_path_and_planted_dll() {
    // (1) Nothing GPU-related mapped by the loader before main / before ORT.
    assert_eq!(
        dml::loaded_module_path("DirectML.dll"),
        None,
        "DirectML.dll must be delay-loaded"
    );
    assert_eq!(
        dml::loaded_module_path("d3d12.dll"),
        None,
        "d3d12.dll must be delay-loaded"
    );

    // (1b) A CPU session and a real embed never map DirectML.
    if let Some(dir) = common::model_dir() {
        let e = LocalEmbedder::load_blocking(common::opts(&dir, EpPlan::Cpu), Arc::new(NoopSink))
            .unwrap();
        let v = e.embed_blocking(EmbedKind::Doc, &["hello world"]).unwrap();
        assert_eq!(v[0].len(), 384);
        assert_eq!(
            dml::loaded_module_path("DirectML.dll"),
            None,
            "CPU path maps no DirectML"
        );
    }

    // (2) Planted unsigned DirectML.dll (a copy of this unsigned test exe).
    let decoy = std::env::temp_dir().join(format!("sr-m1d-decoy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&decoy);
    std::fs::create_dir_all(&decoy).unwrap();
    std::fs::copy(std::env::current_exe().unwrap(), decoy.join("DirectML.dll")).unwrap();
    match dml::verify_microsoft_signature(&decoy.join("DirectML.dll")) {
        Err(DmlError::Untrusted { reason, .. }) => eprintln!("decoy refused: {reason}"),
        other => panic!("decoy must be refused: {other:?}"),
    }
    let r = dml::load_directml(std::slice::from_ref(&decoy));
    assert!(matches!(r, Err(DmlError::Untrusted { .. })), "{r:?}");
    assert_eq!(
        dml::loaded_module_path("DirectML.dll"),
        None,
        "decoy never mapped"
    );

    // (3) The provider silently uses the CPU.
    if let Some(dir) = common::model_dir() {
        let mut o = common::opts(
            &dir,
            EpPlan::DirectMl {
                dxgi_index: common::dxgi_index(),
            },
        );
        o.dml_search_dirs = vec![decoy.clone()];
        let e = LocalEmbedder::load_blocking(o, Arc::new(NoopSink)).expect("no error");
        assert_eq!(e.active_ep(), EpActive::Cpu);
        assert_eq!(e.load_report().reason, EpReason::DllUntrusted);
        assert_eq!(dml::loaded_module_path("DirectML.dll"), None);
    }

    // (4) Last, because it maps a module named DirectML.dll into this process: a *Microsoft-signed*
    // DLL that is not DirectML (System32 d3d12.dll renamed) passes the signature check but is
    // refused for lacking DMLCreateDevice1, so the delay-load helper is never pointed at it.
    let sys_d3d12 = std::path::Path::new(r"C:\Windows\System32\d3d12.dll");
    let (o, root) =
        dml::verify_microsoft_signature(sys_d3d12).expect("control: d3d12 is MS-signed");
    eprintln!("control d3d12.dll: O={o} root={root}");
    // The renamed DLL stays mapped in this process, so its folder can only be removed by a later
    // run: clean up every earlier run's folder first (M3 tester follow-up: one leaked per run).
    if let Ok(rd) = std::fs::read_dir(std::env::temp_dir()) {
        for entry in rd.flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("sr-m1d-renamed-")
            {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    let renamed = std::env::temp_dir().join(format!("sr-m1d-renamed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&renamed);
    std::fs::create_dir_all(&renamed).unwrap();
    std::fs::copy(sys_d3d12, renamed.join("DirectML.dll")).unwrap();
    match dml::load_directml_uncached(std::slice::from_ref(&renamed)) {
        Err(DmlError::Untrusted { reason, .. }) => {
            assert!(reason.contains("DMLCreateDevice1"), "{reason}");
            eprintln!("renamed signed DLL refused: {reason}");
        }
        other => panic!("renamed d3d12 must be refused: {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&decoy);
}
