use super::*;

fn dgpu(vram: u64) -> GpuInfo {
    GpuInfo {
        name: "Test dGPU".into(),
        vendor_id: 0x10de,
        device_id: 0x1234,
        dedicated_vram: vram,
        shared_mem: 8 * GIB,
        is_software: false,
        d3d12_fl11: true,
        uma: false,
        dxgi_index: 0,
        driver_version: 0x001f_0000_0f00_0001,
    }
}
fn igpu(carve_out: u64) -> GpuInfo {
    GpuInfo {
        name: "Test iGPU".into(),
        vendor_id: 0x8086,
        uma: true,
        dedicated_vram: carve_out,
        ..dgpu(0)
    }
}
fn warp() -> GpuInfo {
    GpuInfo {
        name: "Microsoft Basic Render Driver".into(),
        vendor_id: MS_VENDOR_ID,
        device_id: MS_BASIC_RENDER_DEVICE_ID,
        is_software: true,
        dxgi_index: 1,
        ..dgpu(0)
    }
}
fn hw(ram_gib: u64, cores: u32, avx2: bool, disk_gib: u64, gpus: Vec<GpuInfo>) -> HwInfo {
    HwInfo {
        total_ram: ram_gib * GIB,
        physical_cores: cores,
        logical_cores: cores * 2,
        avx2,
        avx512_vnni: false,
        free_disk: disk_gib * GIB,
        gpus,
    }
}
fn t() -> Thresholds {
    Thresholds::default()
}
/// The tier tables are the Windows x64 ones on every host (see `macos_*` tests for Apple silicon).
fn dec(h: &HwInfo) -> TierDecision {
    select_tier_on(h, &t(), TierPlatform::WINDOWS_X64)
}
fn tier(h: &HwInfo) -> Tier {
    dec(h).tier
}

// ------------------------------------------------------------------ thresholds = Amendment 3.6
#[test]
fn thresholds_match_amendment_3_6() {
    let t = t();
    assert_eq!(t.dgpu_min_vram, 4 * GIB);
    assert_eq!(t.high_min_vram, 6 * GIB);
    assert_eq!(t.std_min_ram, 8 * GIB);
    assert_eq!(t.std_min_cores_cpu_only, 6);
    assert_eq!(t.std_min_free_disk, 10 * GIB);
    assert_eq!(t.high_min_ram, 16 * GIB);
    assert_eq!(t.high_min_free_disk, 25 * GIB);
    assert!((t.probe_min_speedup - 1.2).abs() < f32::EPSILON);
}

// ------------------------------------------------------------------ GPU cases
#[test]
fn no_gpu_uses_cpu_rules_and_is_never_high() {
    let d = dec(&hw(16, 8, true, 100, vec![]));
    assert_eq!(
        (d.tier, d.ep, d.gpu_class),
        (Tier::Standard, EpPlan::Cpu, GpuClass::None)
    );
    assert_eq!(tier(&hw(16, 4, true, 100, vec![])), Tier::Minimum);
    assert_eq!(tier(&hw(128, 32, true, 2000, vec![])), Tier::Standard);
}

#[test]
fn integrated_gpu_is_probed_and_never_raises_the_tier() {
    let d = dec(&hw(16, 4, true, 100, vec![igpu(128 * MIB)]));
    assert_eq!(d.ep, EpPlan::ProbeDirectMl { dxgi_index: 0 });
    assert_eq!((d.tier, d.gpu_class), (Tier::Minimum, GpuClass::Integrated));
    let d = dec(&hw(32, 8, true, 100, vec![igpu(128 * MIB)]));
    assert_eq!(
        (d.tier, d.gpu_class),
        (Tier::Standard, GpuClass::Integrated)
    );
}

#[test]
fn uma_apu_with_large_carve_out_is_integrated() {
    let d = dec(&hw(32, 8, true, 100, vec![igpu(8 * GIB)]));
    assert_eq!(d.gpu_class, GpuClass::Integrated);
    assert_eq!(d.tier, Tier::Standard, "via the CPU rule only");
    // Same APU on a 4-core box: the carve-out never lifts it above Minimum.
    assert_eq!(
        tier(&hw(32, 4, true, 100, vec![igpu(8 * GIB)])),
        Tier::Minimum
    );
}

