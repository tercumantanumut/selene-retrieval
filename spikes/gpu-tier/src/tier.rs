//! Pure tier selection. No I/O: `select_tier(&HwInfo, &Thresholds) -> TierDecision`.
//! Proposed home: `sr-common::tier` (M1). Values are fixed in docs/research/m0-e-gpu-tier.md.

use serde::Serialize;

pub const GIB: u64 = 1024 * 1024 * 1024;
pub const MIB: u64 = 1024 * 1024;

/// Microsoft Basic Render Driver (WARP) PCI ids. Also excluded by `DXGI_ADAPTER_FLAG_SOFTWARE`.
pub const MS_VENDOR_ID: u32 = 0x1414;
pub const MS_BASIC_RENDER_DEVICE_ID: u32 = 0x008c;

#[derive(Debug, Clone, Serialize)]
pub struct GpuInfo {
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub dedicated_vram: u64,
    pub shared_mem: u64,
    /// DXGI_ADAPTER_FLAG_SOFTWARE.
    pub is_software: bool,
    /// D3D12CreateDevice(adapter, D3D_FEATURE_LEVEL_11_0) succeeded (DirectML minimum).
    pub d3d12_fl11: bool,
    /// D3D12_FEATURE_DATA_ARCHITECTURE1.UMA: integrated GPU / APU (shares system RAM).
    pub uma: bool,
    /// DXGI enumeration index (= DirectML device_id).
    pub dxgi_index: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct HwInfo {
    /// ullTotalPhys rounded to the nearest GiB (`round_ram`).
    pub total_ram: u64,
    pub physical_cores: u32,
    pub logical_cores: u32,
    pub avx2: bool,
    pub avx512_vnni: bool,
    /// Free bytes on the volume holding the data dir.
    pub free_disk: u64,
    pub gpus: Vec<GpuInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Tier {
    Minimum,
    Standard,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum GpuClass {
    /// No usable adapter (none, software/WARP, or no D3D12 FL 11.0).
    None,
    /// UMA adapter (iGPU / APU), or a dedicated adapter below `dgpu_min_vram`.
    /// May or may not beat the CPU: decided by a one-off timed probe.
    Integrated,
    /// Non-UMA adapter with >= `dgpu_min_vram` dedicated memory.
    Dedicated,
}

/// Execution-provider plan. Every GPU plan has a silent CPU fallback at runtime
/// (registration error, canary failure, device removed) without user action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum EpPlan {
    Cpu,
    /// Use DirectML on this adapter.
    DirectMl { dxgi_index: u32 },
    /// Time a 2-batch canary on DirectML and on CPU once (cached per driver version);
    /// use DirectML when it is >= `probe_min_speedup` x the CPU, else CPU.
    ProbeDirectMl { dxgi_index: u32 },
}

/// Cut-offs. Comparisons are `>=` (at the threshold qualifies).
#[derive(Debug, Clone, Serialize)]
pub struct Thresholds {
    pub dgpu_min_vram: u64,
    /// High's fp16 1024-dim model measured ~3.0-3.3 GB per process (M0-E); 6 GiB leaves room for the
    /// desktop and other apps.
    pub high_min_vram: u64,
    pub std_min_ram: u64,
    pub std_min_cores_cpu_only: u32,
    pub std_min_free_disk: u64,
    pub high_min_ram: u64,
    pub high_min_free_disk: u64,
    pub probe_min_speedup: f32,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            dgpu_min_vram: 4 * GIB,
            high_min_vram: 6 * GIB,
            std_min_ram: 8 * GIB,
            std_min_cores_cpu_only: 6,
            std_min_free_disk: 10 * GIB,
            high_min_ram: 16 * GIB,
            high_min_free_disk: 25 * GIB,
            probe_min_speedup: 1.2,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TierDecision {
    pub tier: Tier,
    pub ep: EpPlan,
    pub gpu_class: GpuClass,
    /// Plain-language reasons (Details / diagnostics only; never in the basic UI).
    pub reasons: Vec<String>,
}

fn basic_render(g: &GpuInfo) -> bool {
    g.vendor_id == MS_VENDOR_ID && g.device_id == MS_BASIC_RENDER_DEVICE_ID
}

/// Usable by DirectML at all.
pub fn gpu_usable(g: &GpuInfo) -> bool {
    !g.is_software && !basic_render(g) && g.d3d12_fl11
}

pub fn classify(g: &GpuInfo, t: &Thresholds) -> GpuClass {
    if !gpu_usable(g) {
        GpuClass::None
    } else if !g.uma && g.dedicated_vram >= t.dgpu_min_vram {
        GpuClass::Dedicated
    } else {
        GpuClass::Integrated
    }
}

/// Best adapter: Dedicated beats Integrated; then most dedicated VRAM; ties -> lowest DXGI index.
pub fn best_gpu<'a>(hw: &'a HwInfo, t: &Thresholds) -> Option<(&'a GpuInfo, GpuClass)> {
    hw.gpus
        .iter()
        .map(|g| (g, classify(g, t)))
        .filter(|(_, c)| *c != GpuClass::None)
        .max_by(|(a, ca), (b, cb)| {
            (*ca == GpuClass::Dedicated)
                .cmp(&(*cb == GpuClass::Dedicated))
                .then(a.dedicated_vram.cmp(&b.dedicated_vram))
                .then(b.dxgi_index.cmp(&a.dxgi_index))
        })
}

pub fn select_tier(hw: &HwInfo, t: &Thresholds) -> TierDecision {
    let mut reasons = Vec::new();
    let best = best_gpu(hw, t);
    let vram = best.map(|(g, _)| g.dedicated_vram).unwrap_or(0);
    let (ep, gpu_class) = match best {
        Some((g, GpuClass::Dedicated)) => {
            reasons.push(format!("graphics card '{}' ({} MiB) will be used", g.name, g.dedicated_vram / MIB));
            (EpPlan::DirectMl { dxgi_index: g.dxgi_index }, GpuClass::Dedicated)
        }
        Some((g, _)) => {
            reasons.push(format!("integrated graphics '{}': used if faster than the processor", g.name));
            (EpPlan::ProbeDirectMl { dxgi_index: g.dxgi_index }, GpuClass::Integrated)
        }
        None => {
            reasons.push("no suitable graphics card; using the processor".into());
            (EpPlan::Cpu, GpuClass::None)
        }
    };
    let dgpu = gpu_class == GpuClass::Dedicated;

    // Gates shared by every tier above Minimum.
    let gates = |min_ram: u64, min_disk: u64| -> Vec<String> {
        let mut r = Vec::new();
        if !hw.avx2 {
            r.push("processor lacks AVX2".into());
        }
        if hw.total_ram < min_ram {
            r.push(format!("memory {} GiB < {} GiB", hw.total_ram / GIB, min_ram / GIB));
        }
        if hw.free_disk < min_disk {
            r.push(format!("free disk {} GiB < {} GiB", hw.free_disk / GIB, min_disk / GIB));
        }
        r
    };

    // High: dedicated GPU only (1024-dim int8 on CPU is too slow for first indexing; see benchmarks).
    let mut high = gates(t.high_min_ram, t.high_min_free_disk);
    if !dgpu || vram < t.high_min_vram {
        high.push(format!("needs a graphics card with >= {} GiB of its own memory", t.high_min_vram / GIB));
    }
    if high.is_empty() {
        reasons.push("High qualifies".into());
        return TierDecision { tier: Tier::High, ep, gpu_class, reasons };
    }
    reasons.extend(high.into_iter().map(|s| format!("not High: {s}")));

    // Standard: dedicated GPU, or enough physical cores. An iGPU never upgrades the tier
    // (it is only used if it beats the CPU, so the CPU rules must hold on their own).
    let mut std = gates(t.std_min_ram, t.std_min_free_disk);
    if !dgpu && hw.physical_cores < t.std_min_cores_cpu_only {
        std.push(format!("{} processor cores < {} (no graphics card)", hw.physical_cores, t.std_min_cores_cpu_only));
    }
    if std.is_empty() {
        reasons.push("Standard qualifies".into());
        return TierDecision { tier: Tier::Standard, ep, gpu_class, reasons };
    }
    reasons.extend(std.into_iter().map(|s| format!("not Standard: {s}")));
    TierDecision { tier: Tier::Minimum, ep, gpu_class, reasons }
}

/// Advanced → manual override. Any tier may be forced; a tier above the recommendation returns
/// a plain warning the UI shows next to the choice (never blocks: the user decided).
pub fn effective_tier(rec: &TierDecision, manual: Option<Tier>) -> (Tier, Option<&'static str>) {
    match manual {
        None => (rec.tier, None),
        Some(m) if m <= rec.tier => (m, None),
        Some(m) => (m, Some("This computer may index slowly with this choice. Search keeps working while it runs.")),
    }
}

// ------------------------------------------------------------------------------------------- profile

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Precision {
    Fp32,
    Fp16,
    Int8,
}

/// One model file. `sha256` is checked at load (bundled) or after download.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ModelFile {
    pub hf_repo: &'static str,
    pub hf_revision: &'static str,
    pub path: &'static str,
    pub bytes: u64,
    pub precision: Precision,
    pub sha256: &'static str,
}

/// EP an index's file may run on. fp32/fp16 are EP-portable (CPU vs DirectML cos >= 0.99998 measured for
/// bge-small, arctic-m, arctic-l, gte-large). int8 is NOT: DirectML int8 vs CPU int8 measured cos 0.991-0.997,
/// below the 0.999 bar, so an int8 index is embedded and queried on the CPU only.
pub fn ep_for_file(f: &ModelFile, gpu_available: bool) -> EpActive {
    match (f.precision, gpu_available) {
        (Precision::Int8, _) | (_, false) => EpActive::Cpu,
        _ => EpActive::DirectMl,
    }
}

/// File for a NEW index build: GPU file when the plan resolved to DirectML, else the CPU file.
/// Afterwards the index is sticky to this file (see docs/02-architecture.md §2.8).
pub fn file_for_new_index(p: &TierProfile, gpu_resolved: bool) -> ModelFile {
    if gpu_resolved { p.gpu_file } else { p.cpu_file }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct AnnParams {
    /// Below this row count: flat scan, no index.
    pub min_rows: u32,
    /// IVF partitions = clamp(round(sqrt(rows) * factor), 1, 4096).
    pub partitions_per_sqrt_rows: f32,
    pub hnsw_m: u32,
    pub hnsw_ef_construction: u32,
    pub nprobes: u32,
    pub refine_factor: u32,
}

/// Everything that scales with the tier. Chunking is deliberately NOT here: chunk_cfg_fp (and so
/// chunk IDs) must not change on a tier change. Every candidate shares the bert-base-uncased
/// WordPiece vocab, so one chunker tokenizer serves all tiers.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct TierProfile {
    pub tier: Tier,
    pub model_id: &'static str,
    pub dims: u32,
    pub pooling: &'static str,
    pub query_prefix: &'static str,
    pub max_seq_tokens: u32,
    /// Variant used when the index is built on DirectML.
    pub gpu_file: ModelFile,
    /// Variant used when the index is built on CPU.
    pub cpu_file: ModelFile,
    pub batch_gpu: u32,
    pub batch_cpu: u32,
    /// Batches in flight. GPU: 2 (tokenise batch N+1 while N runs). CPU: 1 session, intra-op threads
    /// = max(1, physical_cores - 2).
    pub concurrency_gpu: u32,
    pub concurrency_cpu: u32,
    pub ann: AnnParams,
    pub rerank: bool,
    pub rerank_top_n: u32,
}

impl TierProfile {
    pub fn batch(&self, ep: EpActive) -> u32 {
        match ep {
            EpActive::Cpu => self.batch_cpu,
            EpActive::DirectMl => self.batch_gpu,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum EpActive {
    Cpu,
    DirectMl,
}

pub const CHUNK_TOKENS: u32 = 400;
pub const CHUNK_OVERLAP: u32 = 48;

include!("profiles.rs");

/// Round reported physical memory to the nearest GiB (an "8 GB" machine reports ~7.8 GiB).
pub fn round_ram(bytes: u64) -> u64 {
    ((bytes + GIB / 2) / GIB) * GIB
}

#[cfg(test)]
mod tests {
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
        }
    }
    fn igpu(carve_out: u64) -> GpuInfo {
        GpuInfo { name: "Test iGPU".into(), vendor_id: 0x8086, uma: true, dedicated_vram: carve_out, ..dgpu(0) }
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
    fn dec(h: &HwInfo) -> TierDecision {
        select_tier(h, &t())
    }
    fn tier(h: &HwInfo) -> Tier {
        dec(h).tier
    }

    // ------------------------------------------------------------- GPU cases
    #[test]
    fn no_gpu_cpu_rules() {
        let d = dec(&hw(16, 8, true, 100, vec![]));
        assert_eq!((d.tier, d.ep, d.gpu_class), (Tier::Standard, EpPlan::Cpu, GpuClass::None));
        assert_eq!(tier(&hw(16, 4, true, 100, vec![])), Tier::Minimum);
        // Never High without a dedicated GPU, however strong the CPU.
        assert_eq!(tier(&hw(128, 32, true, 2000, vec![])), Tier::Standard);
    }

    #[test]
    fn integrated_gpu_probe_and_no_tier_upgrade() {
        // 4-core laptop with Iris Xe: probe DirectML, but tier follows the CPU rules (Minimum).
        let d = dec(&hw(16, 4, true, 100, vec![igpu(128 * MIB)]));
        assert_eq!(d.ep, EpPlan::ProbeDirectMl { dxgi_index: 0 });
        assert_eq!((d.tier, d.gpu_class), (Tier::Minimum, GpuClass::Integrated));
        // Same iGPU, 8 cores: Standard via the CPU rule; still never High.
        let d = dec(&hw(32, 8, true, 100, vec![igpu(128 * MIB)]));
        assert_eq!((d.tier, d.gpu_class), (Tier::Standard, GpuClass::Integrated));
    }

    #[test]
    fn apu_with_large_carve_out_is_integrated() {
        // AMD APU with an 8 GiB BIOS frame buffer reported as "dedicated": UMA wins.
        let d = dec(&hw(32, 8, true, 100, vec![igpu(8 * GIB)]));
        assert_eq!(d.gpu_class, GpuClass::Integrated);
        assert_ne!(d.tier, Tier::High);
    }

    #[test]
    fn dedicated_vram_below_at_above_4gib() {
        let m = t().dgpu_min_vram;
        assert_eq!(m, 4 * GIB);
        // Below: treated like an iGPU (probe), and a 4-core box stays Minimum.
        let d = dec(&hw(16, 4, true, 100, vec![dgpu(m - 1)]));
        assert_eq!((d.gpu_class, d.tier), (GpuClass::Integrated, Tier::Minimum));
        assert_eq!(d.ep, EpPlan::ProbeDirectMl { dxgi_index: 0 });
        // At / above: dedicated → DirectML by default; Standard even on 4 cores (the GPU does the work).
        for v in [m, m + 1] {
            let d = dec(&hw(16, 4, true, 100, vec![dgpu(v)]));
            assert_eq!((d.gpu_class, d.tier), (GpuClass::Dedicated, Tier::Standard));
            assert_eq!(d.ep, EpPlan::DirectMl { dxgi_index: 0 });
        }
    }

    #[test]
    fn high_vram_below_at_above_6gib() {
        let m = t().high_min_vram;
        assert_eq!(m, 6 * GIB);
        assert_eq!(tier(&hw(32, 8, true, 100, vec![dgpu(m - 1)])), Tier::Standard);
        assert_eq!(tier(&hw(32, 8, true, 100, vec![dgpu(m)])), Tier::High);
        assert_eq!(tier(&hw(32, 8, true, 100, vec![dgpu(m + 1)])), Tier::High);
    }

    #[test]
    fn dedicated_gpu_with_8gb_ram_is_standard_even_with_2_cores() {
        let d = dec(&hw(8, 2, true, 100, vec![dgpu(6 * GIB)]));
        assert_eq!((d.tier, d.ep), (Tier::Standard, EpPlan::DirectMl { dxgi_index: 0 }));
    }

    #[test]
    fn warp_only_is_no_gpu() {
        let d = dec(&hw(32, 8, true, 100, vec![warp()]));
        assert_eq!((d.ep, d.gpu_class, d.tier), (EpPlan::Cpu, GpuClass::None, Tier::Standard));
        // A broken driver reporting VRAM and no software flag is still excluded by PCI id.
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
    }

    #[test]
    fn hybrid_laptop_prefers_dedicated_over_igpu() {
        let mut i = igpu(512 * MIB);
        i.dxgi_index = 0;
        let mut d = dgpu(8 * GIB);
        d.dxgi_index = 1;
        let r = dec(&hw(32, 8, true, 100, vec![i, d, warp()]));
        assert_eq!((r.ep, r.tier), (EpPlan::DirectMl { dxgi_index: 1 }, Tier::High));
        // Even if the iGPU reported more "dedicated" memory (APU carve-out).
        let mut i = igpu(16 * GIB);
        i.dxgi_index = 0;
        let r = dec(&hw(32, 8, true, 100, vec![i, dgpu(4 * GIB)]));
        assert_eq!((r.gpu_class, r.tier), (GpuClass::Dedicated, Tier::Standard));
    }

    // ------------------------------------------------------------- AVX2 missing
    #[test]
    fn no_avx2_is_minimum_even_with_dgpu_but_gpu_still_used() {
        assert_eq!(tier(&hw(64, 16, false, 500, vec![])), Tier::Minimum);
        let d = dec(&hw(64, 16, false, 500, vec![dgpu(16 * GIB)]));
        assert_eq!(d.tier, Tier::Minimum);
        assert_eq!(d.ep, EpPlan::DirectMl { dxgi_index: 0 });
        assert!(d.reasons.iter().any(|r| r.contains("AVX2")));
    }

    // ------------------------------------------------------------- RAM / cores / disk boundaries
    #[test]
    fn std_ram_below_at_above() {
        let mut h = hw(0, 6, true, 100, vec![]);
        for (ram, want) in [(t().std_min_ram - 1, Tier::Minimum), (t().std_min_ram, Tier::Standard), (t().std_min_ram + 1, Tier::Standard)] {
            h.total_ram = ram;
            assert_eq!(tier(&h), want, "ram={ram}");
        }
    }

    #[test]
    fn high_ram_below_at_above() {
        let mut h = hw(0, 8, true, 100, vec![dgpu(8 * GIB)]);
        for (ram, want) in [(t().high_min_ram - 1, Tier::Standard), (t().high_min_ram, Tier::High), (t().high_min_ram + 1, Tier::High)] {
            h.total_ram = ram;
            assert_eq!(tier(&h), want, "ram={ram}");
        }
    }

    #[test]
    fn eight_gb_laptop_counts_as_eight() {
        assert_eq!(round_ram(8 * GIB - 300 * MIB), 8 * GIB); // 7.7 GiB reported
        assert_eq!(round_ram(8 * GIB - 600 * MIB), 7 * GIB); // 7.4 GiB: really a smaller machine
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
        for (d, want) in [(t().std_min_free_disk - 1, Tier::Minimum), (t().std_min_free_disk, Tier::Standard), (t().std_min_free_disk + 1, Tier::Standard)] {
            h.free_disk = d;
            assert_eq!(tier(&h), want);
        }
    }

    #[test]
    fn high_disk_below_at_above() {
        let mut h = hw(32, 8, true, 0, vec![dgpu(8 * GIB)]);
        for (d, want) in [(t().high_min_free_disk - 1, Tier::Standard), (t().high_min_free_disk, Tier::High), (t().high_min_free_disk + 1, Tier::High)] {
            h.free_disk = d;
            assert_eq!(tier(&h), want);
        }
    }

    // ------------------------------------------------------------- override + profiles
    #[test]
    fn manual_override() {
        let rec = dec(&hw(16, 4, true, 100, vec![]));
        assert_eq!(rec.tier, Tier::Minimum);
        assert_eq!(effective_tier(&rec, None), (Tier::Minimum, None));
        let (t2, warn) = effective_tier(&rec, Some(Tier::High));
        assert_eq!(t2, Tier::High);
        assert!(warn.is_some());
        let rec = dec(&hw(32, 8, true, 100, vec![dgpu(8 * GIB)]));
        assert_eq!(effective_tier(&rec, Some(Tier::Minimum)), (Tier::Minimum, None));
    }

    #[test]
    fn profiles_are_consistent() {
        assert!(profile(Tier::Minimum).dims < profile(Tier::Standard).dims);
        assert!(profile(Tier::Standard).dims < profile(Tier::High).dims);
        assert_eq!(profile(Tier::Minimum).dims, 384);
        for tr in [Tier::Minimum, Tier::Standard, Tier::High] {
            let p = profile(tr);
            assert_eq!(p.tier, tr);
            // Chunk + query prefix must fit the model window without truncation.
            assert!(CHUNK_TOKENS + 16 <= p.max_seq_tokens);
            assert!(p.batch_gpu >= p.batch_cpu);
            assert_eq!(p.gpu_file.sha256.len(), 64);
            assert_eq!(p.cpu_file.sha256.len(), 64);
            assert!(p.ann.refine_factor >= 1 && p.ann.nprobes >= 1);
        }
        assert!(CHUNK_OVERLAP < CHUNK_TOKENS);
    }

    #[test]
    fn int8_is_cpu_pinned_fp_is_portable() {
        for tr in [Tier::Minimum, Tier::Standard, Tier::High] {
            let p = profile(tr);
            assert_ne!(p.gpu_file.precision, Precision::Int8, "GPU builds must use an EP-portable file");
            let f = file_for_new_index(&p, true);
            assert_eq!(ep_for_file(&f, true), EpActive::DirectMl);
            assert_eq!(ep_for_file(&f, false), EpActive::Cpu); // GPU gone: same file on CPU, no re-embed
            let c = file_for_new_index(&p, false);
            if c.precision == Precision::Int8 {
                assert_eq!(ep_for_file(&c, true), EpActive::Cpu); // GPU appears later: int8 index stays on CPU
            }
        }
    }
}
