//! DirectML from the build we ship (static pyke ORT, delay-loaded DirectML, verified absolute-path
//! load), its parity with the CPU and the reference, and the silent CPU fallback ladder.
#![cfg(all(feature = "local", windows))]
#![allow(clippy::expect_used, clippy::unwrap_used)] // test code (clippy.toml allows both in tests)

mod common;

use std::sync::Arc;

use sr_common::tier::{EpActive, EpPlan, Thresholds};
use sr_providers::local::{FaultInjection, LocalEmbedder, LocalOptions, min_cosine};
use sr_providers::{EmbeddingProvider, EpReason, MemorySink, ProviderEvent};
use tokio_util::sync::CancellationToken;

/// Load on DirectML; `None` (skip) when this machine can't run it and `SR_REQUIRE_DML` is unset.
async fn load_dml(o: LocalOptions, sink: Arc<MemorySink>) -> Option<LocalEmbedder> {
    let e = LocalEmbedder::load(o, sink)
        .await
        .expect("load never fails because of the GPU");
    if e.active_ep() != EpActive::DirectMl {
        let r = e.load_report();
        assert!(
            !common::require_dml(),
            "SR_REQUIRE_DML set but DirectML not active: {r:?}"
        );
        eprintln!("SKIP: DirectML not available here: {r:?}");
        return None;
    }
    Some(e)
}