#[test]
fn dgpu_vram_below_at_above_4gib() {
    let m = t().dgpu_min_vram;
    let d = dec(&hw(16, 4, true, 100, vec![dgpu(m - 1)]));
    assert_eq!((d.gpu_class, d.tier), (GpuClass::Integrated, Tier::Minimum));
    assert_eq!(d.ep, EpPlan::ProbeDirectMl { dxgi_index: 0 });
    for v in [m, m + 1] {
        let d = dec(&hw(16, 4, true, 100, vec![dgpu(v)]));
        assert_eq!((d.gpu_class, d.tier), (GpuClass::Dedicated, Tier::Standard));
        assert_eq!(d.ep, EpPlan::DirectMl { dxgi_index: 0 });
    }
}

#[test]
fn dgpu_vram_below_at_above_6gib_for_high() {
    let m = t().high_min_vram;
    assert_eq!(
        tier(&hw(32, 8, true, 100, vec![dgpu(m - 1)])),
        Tier::Standard
    );
    assert_eq!(tier(&hw(32, 8, true, 100, vec![dgpu(m)])), Tier::High);
    assert_eq!(tier(&hw(32, 8, true, 100, vec![dgpu(m + 1)])), Tier::High);
}

#[test]
fn dgpu_makes_low_core_box_standard() {
    let d = dec(&hw(8, 2, true, 100, vec![dgpu(6 * GIB)]));
    assert_eq!(
        (d.tier, d.ep),
        (Tier::Standard, EpPlan::DirectMl { dxgi_index: 0 })
    );
}

#[test]
fn warp_and_lying_driver_are_no_gpu() {
    let d = dec(&hw(32, 8, true, 100, vec![warp()]));
    assert_eq!(
        (d.ep, d.gpu_class, d.tier),
        (EpPlan::Cpu, GpuClass::None, Tier::Standard)
    );
    let mut w = warp();
    w.is_software = false;
    w.dedicated_vram = 16 * GIB;
    assert_eq!(classify(&w, &t()), GpuClass::None);
}

#[test]
fn no_d3d12_fl11_is_no_gpu() {
    let mut g = dgpu(8 * GIB);
    g.d3d12_fl11 = false;
    assert_eq!(classify(&g, &t()), GpuClass::None);
    let d = dec(&hw(32, 4, true, 100, vec![g]));
    assert_eq!((d.ep, d.tier), (EpPlan::Cpu, Tier::Minimum));
}

#[test]
fn hybrid_laptop_prefers_the_dedicated_gpu() {
    let mut i = igpu(512 * MIB);
    i.dxgi_index = 0;
    let mut d = dgpu(8 * GIB);
    d.dxgi_index = 1;
    let r = dec(&hw(32, 8, true, 100, vec![i, d, warp()]));
    assert_eq!(
        (r.ep, r.tier),
        (EpPlan::DirectMl { dxgi_index: 1 }, Tier::High)
    );
    let mut i = igpu(16 * GIB);
    i.dxgi_index = 0;
    let r = dec(&hw(32, 8, true, 100, vec![i, dgpu(4 * GIB)]));
    assert_eq!((r.gpu_class, r.tier), (GpuClass::Dedicated, Tier::Standard));
}

// ------------------------------------------------------------------ AVX2
#[test]
fn no_avx2_is_minimum_even_with_a_big_dgpu_but_the_gpu_is_still_used() {
    assert_eq!(tier(&hw(64, 16, false, 500, vec![])), Tier::Minimum);
    let d = dec(&hw(64, 16, false, 500, vec![dgpu(16 * GIB)]));
    assert_eq!(d.tier, Tier::Minimum);
    assert_eq!(d.ep, EpPlan::DirectMl { dxgi_index: 0 });
    assert!(d.reasons.iter().any(|r| r.contains("AVX2")));
}

// ------------------------------------------------------------------ RAM / cores / disk
#[test]
fn std_ram_below_at_above() {
    let mut h = hw(0, 6, true, 100, vec![]);
    let m = t().std_min_ram;
    for (ram, want) in [
        (m - 1, Tier::Minimum),
        (m, Tier::Standard),
        (m + 1, Tier::Standard),
    ] {
        h.total_ram = ram;
        assert_eq!(tier(&h), want, "ram={ram}");
    }
}

#[test]
fn high_ram_below_at_above() {
    let mut h = hw(0, 8, true, 100, vec![dgpu(8 * GIB)]);
    let m = t().high_min_ram;
    for (ram, want) in [
        (m - 1, Tier::Standard),
        (m, Tier::High),
        (m + 1, Tier::High),
    ] {
        h.total_ram = ram;
        assert_eq!(tier(&h), want, "ram={ram}");
    }
}

#[test]
fn eight_gb_laptop_counts_as_eight() {
    assert_eq!(round_ram(8 * GIB - 300 * MIB), 8 * GIB);
    assert_eq!(round_ram(8 * GIB - 600 * MIB), 7 * GIB);
    assert_eq!(round_ram(u64::MAX) % GIB, 0, "no overflow");
}

#[test]
fn std_cores_below_at_above_cpu_only() {
    let c = t().std_min_cores_cpu_only;
    assert_eq!(tier(&hw(16, c - 1, true, 100, vec![])), Tier::Minimum);
    assert_eq!(tier(&hw(16, c, true, 100, vec![])), Tier::Standard);
    assert_eq!(tier(&hw(16, c + 1, true, 100, vec![])), Tier::Standard);
}

#[test]
fn std_disk_below_at_above() {
    let mut h = hw(16, 8, true, 0, vec![]);
    let m = t().std_min_free_disk;
    for (d, want) in [
        (m - 1, Tier::Minimum),
        (m, Tier::Standard),
        (m + 1, Tier::Standard),
    ] {
        h.free_disk = d;
        assert_eq!(tier(&h), want);
    }
}

#[test]
fn high_disk_below_at_above() {
    let mut h = hw(32, 8, true, 0, vec![dgpu(8 * GIB)]);
    let m = t().high_min_free_disk;
    for (d, want) in [
        (m - 1, Tier::Standard),
        (m, Tier::High),
        (m + 1, Tier::High),
    ] {
        h.free_disk = d;
        assert_eq!(tier(&h), want);
    }
}

// ------------------------------------------------------------------ override + profiles
#[test]
fn manual_override_warns_only_above_the_recommendation() {
    let rec = dec(&hw(16, 4, true, 100, vec![]));
    assert_eq!(effective_tier(&rec, None), (Tier::Minimum, None));
    assert_eq!(
        effective_tier(&rec, Some(Tier::High)),
        (Tier::High, Some(OVERRIDE_WARNING))
    );
    let rec = dec(&hw(32, 8, true, 100, vec![dgpu(8 * GIB)]));
    assert_eq!(
        effective_tier(&rec, Some(Tier::Minimum)),
        (Tier::Minimum, None)
    );
}

#[test]
fn profiles_match_amendment_3_4_to_3_6() {
    let (m, s, h) = (
        TierProfile::for_tier(Tier::Minimum),
        TierProfile::for_tier(Tier::Standard),
        TierProfile::for_tier(Tier::High),
    );
    assert_eq!((m.model_id, m.dims), ("BAAI/bge-small-en-v1.5", 384));
    assert_eq!(m.gpu_file.path, "model_optimized.onnx");
    assert_eq!(
        m.gpu_file, m.cpu_file,
        "one bundled fp32 file for CPU and GPU"
    );
    assert_eq!(m.gpu_file.precision, Precision::Fp32);
    assert!(m.bundled && !s.bundled && !h.bundled);
    assert_eq!(
        (s.model_id, s.dims),
        ("Snowflake/snowflake-arctic-embed-m-v1.5", 768)
    );
    assert_eq!(
        (h.model_id, h.dims),
        ("Snowflake/snowflake-arctic-embed-l", 1024)
    );
    assert_eq!((CHUNK_TOKENS, CHUNK_OVERLAP), (400, 48));
    for p in [m, s, h] {
        assert_eq!(TierProfile::for_tier(p.tier).tier, p.tier);
        // 3.4: no int8 anywhere until the eval gate clears it.
        assert_ne!(p.gpu_file.precision, Precision::Int8);
        assert_ne!(p.cpu_file.precision, Precision::Int8);
        assert_eq!(p.pooling, Pooling::Cls);
        assert!(CHUNK_TOKENS + 16 <= p.max_seq_tokens, "chunk + prefix fit");
        assert!(p.batch_gpu >= p.batch_cpu);
        for f in [p.gpu_file, p.cpu_file] {
            assert_eq!(f.sha256.len(), 64);
            assert!(
                f.sha256
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            );
            assert_eq!(f.hf_revision.len(), 40);
        }
        assert!(p.ann.refine_factor >= 1 && p.ann.nprobes >= 1);
        assert_eq!(p.ann.index_type, AnnIndexType::IvfHnswSq);
    }
    assert!(m.dims < s.dims && s.dims < h.dims);
    assert!(h.rerank && h.rerank_top_n == 50 && !m.rerank && !s.rerank);
    const { assert!(CHUNK_OVERLAP < CHUNK_TOKENS) };
}