#[tokio::test(flavor = "multi_thread")]
async fn directml_runs_from_static_build_and_matches_cpu_and_reference() {
    let Some(dir) = common::model_dir() else {
        return;
    };
    let sink = MemorySink::new();
    let plan = EpPlan::DirectMl {
        dxgi_index: common::dxgi_index(),
    };
    let Some(gpu) = load_dml(common::opts(&dir, plan), sink.clone()).await else {
        return;
    };
    let r = gpu.load_report().clone();
    eprintln!("DirectML load report: {r:?}");
    assert_eq!(r.reason, EpReason::Planned);
    let canary = r.canary_min_cos.expect("canary ran");
    assert!(canary >= 0.999, "canary {canary}");

    // The verified file is the module the process actually mapped; d3d12 is System32's.
    let m = r.dml.expect("verified module");
    assert_eq!(m.signer, "Microsoft Corporation");
    assert!(
        m.root.starts_with("Microsoft Root Certificate Authority"),
        "{}",
        m.root
    );
    let mapped =
        sr_providers::dml::loaded_module_path("DirectML.dll").expect("DirectML.dll mapped");
    assert_eq!(
        mapped.to_string_lossy().to_lowercase(),
        m.path.to_string_lossy().to_lowercase()
    );
    let d3d12 = sr_providers::dml::loaded_module_path("d3d12.dll").expect("d3d12 mapped");
    assert!(
        d3d12
            .to_string_lossy()
            .to_lowercase()
            .contains(r"\system32\"),
        "{}",
        d3d12.display()
    );
    assert!(matches!(
        sink.take().as_slice(),
        [ProviderEvent::EpChanged {
            from: None,
            to: EpActive::DirectMl,
            reason_code: EpReason::Planned,
            ..
        }]
    ));
    // VRAM-uncapped Minimum GPU batch from the TierProfile.
    assert_eq!(gpu.max_batch(), 64);

    let s = common::sentences();
    let refs: Vec<&str> = s.iter().map(String::as_str).collect();
    let c = CancellationToken::new();
    let on_gpu = gpu.embed_documents(&refs, &c).await.unwrap();
    assert_eq!(
        gpu.active_ep(),
        EpActive::DirectMl,
        "still on the GPU after a real run"
    );
    let (min_ref, mean_ref) = common::stats(&on_gpu, &common::reference());
    eprintln!("DirectML vs sentence-transformers: min={min_ref:.6} mean={mean_ref:.6}");
    assert!(min_ref >= 0.999, "DML vs reference {min_ref}");

    let cpu = LocalEmbedder::load(
        common::opts(&dir, EpPlan::Cpu),
        Arc::new(sr_providers::NoopSink),
    )
    .await
    .unwrap();
    let on_cpu = cpu.embed_documents(&refs, &c).await.unwrap();
    let (min_cpu, mean_cpu) = common::stats(&on_gpu, &on_cpu);
    eprintln!("DirectML vs CPU (same file): min={min_cpu:.6} mean={mean_cpu:.6}");
    assert!(min_cpu >= 0.999, "DML vs CPU {min_cpu}");

    // Queries too (prefix path).
    let qg = gpu
        .embed_query("rebuild the vector index", &c)
        .await
        .unwrap();
    let qc = cpu
        .embed_query("rebuild the vector index", &c)
        .await
        .unwrap();
    assert!(min_cosine(&[qg], &[qc]) >= 0.999);
}

#[tokio::test(flavor = "multi_thread")]
async fn probe_plan_uses_directml_only_when_faster() {
    let Some(dir) = common::model_dir() else {
        return;
    };
    let plan = EpPlan::ProbeDirectMl {
        dxgi_index: common::dxgi_index(),
    };
    let e = LocalEmbedder::load(common::opts(&dir, plan), MemorySink::new())
        .await
        .unwrap();
    let r = e.load_report();
    eprintln!("probe report: {r:?}");
    match r.ep {
        EpActive::DirectMl => {
            assert!(r.probe_speedup.unwrap_or(0.0) >= Thresholds::default().probe_min_speedup);
        }
        EpActive::Cpu => {
            assert!(
                !common::require_dml() || r.reason == EpReason::ProbeTooSlow,
                "{r:?}"
            );
        }
    }
}

/// Every forced DirectML failure ends on a working CPU session, with no error returned.
async fn assert_silent_fallback(o: LocalOptions, want: EpReason) {
    let sink = MemorySink::new();
    let e = LocalEmbedder::load(o, sink.clone())
        .await
        .expect("GPU failure is never an error");
    let r = e.load_report().clone();
    if r.reason == EpReason::DllMissing && !common::require_dml() {
        eprintln!("SKIP: no DirectML.dll");
        return;
    }
    assert_eq!(e.active_ep(), EpActive::Cpu, "{r:?}");
    assert_eq!(r.reason, want, "{r:?}");
    assert_eq!(e.max_batch(), 8, "CPU batch from the TierProfile");
    let s = common::sentences();
    let refs: Vec<&str> = s.iter().map(String::as_str).collect();
    let v = e
        .embed_documents(&refs, &CancellationToken::new())
        .await
        .expect("CPU works");
    let (min, _) = common::stats(&v, &common::reference());
    assert!(min >= 0.999);
    let ev = sink.take();
    assert!(
        matches!(ev.as_slice(), [ProviderEvent::EpChanged { to: EpActive::Cpu, reason_code, .. }] if *reason_code == want),
        "{ev:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn forced_failure_bad_adapter_falls_back_to_cpu() {
    let Some(dir) = common::model_dir() else {
        return;
    };
    // Real ORT registration error (887A0002-style) with error_on_failure.
    assert_silent_fallback(
        common::opts(&dir, EpPlan::DirectMl { dxgi_index: 99 }),
        EpReason::RegistrationFailed,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn forced_failure_non_finite_canary_falls_back_to_cpu() {
    let Some(dir) = common::model_dir() else {
        return;
    };
    let mut o = common::opts(
        &dir,
        EpPlan::DirectMl {
            dxgi_index: common::dxgi_index(),
        },
    );
    o.fault = FaultInjection::DmlCanaryNonFinite;
    assert_silent_fallback(o, EpReason::CanaryNonFinite).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn forced_failure_canary_mismatch_falls_back_to_cpu() {
    let Some(dir) = common::model_dir() else {
        return;
    };
    let mut o = common::opts(
        &dir,
        EpPlan::DirectMl {
            dxgi_index: common::dxgi_index(),
        },
    );
    o.fault = FaultInjection::DmlCanaryMismatch;
    assert_silent_fallback(o, EpReason::CanaryMismatch).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn gpu_off_switch_uses_cpu() {
    let Some(dir) = common::model_dir() else {
        return;
    };
    let mut o = common::opts(
        &dir,
        EpPlan::DirectMl {
            dxgi_index: common::dxgi_index(),
        },
    );
    o.use_gpu = false;
    assert_silent_fallback(o, EpReason::DisabledByUser).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_fault_mid_indexing_reruns_on_cpu_without_error() {
    let Some(dir) = common::model_dir() else {
        return;
    };
    let sink = MemorySink::new();
    let mut o = common::opts(
        &dir,
        EpPlan::DirectMl {
            dxgi_index: common::dxgi_index(),
        },
    );
    o.fault = FaultInjection::DmlRunFailsOnce;
    let Some(e) = load_dml(o, sink.clone()).await else {
        return;
    };
    sink.take();
    let s = common::sentences();
    let refs: Vec<&str> = s.iter().map(String::as_str).collect();
    let v = e
        .embed_documents(&refs, &CancellationToken::new())
        .await
        .expect("no error surfaced");
    assert_eq!(e.active_ep(), EpActive::Cpu);
    assert_eq!(e.max_batch(), 8);
    let (min, _) = common::stats(&v, &common::reference());
    assert!(min >= 0.999);
    let ev = sink.take();
    assert!(
        matches!(
            ev.as_slice(),
            [ProviderEvent::EpChanged {
                from: Some(EpActive::DirectMl),
                to: EpActive::Cpu,
                reason_code: EpReason::RuntimeFault,
                ..
            }]
        ),
        "{ev:?}"
    );
}