#[test]
fn fp_files_are_portable_int8_is_cpu_pinned() {
    for tr in Tier::ALL {
        let p = TierProfile::for_tier(tr);
        let f = p.file_for_new_index(true);
        assert_eq!(ep_for_file(&f, true), EpActive::DirectMl);
        assert_eq!(ep_for_file(&f, false), EpActive::Cpu);
    }
    assert_eq!(ep_for_file(&files::BGE_SMALL_INT8, true), EpActive::Cpu);
}

#[test]
fn gpu_batch_is_capped_by_vram() {
    let t = t();
    let s = TierProfile::for_tier(Tier::Standard);
    assert_eq!(s.gpu_batch_for_vram(8 * GIB, &t), 64);
    assert_eq!(s.gpu_batch_for_vram(4 * GIB, &t), 32);
    assert_eq!(s.gpu_batch_for_vram(GIB, &t), 16);
    let h = TierProfile::for_tier(Tier::High);
    assert_eq!(h.gpu_batch_for_vram(0, &t), 8, "never below the CPU batch");
    assert_eq!(s.batch(EpActive::Cpu), 8);
    assert_eq!(TierProfile::cpu_threads(6), 4);
    assert_eq!(TierProfile::cpu_threads(1), 1);
}

#[test]
fn ann_partitions_are_clamped() {
    let a = TierProfile::for_tier(Tier::Minimum).ann;
    assert_eq!(a.num_partitions(0), 1);
    // M1-E measured 61 partitions at 1M rows (16 384 rows per partition); W2-H R-b resolved.
    assert_eq!(a.num_partitions(1_000_000), 61);
    assert_eq!(a.num_partitions(50_000), 3);
    assert_eq!(a.num_partitions(u64::MAX), 4096);
    assert!(!a.wants_index(49_999) && a.wants_index(50_000));
}

#[test]
fn ann_defaults_are_one_source_of_truth() {
    // CO-1: every M1-E-measured knob (m1-e-core.md §5, §8; Amendment 4.7) lives here, once.
    let a = AnnParams::default();
    assert_eq!(a, ANN_DEFAULT);
    assert_eq!(TierProfile::for_tier(Tier::Minimum).ann, ANN_DEFAULT);
    assert_eq!(a.index_type, AnnIndexType::IvfHnswSq);
    assert_eq!(a.min_rows, 50_000);
    assert_eq!(a.rows_per_partition, 16_384);
    assert_eq!((a.hnsw_m, a.hnsw_ef_construction), (20, 150));
    assert_eq!((a.nprobes, a.ef, a.refine_factor), (16, 100, 5));
    // Partial scopes: exact scan below 5 %, else widen by clamp(1/selectivity, 4, 20), ef ≤ 2000.
    assert!((a.flat_scan_selectivity - 0.05).abs() < f64::EPSILON);
    assert!((a.partial_widen_min - 4.0).abs() < f64::EPSILON);
    assert!((a.partial_widen_max - 20.0).abs() < f64::EPSILON);
    assert_eq!(a.max_ef, 2_000);
    assert!((a.retrain_growth - 2.0).abs() < f64::EPSILON);
    assert!((a.reindex_unindexed_fraction - 0.05).abs() < f64::EPSILON);
    // Larger tiers only override query/build breadth, never the scope rules.
    for tr in Tier::ALL {
        let p = TierProfile::for_tier(tr).ann;
        assert_eq!(p.rows_per_partition, a.rows_per_partition);
        assert_eq!(p.flat_scan_selectivity, a.flat_scan_selectivity);
        assert!(p.partial_widen_min <= p.partial_widen_max);
        assert!(p.max_ef >= p.ef);
    }
}

#[test]
fn decision_round_trips_for_meta_storage() {
    let d = dec(&hw(32, 8, true, 100, vec![dgpu(8 * GIB)]));
    let j = serde_json::to_string(&d).unwrap();
    assert!(j.contains(r#""kind":"direct_ml""#), "{j}");
    let back: TierDecision = serde_json::from_str(&j).unwrap();
    assert_eq!(back, d);
    assert_eq!(Tier::parse("standard"), Some(Tier::Standard));
    assert_eq!(Tier::High.label(), "Best");
}

#[test]
fn ann_query_defaults_are_the_m1_measured_ones() {
    // M1-E §8 / Amendment 4.7: nprobes 16, ef 100, refine 5 (1M × 384, recall@10 0.999).
    let a = TierProfile::for_tier(Tier::Minimum).ann;
    assert_eq!((a.nprobes, a.ef, a.refine_factor), (16, 100, 5));
    for tr in Tier::ALL {
        let p = TierProfile::for_tier(tr).ann;
        // Larger tiers never search less than the measured baseline.
        assert!(p.nprobes >= a.nprobes && p.ef >= a.ef && p.refine_factor >= a.refine_factor);
        assert!(p.hnsw_ef_construction >= p.hnsw_m);
    }
}

// ------------------------------------------------------------------ M2-G: GPU policy (§2.8)
const H: i64 = 60 * 60 * 1000;

fn dec_for(g: GpuInfo) -> (TierDecision, Vec<GpuInfo>) {
    let gpus = vec![g];
    (dec(&hw(16, 8, true, 100, gpus.clone())), gpus)
}

#[test]
fn gpu_info_without_driver_version_deserialises() {
    // meta.hw_detection rows written by M1 have no driver_version.
    let mut v = serde_json::to_value(dgpu(8 * GIB)).unwrap();
    v.as_object_mut().unwrap().remove("driver_version");
    let g: GpuInfo = serde_json::from_value(v).unwrap();
    assert_eq!(g.driver_version, 0);
    assert_eq!(dgpu(0).driver_version_string(), "31.0.3840.1");
}

#[test]
fn three_faults_in_24h_block_until_driver_changes() {
    let g = dgpu(8 * GIB);
    let k = GpuKey::of(&g);
    let mut l = GpuFaultLedger::default();
    assert!(!l.record(k, 0));
    assert!(!l.record(k, 10 * H));
    assert!(!l.blocks(k));
    // Third fault inside the window → blocked.
    assert!(l.record(k, 23 * H));
    assert!(l.blocks(k));
    assert_eq!(l.recent(k, 23 * H), 3);
    // The window passing does NOT lift the block: only a driver change does.
    assert!(l.blocks(k));
    let mut newer = k;
    newer.driver_version += 1;
    assert!(!l.blocks(newer));
    assert!(l.forget_stale_driver(newer));
    assert!(l.blocked.is_none() && l.faults.is_empty());
    assert!(!l.forget_stale_driver(newer), "idempotent");
}

#[test]
fn faults_outside_the_window_do_not_count() {
    let k = GpuKey::of(&dgpu(8 * GIB));
    let mut l = GpuFaultLedger::default();
    l.record(k, 0);
    l.record(k, 1);
    // 24 h later the first two have expired (window is half-open: exactly 24 h = expired).
    assert!(!l.record(k, GPU_FAULT_WINDOW_MS + 1));
    assert!(!l.blocks(k));
    assert_eq!(l.recent(k, GPU_FAULT_WINDOW_MS + 1), 1);
    // Faults of another adapter never count against this one.
    let other = GpuKey { device_id: 9, ..k };
    l.record(other, GPU_FAULT_WINDOW_MS + 2);
    l.record(other, GPU_FAULT_WINDOW_MS + 3);
    assert!(!l.blocks(k));
}

#[test]
fn effective_ep_dedicated_off_switch_fault_limit_and_gone() {
    let (d, gpus) = dec_for(dgpu(8 * GIB));
    assert_eq!(d.ep, EpPlan::DirectMl { dxgi_index: 0 });
    let mut s = GpuState::default();
    assert_eq!(
        effective_ep(&d, &gpus, true, &s),
        (EpPlan::DirectMl { dxgi_index: 0 }, EpPlanReason::Detected)
    );
    assert_eq!(
        effective_ep(&d, &gpus, false, &s),
        (EpPlan::Cpu, EpPlanReason::DisabledByUser)
    );
    let k = GpuKey::of(&gpus[0]);
    for t in 0..3 {
        s.ledger.record(k, t);
    }
    assert_eq!(
        effective_ep(&d, &gpus, true, &s),
        (EpPlan::Cpu, EpPlanReason::FaultLimit)
    );
    // Driver update → GPU again.
    let mut upd = gpus.clone();
    upd[0].driver_version += 1;
    assert_eq!(
        effective_ep(&d, &upd, true, &s).0,
        EpPlan::DirectMl { dxgi_index: 0 }
    );
    // Adapter unplugged (eGPU) or index moved.
    assert_eq!(
        effective_ep(&d, &[], true, &GpuState::default()),
        (EpPlan::Cpu, EpPlanReason::AdapterGone)
    );
    // CPU plan stays CPU whatever the switch says.
    let (cpu, _) = dec_for(warp());
    assert_eq!(
        effective_ep(&cpu, &[warp()], true, &s),
        (EpPlan::Cpu, EpPlanReason::Detected)
    );
}

#[test]
fn igpu_probe_is_cached_per_driver_version() {
    let (d, gpus) = dec_for(igpu(512 * MIB));
    assert_eq!(d.ep, EpPlan::ProbeDirectMl { dxgi_index: 0 });
    let mut s = GpuState::default();
    assert_eq!(
        effective_ep(&d, &gpus, true, &s),
        (
            EpPlan::ProbeDirectMl { dxgi_index: 0 },
            EpPlanReason::ProbePending
        )
    );
    let k = GpuKey::of(&gpus[0]);
    s.probes.put(ProbeResult {
        key: k,
        use_gpu: true,
        speedup: Some(1.8),
        at_ms: 0,
    });
    assert_eq!(
        effective_ep(&d, &gpus, true, &s),
        (
            EpPlan::DirectMl { dxgi_index: 0 },
            EpPlanReason::ProbeCachedFast
        )
    );
    s.probes.put(ProbeResult {
        key: k,
        use_gpu: false,
        speedup: Some(0.9),
        at_ms: 1,
    });
    assert_eq!(s.probes.entries.len(), 1, "one entry per adapter");
    assert_eq!(
        effective_ep(&d, &gpus, true, &s),
        (EpPlan::Cpu, EpPlanReason::ProbeCachedSlow)
    );
    // New driver → probe again.
    let mut upd = gpus.clone();
    upd[0].driver_version += 1;
    assert_eq!(
        effective_ep(&d, &upd, true, &s).1,
        EpPlanReason::ProbePending
    );
    // The cached probe never changes the tier (an iGPU never raises it).
    assert_eq!(d.tier, Tier::Standard); // 8 cores, CPU rule
    let st = GpuState {
        probes: s.probes.clone(),
        ..GpuState::default()
    };
    let j = serde_json::to_string(&st).unwrap();
    assert_eq!(serde_json::from_str::<GpuState>(&j).unwrap(), st);
    assert_eq!(
        serde_json::from_str::<GpuState>("{}").unwrap(),
        GpuState::default()
    );
}

#[test]
fn batch_governor_halves_slow_batches_after_warmup() {
    let mut g = GpuBatchGovernor::new(64, 8);
    assert_eq!(g.batch(), 64);
    // First full batch at a size is a warm-up (DirectML compiles per shape): never judged.
    assert_eq!(g.observe(64, 5_000), None);
    assert_eq!(g.observe(64, 450), None);
    assert_eq!(g.observe(64, 501), Some(32));
    // Short tail batches are ignored.
    assert_eq!(g.observe(10, 9_000), None);
    assert_eq!(g.observe(32, 900), None, "warm-up at the new size");
    assert_eq!(g.observe(32, 900), Some(16));
    assert_eq!(g.observe(16, 900), None);
    assert_eq!(g.observe(16, 900), Some(8));
    assert_eq!(g.observe(8, 900), None);
    assert_eq!(g.observe(8, 900), None, "floor");
    assert_eq!(g.batch(), 8);
    assert_eq!(
        GpuBatchGovernor::new(4, 8).batch(),
        8,
        "start below floor is raised"
    );
}

#[test]
fn recommendation_line_and_sizes() {
    assert_eq!(display_mb(files::ARCTIC_M15_FP16.bytes), 208);
    assert_eq!(display_mb(files::ARCTIC_L_FP16.bytes), 638);
    let r = recommendation(Tier::Standard, Tier::Minimum, true).unwrap();
    assert_eq!(
        r.text,
        "Recommended for your computer: Better search (208 MB download)"
    );
    assert_eq!(r.file, STANDARD.gpu_file);
    assert_eq!(r.download_bytes, STANDARD.gpu_file.bytes);
    // CPU-only Standard builds with the fp32 file (3.4).
    let c = recommendation(Tier::Standard, Tier::Minimum, false).unwrap();
    assert_eq!(c.file, STANDARD.cpu_file);
    assert!(c.text.ends_with("(416 MB download)"), "{}", c.text);
    let h = recommendation(Tier::High, Tier::Standard, true).unwrap();
    assert_eq!(
        h.text,
        "Recommended for your computer: Best search (638 MB download)"
    );
    // Never downwards, never for the tier in use.
    assert!(recommendation(Tier::Minimum, Tier::Minimum, true).is_none());
    assert!(recommendation(Tier::Standard, Tier::High, true).is_none());
}

#[test]
fn upgrade_disk_preflight_formula() {
    // model + rows × dims × 4 × 1.3 + 2 GiB
    assert_eq!(upgrade_disk_needed(0, 0, 768), 2 * GIB);
    let n = upgrade_disk_needed(218_155_826, 100_000, 768);
    assert_eq!(n, 218_155_826 + 307_200_000 + 92_160_000 + 2 * GIB);
    assert_eq!(upgrade_disk_needed(u64::MAX, u64::MAX, 1024), u64::MAX);
}

// ------------------------------------------------------------------ Amendment 18: Apple silicon
fn mac(h: &HwInfo) -> TierDecision {
    select_tier_on(h, &t(), TierPlatform::MACOS_ARM64)
}

#[test]
fn macos_never_uses_directml_and_ignores_adapters() {
    // Whatever an adapter list says, there is no DirectML on macOS: CPU plan, never High.
    for gpus in [vec![], vec![dgpu(16 * GIB)], vec![igpu(GIB)]] {
        let d = mac(&hw(64, 12, false, 500, gpus));
        assert_eq!((d.ep, d.gpu_class), (EpPlan::Cpu, GpuClass::None));
        assert_eq!(d.tier, Tier::Standard, "{:?}", d.reasons);
        let (plan, _) = effective_ep(&d, &[dgpu(16 * GIB)], true, &GpuState::default());
        assert_eq!(plan, EpPlan::Cpu);
    }
}

#[test]
fn macos_tier_follows_ram_cores_and_disk_without_avx2() {
    // `avx2` is false on aarch64; NEON qualifies instead.
    assert_eq!(mac(&hw(8, 8, false, 100, vec![])).tier, Tier::Standard); // M1/M2 8 GB
    assert_eq!(mac(&hw(16, 10, false, 100, vec![])).tier, Tier::Standard);
    assert_eq!(mac(&hw(7, 8, false, 100, vec![])).tier, Tier::Minimum);
    assert_eq!(mac(&hw(16, 4, false, 100, vec![])).tier, Tier::Minimum);
    assert_eq!(mac(&hw(16, 8, false, 9, vec![])).tier, Tier::Minimum);
    let d = mac(&hw(8, 8, false, 100, vec![]));
    assert!(
        !d.reasons.iter().any(|r| r.contains("AVX2")),
        "{:?}",
        d.reasons
    );
    // The same machine on Windows x64 without AVX2 stays at Minimum (unchanged).
    assert_eq!(dec(&hw(8, 8, false, 100, vec![])).tier, Tier::Minimum);
}

#[test]
fn current_platform_matches_the_build() {
    let p = TierPlatform::current();
    assert_eq!(p.directml, cfg!(windows));
    #[cfg(all(windows, target_arch = "x86_64"))]
    assert_eq!(p, TierPlatform::WINDOWS_X64);
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    assert_eq!(p, TierPlatform::MACOS_ARM64);
}
