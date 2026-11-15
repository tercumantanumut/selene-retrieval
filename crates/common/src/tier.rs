//! Hardware tiers (Amendments 1, 2 and 3.4–3.6; docs/02-architecture.md §2.8).
//!
//! * [`select_tier`] is **pure**: `(&HwInfo, &Thresholds) -> TierDecision`. Detection (DXGI/D3D12,
//!   CPUID, `GlobalMemoryStatusEx`, free disk) lives in the engine; this module only decides.
//! * Every numeric value comes from `docs/research/m0-e-gpu-tier.md` (authoritative), amended by
//!   `docs/00-review-and-decisions.md` Amendment 3:
//!   - 3.4: Minimum ships **bge-small fp32 `model_optimized.onnx`** (Qdrant, 66 MB) for both CPU
//!     and GPU. int8 is off for every tier until the M1 eval harness clears it (≤ 1.0 nDCG@10
//!     loss); CPU-only Standard/High use fp32.
//!   - 3.5: chunking is **400 / 48 tokens for every tier** ([`CHUNK_TOKENS`], [`CHUNK_OVERLAP`]),
//!     so a tier change never changes chunk IDs.
//!   - 3.6: Standard = AVX2 + 8 GiB RAM + 10 GiB disk + (dGPU ≥ 4 GiB or ≥ 6 physical cores);
//!     High = AVX2 + 16 GiB RAM + 25 GiB disk + **dGPU ≥ 6 GiB**.
//! * All comparisons are `>=` (at the threshold qualifies).
//! * Amendment 18 (macOS on Apple silicon): there is no DirectML, so adapters are ignored and
//!   embeddings run on the processor (never High); aarch64's baseline NEON stands in for AVX2.
//!   [`TierPlatform`] carries both facts; Windows x64 decisions are unchanged.
//! * The profile in force is the **active index's** profile, never the recommended one (§2.8).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const GIB: u64 = 1024 * 1024 * 1024;
pub const MIB: u64 = 1024 * 1024;

/// Microsoft Basic Render Driver (WARP) PCI ids. Also excluded by `DXGI_ADAPTER_FLAG_SOFTWARE`.
pub const MS_VENDOR_ID: u32 = 0x1414;
pub const MS_BASIC_RENDER_DEVICE_ID: u32 = 0x008c;

/// Chunk size in tokens. **Not per tier**: `chunk_cfg_fp` (and so every chunk ID) depends on it.
pub const CHUNK_TOKENS: u32 = 400;
/// Chunk overlap in tokens. Not per tier (see [`CHUNK_TOKENS`]).
pub const CHUNK_OVERLAP: u32 = 48;
/// The one chunker tokenizer. Every tier model shares the bert-base-uncased WordPiece vocab
/// (30 522 entries, vocab sha `f21ba7d9e039`, verified M0-E), so this id goes into `chunk_cfg_fp`.
pub const CHUNKER_TOKENIZER_ID: &str = "bert-base-uncased-wordpiece@f21ba7d9e039";

// ------------------------------------------------------------------------------------- hardware

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GpuInfo {
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub dedicated_vram: u64,
    pub shared_mem: u64,
    /// `DXGI_ADAPTER_FLAG_SOFTWARE`.
    pub is_software: bool,
    /// `D3D12CreateDevice(adapter, D3D_FEATURE_LEVEL_11_0)` succeeded (DirectML minimum).
    pub d3d12_fl11: bool,
    /// `D3D12_FEATURE_DATA_ARCHITECTURE1.UMA`: integrated GPU / APU (shares system RAM).
    pub uma: bool,
    /// DXGI enumeration index (= DirectML device id).
    pub dxgi_index: u32,
    /// User-mode driver version from `IDXGIAdapter::CheckInterfaceSupport`, packed as four u16
    /// (`a.b.c.d`, high to low). 0 = unknown. Keys the iGPU probe cache and the 3-faults rule
    /// (§2.8: "cached per driver version", "CPU until the driver version changes").
    #[serde(default)]
    pub driver_version: u64,
}

impl GpuInfo {
    /// `a.b.c.d` for diagnostics.
    pub fn driver_version_string(&self) -> String {
        let v = self.driver_version;
        format!(
            "{}.{}.{}.{}",
            (v >> 48) & 0xffff,
            (v >> 32) & 0xffff,
            (v >> 16) & 0xffff,
            v & 0xffff
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HwInfo {
    /// Total physical memory rounded to the nearest GiB ([`round_ram`]).
    pub total_ram: u64,
    pub physical_cores: u32,
    pub logical_cores: u32,
    pub avx2: bool,
    pub avx512_vnni: bool,
    /// Free bytes on the volume holding the data dir.
    pub free_disk: u64,
    pub gpus: Vec<GpuInfo>,
}

/// Round reported physical memory to the nearest GiB (an "8 GB" laptop reports ~7.7 GiB).
pub const fn round_ram(bytes: u64) -> u64 {
    bytes.saturating_add(GIB / 2) / GIB * GIB
}

// ------------------------------------------------------------------------------------- decision

/// Hardware tier. UI labels: Basic / Better / Best ([`Tier::label`]); never shown as tier names.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    Serialize,
    Deserialize,
    JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    #[default]
    Minimum,
    Standard,
    High,
}

impl Tier {
    pub const ALL: [Tier; 3] = [Tier::Minimum, Tier::Standard, Tier::High];

    /// Plain label for the UI.
    pub const fn label(self) -> &'static str {
        match self {
            Tier::Minimum => "Basic",
            Tier::Standard => "Better",
            Tier::High => "Best",
        }
    }

    /// Stable catalog value (`embedding_profiles.tier`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Tier::Minimum => "minimum",
            Tier::Standard => "standard",
            Tier::High => "high",
        }
    }

    pub fn parse(s: &str) -> Option<Tier> {
        Tier::ALL.into_iter().find(|t| t.as_str() == s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GpuClass {
    /// No usable adapter (none, software/WARP, or no D3D12 FL 11.0).
    None,
    /// UMA adapter (iGPU / APU), or a dedicated adapter below `dgpu_min_vram`. Used only if a
    /// one-off timed probe shows it beats the CPU. **Never raises the tier.**
    Integrated,
    /// Non-UMA adapter with at least `dgpu_min_vram` dedicated memory.
    Dedicated,
}

/// Execution-provider plan. Every GPU plan has a silent CPU fallback at runtime (registration
/// error, canary failure, device removed); none of them is ever a user-visible error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EpPlan {
    Cpu,
    /// DirectML on this adapter.
    DirectMl {
        dxgi_index: u32,
    },
    /// Time a 2-batch canary on DirectML and the CPU once (cached per driver version); DirectML
    /// only if it is at least `probe_min_speedup`× faster and cos(DML, CPU) ≥ 0.999.
    ProbeDirectMl {
        dxgi_index: u32,
    },
}

/// The EP a session actually runs on (after probe / fallback).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EpActive {
    Cpu,
    DirectMl,
}

/// Cut-offs (`Thresholds::default()` = M0-E §1c / Amendment 3.6). All comparisons are `>=`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Thresholds {
    /// Counts as a dedicated GPU (non-UMA): DirectML by default, qualifies Standard.
    pub dgpu_min_vram: u64,
    /// High's fp16 1024-dim model measured 3.0–3.3 GB per process; 6 GiB leaves headroom.
    pub high_min_vram: u64,
    pub std_min_ram: u64,
    /// Physical cores that qualify Standard without a dedicated GPU.
    pub std_min_cores_cpu_only: u32,
    pub std_min_free_disk: u64,
    pub high_min_ram: u64,
    pub high_min_free_disk: u64,
    /// iGPU probe: DirectML must be at least this much faster than the CPU.
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

/// Result of [`select_tier`]; stored in `meta.hw_detection` and emitted as `tier.detected`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TierDecision {
    pub tier: Tier,
    pub ep: EpPlan,
    pub gpu_class: GpuClass,
    /// Plain-language reasons (Details / diagnostics only; never in the Basic UI).
    pub reasons: Vec<String>,
}

fn basic_render(g: &GpuInfo) -> bool {
    g.vendor_id == MS_VENDOR_ID && g.device_id == MS_BASIC_RENDER_DEVICE_ID
}

/// Usable by DirectML at all (not software/WARP, has D3D12 FL 11.0).
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

/// Best adapter: Dedicated beats Integrated, then most dedicated VRAM, then lowest DXGI index.
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

/// What the platform gives the tier rules (Amendment 18).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TierPlatform {
    /// Graphics adapters can run DirectML (Windows only).
    pub directml: bool,
    /// The processor's vector unit always qualifies, whatever [`HwInfo::avx2`] says (aarch64
    /// NEON is part of the base instruction set).
    pub simd_baseline: bool,
}

impl TierPlatform {
    /// Windows on x64: DirectML, and AVX2 must be detected.
    pub const WINDOWS_X64: Self = Self {
        directml: true,
        simd_baseline: false,
    };
    /// macOS on Apple silicon: no DirectML, NEON always present.
    pub const MACOS_ARM64: Self = Self {
        directml: false,
        simd_baseline: true,
    };

    /// The platform this build runs on.
    pub const fn current() -> Self {
        Self {
            directml: cfg!(windows),
            simd_baseline: cfg!(target_arch = "aarch64"),
        }
    }
}

/// Pure tier selection (Amendment 3.6) for the platform this build runs on.
pub fn select_tier(hw: &HwInfo, t: &Thresholds) -> TierDecision {
    select_tier_on(hw, t, TierPlatform::current())
}

/// Pure tier selection (Amendment 3.6) for `platform`. Without DirectML the adapters are never
/// used, so the plan is always [`EpPlan::Cpu`].
pub fn select_tier_on(hw: &HwInfo, t: &Thresholds, platform: TierPlatform) -> TierDecision {
    let mut reasons = Vec::new();
    let best = if platform.directml {
        best_gpu(hw, t)
    } else {
        None
    };
    let vram = best.map_or(0, |(g, _)| g.dedicated_vram);
    let (ep, gpu_class) = match best {
        Some((g, GpuClass::Dedicated)) => {
            reasons.push(format!(
                "graphics card '{}' ({} MiB) will be used",
                g.name,
                g.dedicated_vram / MIB
            ));
            (
                EpPlan::DirectMl {
                    dxgi_index: g.dxgi_index,
                },
                GpuClass::Dedicated,
            )
        }
        Some((g, _)) => {
            reasons.push(format!(
                "integrated graphics '{}': used if faster than the processor",
                g.name
            ));
            (
                EpPlan::ProbeDirectMl {
                    dxgi_index: g.dxgi_index,
                },
                GpuClass::Integrated,
            )
        }
        None if platform.directml => {
            reasons.push("no suitable graphics card; using the processor".into());
            (EpPlan::Cpu, GpuClass::None)
        }
        None => {
            reasons.push("search runs on the processor on this computer".into());
            (EpPlan::Cpu, GpuClass::None)
        }
    };
    let dgpu = gpu_class == GpuClass::Dedicated;

    // Gates shared by every tier above Minimum.
    let gates = |min_ram: u64, min_disk: u64| -> Vec<String> {
        let mut r = Vec::new();
        if !hw.avx2 && !platform.simd_baseline {
            r.push("processor lacks AVX2".into());
        }
        if hw.total_ram < min_ram {
            r.push(format!(
                "memory {} GiB < {} GiB",
                hw.total_ram / GIB,
                min_ram / GIB
            ));
        }
        if hw.free_disk < min_disk {
            r.push(format!(
                "free disk {} GiB < {} GiB",
                hw.free_disk / GIB,
                min_disk / GIB
            ));
        }
        r
    };

    // High: dedicated GPU only (1024-dim on the CPU is ~2.6 chunks/s: too slow for first indexing).
    let mut high = gates(t.high_min_ram, t.high_min_free_disk);
    if !dgpu || vram < t.high_min_vram {
        high.push(format!(
            "needs a graphics card with >= {} GiB of its own memory",
            t.high_min_vram / GIB
        ));
    }
    if high.is_empty() {
        reasons.push("High qualifies".into());
        return TierDecision {
            tier: Tier::High,
            ep,
            gpu_class,
            reasons,
        };
    }
    reasons.extend(high.into_iter().map(|s| format!("not High: {s}")));

    // Standard: dedicated GPU, or enough physical cores. An iGPU never upgrades the tier.
    let mut std = gates(t.std_min_ram, t.std_min_free_disk);
    if !dgpu && hw.physical_cores < t.std_min_cores_cpu_only {
        std.push(format!(
            "{} processor cores < {} (no graphics card)",
            hw.physical_cores, t.std_min_cores_cpu_only
        ));
    }
    if std.is_empty() {
        reasons.push("Standard qualifies".into());
        return TierDecision {
            tier: Tier::Standard,
            ep,
            gpu_class,
            reasons,
        };
    }
    reasons.extend(std.into_iter().map(|s| format!("not Standard: {s}")));
    TierDecision {
        tier: Tier::Minimum,
        ep,
        gpu_class,
        reasons,
    }
}

/// Plain warning shown next to a manual choice above the recommendation (never blocks).
pub const OVERRIDE_WARNING: &str =
    "This computer may index slowly with this choice. Search keeps working while it runs.";

/// Advanced → manual override. Any tier may be forced; one above the recommendation carries
/// [`OVERRIDE_WARNING`].
pub fn effective_tier(rec: &TierDecision, manual: Option<Tier>) -> (Tier, Option<&'static str>) {
    match manual {
        None => (rec.tier, None),
        Some(m) if m <= rec.tier => (m, None),
        Some(m) => (m, Some(OVERRIDE_WARNING)),
    }
}

// ----------------------------------------------------------------------------------- GPU policy
//
// Amendment 2 / §2.8, all pure (the engine persists [`GpuState`] in catalog `meta` and calls
// [`effective_ep`] before every new embedding session):
// * 3 runtime faults (device removed, TDR, DirectML run error) within 24 h on one adapter +
//   driver → CPU until that adapter's driver version changes ([`GpuFaultLedger`]).
// * The iGPU / small-dGPU probe runs once per adapter + driver ([`ProbeCache`]).
// * "Use graphics card: Off" → CPU for new sessions (no re-embed: the variant is sticky).

/// One adapter at one driver version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct GpuKey {
    pub vendor_id: u32,
    pub device_id: u32,
    pub driver_version: u64,
}

impl GpuKey {
    pub const fn of(g: &GpuInfo) -> Self {
        Self {
            vendor_id: g.vendor_id,
            device_id: g.device_id,
            driver_version: g.driver_version,
        }
    }

    /// Same physical adapter model (driver version ignored).
    pub const fn same_adapter(&self, o: &GpuKey) -> bool {
        self.vendor_id == o.vendor_id && self.device_id == o.device_id
    }
}

/// Faults counted per window (Amendment 2: "three faults in 24 h").
pub const GPU_FAULT_LIMIT: usize = 3;
pub const GPU_FAULT_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GpuFault {
    pub key: GpuKey,
    pub at_ms: i64,
}

/// GPU runtime faults. Once [`GPU_FAULT_LIMIT`] faults of one adapter + driver fall inside
/// [`GPU_FAULT_WINDOW_MS`], that adapter stays on the CPU **until its driver version changes**
/// (not merely until the window passes).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GpuFaultLedger {
    #[serde(default)]
    pub faults: Vec<GpuFault>,
    /// Adapter + driver that reached the limit.
    #[serde(default)]
    pub blocked: Option<GpuKey>,
}

impl GpuFaultLedger {
    /// Record one fault; returns true if this fault put the adapter over the limit.
    pub fn record(&mut self, key: GpuKey, now_ms: i64) -> bool {
        self.faults
            .retain(|f| now_ms.saturating_sub(f.at_ms) < GPU_FAULT_WINDOW_MS);
        self.faults.push(GpuFault { key, at_ms: now_ms });
        let n = self.faults.iter().filter(|f| f.key == key).count();
        if n >= GPU_FAULT_LIMIT && self.blocked != Some(key) {
            self.blocked = Some(key);
            return true;
        }
        false
    }

    /// Faults of `key` in the window ending at `now_ms`.
    pub fn recent(&self, key: GpuKey, now_ms: i64) -> usize {
        self.faults
            .iter()
            .filter(|f| f.key == key && now_ms.saturating_sub(f.at_ms) < GPU_FAULT_WINDOW_MS)
            .count()
    }

    /// Whether `key` must stay on the CPU. A driver update of the blocked adapter lifts the block
    /// (and [`Self::forget_stale_driver`] then drops it).
    pub fn blocks(&self, key: GpuKey) -> bool {
        self.blocked == Some(key)
    }

    /// Drop the block and faults recorded for `key`'s adapter under a different driver version.
    /// Returns true if anything changed.
    pub fn forget_stale_driver(&mut self, key: GpuKey) -> bool {
        let before = (self.faults.len(), self.blocked);
        self.faults
            .retain(|f| !(f.key.same_adapter(&key) && f.key.driver_version != key.driver_version));
        if let Some(b) = self.blocked
            && b.same_adapter(&key)
            && b.driver_version != key.driver_version
        {
            self.blocked = None;
        }
        before != (self.faults.len(), self.blocked)
    }
}

/// Outcome of the one-off iGPU / small-dGPU probe (DirectML ≥ `probe_min_speedup`× the CPU and
/// canary cosine ≥ 0.999).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProbeResult {
    pub key: GpuKey,
    pub use_gpu: bool,
    /// DirectML / CPU speed ratio, when the timing ran.
    pub speedup: Option<f32>,
    pub at_ms: i64,
}

/// Probe results per adapter + driver version (one entry per adapter; a driver update replaces it).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProbeCache {
    #[serde(default)]
    pub entries: Vec<ProbeResult>,
}

impl ProbeCache {
    pub fn get(&self, key: GpuKey) -> Option<&ProbeResult> {
        self.entries.iter().find(|e| e.key == key)
    }

    pub fn put(&mut self, r: ProbeResult) {
        self.entries.retain(|e| !e.key.same_adapter(&r.key));
        self.entries.push(r);
    }
}

/// Everything the GPU policy remembers across restarts (catalog `meta`, key [`GpuState::META_KEY`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GpuState {
    #[serde(default)]
    pub ledger: GpuFaultLedger,
    #[serde(default)]
    pub probes: ProbeCache,
}

impl GpuState {
    pub const META_KEY: &'static str = "gpu_state";
}

/// Why [`effective_ep`] chose its plan (diagnostics).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EpPlanReason {
    /// `select_tier`'s plan, unchanged (a dedicated GPU, or no usable GPU at all).
    Detected,
    /// Advanced → "Use graphics card: Off".
    DisabledByUser,
    /// 3 faults in 24 h on this adapter + driver.
    FaultLimit,
    /// The probe ran before on this adapter + driver and DirectML won: no re-probe.
    ProbeCachedFast,
    /// The probe ran before on this adapter + driver and the CPU won.
    ProbeCachedSlow,
    /// iGPU / small dGPU with no cached result: probe now.
    ProbePending,
    /// The plan named an adapter that is no longer present.
    AdapterGone,
}

/// The plan for a **new** embedding session (pure). Never upgrades the tier; never an error.
pub fn effective_ep(
    decision: &TierDecision,
    gpus: &[GpuInfo],
    use_gpu: bool,
    state: &GpuState,
) -> (EpPlan, EpPlanReason) {
    let idx = match decision.ep {
        EpPlan::Cpu => return (EpPlan::Cpu, EpPlanReason::Detected),
        EpPlan::DirectMl { dxgi_index } | EpPlan::ProbeDirectMl { dxgi_index } => dxgi_index,
    };
    if !use_gpu {
        return (EpPlan::Cpu, EpPlanReason::DisabledByUser);
    }
    let Some(g) = gpus.iter().find(|g| g.dxgi_index == idx) else {
        return (EpPlan::Cpu, EpPlanReason::AdapterGone);
    };
    let key = GpuKey::of(g);
    if state.ledger.blocks(key) {
        return (EpPlan::Cpu, EpPlanReason::FaultLimit);
    }
    match decision.ep {
        EpPlan::ProbeDirectMl { dxgi_index } => match state.probes.get(key) {
            Some(r) if r.use_gpu => (
                EpPlan::DirectMl { dxgi_index },
                EpPlanReason::ProbeCachedFast,
            ),
            Some(_) => (EpPlan::Cpu, EpPlanReason::ProbeCachedSlow),
            None => (decision.ep, EpPlanReason::ProbePending),
        },
        other => (other, EpPlanReason::Detected),
    }
}

/// GPU batches should take about this long (desktop responsiveness, TDR margin).
pub const GPU_BATCH_TARGET_MS: u64 = 200;
/// A GPU batch slower than this halves the batch size.
pub const GPU_BATCH_HALVE_MS: u64 = 500;

/// Runtime GPU batch size (§2.8: "halve while a batch takes > 500 ms"). The first batch at each
/// size is not judged (DirectML compiles kernels per new shape), and the batch never drops below
/// `floor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuBatchGovernor {
    batch: u32,
    floor: u32,
    judged_at: Option<u32>,
}

impl GpuBatchGovernor {
    pub fn new(start: u32, floor: u32) -> Self {
        let floor = floor.max(1);
        Self {
            batch: start.max(floor),
            floor,
            judged_at: None,
        }
    }

    pub const fn batch(&self) -> u32 {
        self.batch
    }

    /// Report one finished batch of `items` texts (≤ the current batch) that took `ms`.
    /// Returns the new batch size if it was halved.
    pub fn observe(&mut self, items: u32, ms: u64) -> Option<u32> {
        if items < self.batch {
            return None; // a short tail batch says little about the full size
        }
        if self.judged_at != Some(self.batch) {
            self.judged_at = Some(self.batch); // warm-up run for this shape
            return None;
        }
        if ms > GPU_BATCH_HALVE_MS && self.batch > self.floor {
            self.batch = (self.batch / 2).max(self.floor);
            return Some(self.batch);
        }
        None
    }
}

// ------------------------------------------------------------------------- upgrade + downloads

/// Size shown to the user: bytes in MiB, rounded (218 155 826 B → "208 MB", as in the plan).
pub const fn display_mb(bytes: u64) -> u64 {
    (bytes + MIB / 2) / MIB
}

/// The Basic settings line (§2.5): "Recommended for your computer: Better search (208 MB
/// download)" + [Use it].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Recommendation {
    pub tier: Tier,
    /// "Better" / "Best".
    pub label: &'static str,
    /// The file a new index would be built with (sticky variant).
    pub file: ModelFile,
    /// ONNX bytes (the tokenizer side files add < 1 MB).
    pub download_bytes: u64,
    pub text: String,
}

/// `Some` only when the recommendation is above the active tier: nothing is ever suggested
/// downwards, and nothing downloads without [Use it]. `gpu_resolved` = a new index would be built
/// on DirectML (plan after [`effective_ep`], not a CPU plan).
pub fn recommendation(
    recommended: Tier,
    active: Tier,
    gpu_resolved: bool,
) -> Option<Recommendation> {
    if recommended <= active {
        return None;
    }
    let p = TierProfile::for_tier(recommended);
    let file = p.file_for_new_index(gpu_resolved);
    let text = if p.bundled {
        format!(
            "Recommended for your computer: {} search",
            recommended.label()
        )
    } else {
        format!(
            "Recommended for your computer: {} search ({} MB download)",
            recommended.label(),
            display_mb(file.bytes)
        )
    };
    Some(Recommendation {
        tier: recommended,
        label: recommended.label(),
        file,
        download_bytes: if p.bundled { 0 } else { file.bytes },
        text,
    })
}

/// Free space a tier switch needs on the data volume (§2.8 disk pre-flight):
/// `missing model bytes + rows × dims × 4 × 1.3 + 2 GiB` (old and new index coexist until the swap).
pub fn upgrade_disk_needed(missing_model_bytes: u64, rows: u64, dims: u32) -> u64 {
    let vectors = rows.saturating_mul(u64::from(dims)).saturating_mul(4);
    missing_model_bytes
        .saturating_add(vectors.saturating_add(vectors / 10 * 3 + vectors % 10 * 3 / 10))
        .saturating_add(2 * GIB)
}

// ------------------------------------------------------------------------------------- profiles

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Precision {
    Fp32,
    Fp16,
    Int8,
}

impl Precision {
    /// Catalog value (`embedding_profiles.precision`; `remote` is used for remote profiles).
    pub const fn as_str(self) -> &'static str {
        match self {
            Precision::Fp32 => "fp32",
            Precision::Fp16 => "fp16",
            Precision::Int8 => "int8",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Pooling {
    /// CLS token, then L2-normalise (every tier model).
    Cls,
    Mean,
}

impl Pooling {
    pub const fn as_str(self) -> &'static str {
        match self {
            Pooling::Cls => "cls",
            Pooling::Mean => "mean",
        }
    }
}

/// One model file, pinned in the binary. `sha256` is checked at load (bundled) or after download.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ModelFile {
    pub hf_repo: &'static str,
    pub hf_revision: &'static str,
    pub path: &'static str,
    pub bytes: u64,
    pub precision: Precision,
    /// HF LFS oid (= sha256 of the file), lowercase hex.
    pub sha256: &'static str,
}

/// EP an index's file may run on. fp32/fp16 are EP-portable (CPU vs DirectML cos ≥ 0.99998);
/// int8 is CPU-pinned (DirectML int8 vs CPU int8 cos 0.991–0.997 < 0.999).
pub const fn ep_for_file(f: &ModelFile, gpu_available: bool) -> EpActive {
    match (f.precision, gpu_available) {
        (Precision::Int8, _) | (_, false) => EpActive::Cpu,
        _ => EpActive::DirectMl,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AnnIndexType {
    /// Exact scan (small tables or very selective filters).
    Flat,
    /// Default (Amendment 3.8: recall@10 0.98 vs flat).
    #[default]
    IvfHnswSq,
    /// Memory-saving option only (recall@10 0.82).
    IvfPq,
}

/// ANN parameters: **the single source of truth** for every ANN default in the workspace
/// (CO-1). `sr_core::index::lance::AnnConfig::default()` is `AnnConfig::from(&ANN_DEFAULT)`;
/// nothing else hard-codes these numbers.
///
/// Every Minimum value was measured in M1-E (1M × 384 dims, CPU; `docs/research/m1-e-core.md`
/// §5 and §8, Amendment 4.7): IVF_HNSW_SQ with 16 384 rows per partition (61 partitions at 1M),
/// m 20, ef_construction 150; query nprobes 16, ef 100, refine 5 → dense recall@10 0.999, hybrid
/// p95 41.8 ms. Partial scopes: exact scan below 5 % of rows; otherwise a prefiltered ANN with
/// `nprobes`/`ef` widened by `clamp(1/selectivity, 4, 20)` (`ef` capped at 2 000) → recall@10
/// ≥ 0.997 at 20 % and 60 % scopes. Standard/High keep proportionally larger knobs until the eval
/// harness re-measures them at 768/1024 dims.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct AnnParams {
    /// Index kind. `Flat` = never build an ANN index; M1's store implements IVF_HNSW_SQ only
    /// (IVF_PQ is a later memory-saving option).
    pub index_type: AnnIndexType,
    /// Below this row count: flat scan, no index (C5: 50k).
    pub min_rows: u64,
    /// IVF partitions = clamp(rows / `rows_per_partition`, 1, 4096).
    pub rows_per_partition: u32,
    pub hnsw_m: u32,
    pub hnsw_ef_construction: u32,
    /// Query: IVF partitions probed.
    pub nprobes: u32,
    /// Query: HNSW search breadth. lance requires `ef ≥ k × refine_factor`; the store raises it
    /// to that bound per query, so this is a floor.
    pub ef: u32,
    /// Query: re-rank `k × refine_factor` candidates with full-precision vectors.
    pub refine_factor: u32,
    /// Exact filtered scan when the scope keeps fewer than this fraction of rows (C5).
    pub flat_scan_selectivity: f64,
    /// Partial scopes: `nprobes`/`ef` are multiplied by `clamp(1/selectivity, min, max)`.
    pub partial_widen_min: f64,
    pub partial_widen_max: f64,
    /// Upper bound for the widened `ef`.
    pub max_ef: u32,
    /// Retrain the index when rows ≥ trained rows × this factor.
    pub retrain_growth: f64,
    /// Incrementally index new rows once unindexed rows exceed this fraction of indexed rows.
    pub reindex_unindexed_fraction: f64,
}

impl Default for AnnParams {
    fn default() -> Self {
        ANN_DEFAULT
    }
}

impl AnnParams {
    /// IVF partitions for `rows` rows.
    pub fn num_partitions(&self, rows: u64) -> u32 {
        let p = (rows / u64::from(self.rows_per_partition.max(1))).clamp(1, 4096);
        u32::try_from(p).unwrap_or(4096)
    }

    /// Whether an ANN index should exist at `rows` rows.
    pub const fn wants_index(&self, rows: u64) -> bool {
        rows >= self.min_rows
    }
}

/// Everything that scales with the tier. Chunking is deliberately **not** here (3.5).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct TierProfile {
    pub tier: Tier,
    /// Canonical model id (HF repo of the original model).
    pub model_id: &'static str,
    pub dims: u32,
    pub pooling: Pooling,
    pub query_prefix: &'static str,
    /// Documents get no prefix for every tier model.
    pub doc_prefix: &'static str,
    pub max_seq_tokens: u32,
    /// Variant used when an index is built on DirectML.
    pub gpu_file: ModelFile,
    /// Variant used when an index is built on the CPU.
    pub cpu_file: ModelFile,
    /// Shipped inside the installer (only Minimum).
    pub bundled: bool,
    pub batch_gpu: u32,
    pub batch_cpu: u32,
    /// Batches in flight. GPU 2 (tokenise N+1 while N runs); CPU 1 session with
    /// intra-op threads = max(1, physical cores − 2).
    pub concurrency_gpu: u32,
    pub concurrency_cpu: u32,
    pub ann: AnnParams,
    pub rerank: bool,
    pub rerank_top_n: u32,
}

impl TierProfile {
    pub const fn for_tier(tier: Tier) -> &'static TierProfile {
        match tier {
            Tier::Minimum => &MINIMUM,
            Tier::Standard => &STANDARD,
            Tier::High => &HIGH,
        }
    }

    /// File for a **new** index build. Afterwards the index is sticky to this file (§2.8).
    pub const fn file_for_new_index(&self, gpu_resolved: bool) -> ModelFile {
        if gpu_resolved {
            self.gpu_file
        } else {
            self.cpu_file
        }
    }

    pub const fn batch(&self, ep: EpActive) -> u32 {
        match ep {
            EpActive::Cpu => self.batch_cpu,
            EpActive::DirectMl => self.batch_gpu,
        }
    }

    pub const fn concurrency(&self, ep: EpActive) -> u32 {
        match ep {
            EpActive::Cpu => self.concurrency_cpu,
            EpActive::DirectMl => self.concurrency_gpu,
        }
    }

    /// Starting GPU batch capped by the adapter's dedicated VRAM (3.6). Measured per-process
    /// footprints were 2.6–3.3 GB at b16–b64 on a 32 GiB card; below `dgpu_min_vram` (iGPU,
    /// small dGPU) the batch starts at a quarter, below `high_min_vram` at half. The runtime
    /// still halves while a batch takes > 500 ms. Initial heuristic, re-measured in M2 (U1/U3).
    pub fn gpu_batch_for_vram(&self, dedicated_vram: u64, t: &Thresholds) -> u32 {
        let b = if dedicated_vram >= t.high_min_vram {
            self.batch_gpu
        } else if dedicated_vram >= t.dgpu_min_vram {
            self.batch_gpu / 2
        } else {
            self.batch_gpu / 4
        };
        b.max(self.batch_cpu).max(1)
    }

    /// Intra-op CPU threads for one session.
    pub const fn cpu_threads(physical_cores: u32) -> u32 {
        let t = physical_cores.saturating_sub(2);
        if t == 0 { 1 } else { t }
    }
}

const BGE_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

/// The measured M1 ANN defaults (see [`AnnParams`]); also the Minimum tier's values.
pub const ANN_DEFAULT: AnnParams = AnnParams {
    index_type: AnnIndexType::IvfHnswSq,
    min_rows: 50_000,
    rows_per_partition: 16_384,
    hnsw_m: 20,
    hnsw_ef_construction: 150,
    nprobes: 16,
    ef: 100,
    refine_factor: 5,
    flat_scan_selectivity: 0.05,
    partial_widen_min: 4.0,
    partial_widen_max: 20.0,
    max_ef: 2_000,
    retrain_growth: 2.0,
    reindex_unindexed_fraction: 0.05,
};

/// Model files pinned in the binary (HF LFS sha256, verified 2026-10-17).
pub mod files {
    use super::{ModelFile, Precision};

    const BGE_Q_REV: &str = "aa8f8b060edb00e03bfdd08813a2949946c8ba55";
    const BGE_X_REV: &str = "ea104dacec62c0de699686887e3f920caeb4f3e3";
    const ARCTIC_M_REV: &str = "e58a8f756156a1293d763f17e3aae643474e9b8a";
    const ARCTIC_L_REV: &str = "d8fb21ca8d905d2832ee8b96c894d3298964346b";

    /// Bundled Minimum file (3.4): fp32, fastembed's built-in; parity min cos 0.999998 vs fp32.
    pub const BGE_SMALL_FP32_OPT: ModelFile = ModelFile {
        hf_repo: "Qdrant/bge-small-en-v1.5-onnx-Q",
        hf_revision: BGE_Q_REV,
        path: "model_optimized.onnx",
        bytes: 66_465_124,
        precision: Precision::Fp32,
        sha256: "51f1bd0addd6e859e42c2c8021a5e5461385bb676a649f4b269aa445449f2431",
    };
    pub const ARCTIC_M15_FP16: ModelFile = ModelFile {
        hf_repo: "Snowflake/snowflake-arctic-embed-m-v1.5",
        hf_revision: ARCTIC_M_REV,
        path: "onnx/model_fp16.onnx",
        bytes: 218_155_826,
        precision: Precision::Fp16,
        sha256: "8384cc8a480d60ade695631ced7b015b3f21d959a8ad3cdffd189a4005408c98",
    };
    pub const ARCTIC_M15_FP32: ModelFile = ModelFile {
        hf_repo: "Snowflake/snowflake-arctic-embed-m-v1.5",
        hf_revision: ARCTIC_M_REV,
        path: "onnx/model.onnx",
        bytes: 435_939_137,
        precision: Precision::Fp32,
        sha256: "bc6f7662487a0796e6f86229c8e5c3ab2f7059e08e0bb2f524125273f72171c4",
    };
    pub const ARCTIC_L_FP16: ModelFile = ModelFile {
        hf_repo: "Snowflake/snowflake-arctic-embed-l",
        hf_revision: ARCTIC_L_REV,
        path: "onnx/model_fp16.onnx",
        bytes: 668_828_773,
        precision: Precision::Fp16,
        sha256: "8ca24235754e5553234266fba529a7eec363e8d565d3f21761ada22841f5f3c8",
    };
    pub const ARCTIC_L_FP32: ModelFile = ModelFile {
        hf_repo: "Snowflake/snowflake-arctic-embed-l",
        hf_revision: ARCTIC_L_REV,
        path: "onnx/model.onnx",
        bytes: 1_336_854_283,
        precision: Precision::Fp32,
        sha256: "9c6ae4c0108aa2f7fb4cf95776f40bfd7dca7edb6d8c686d6470b6ab105ca559",
    };

    /// Eval-harness candidates only (3.4: int8 is off until it shows ≤ 1.0 nDCG@10 loss).
    pub const BGE_SMALL_INT8: ModelFile = ModelFile {
        hf_repo: "Xenova/bge-small-en-v1.5",
        hf_revision: BGE_X_REV,
        path: "onnx/model_int8.onnx",
        bytes: 33_760_831,
        precision: Precision::Int8,
        sha256: "bf64d05457cb391fa88d045faf5927a15ea36d96228ddf23ea970087afdc1197",
    };
    pub const ARCTIC_M15_INT8: ModelFile = ModelFile {
        hf_repo: "Snowflake/snowflake-arctic-embed-m-v1.5",
        hf_revision: ARCTIC_M_REV,
        path: "onnx/model_int8.onnx",
        bytes: 110_145_162,
        precision: Precision::Int8,
        sha256: "a18f437b2466863901a0bdc14904cf93246f5ecce0b656fc773bc2b7b2f84f6e",
    };
    pub const ARCTIC_L_INT8: ModelFile = ModelFile {
        hf_repo: "Snowflake/snowflake-arctic-embed-l",
        hf_revision: ARCTIC_L_REV,
        path: "onnx/model_int8.onnx",
        bytes: 336_984_086,
        precision: Precision::Int8,
        sha256: "ab802115224b05d283e2042cf889b5490c476e19d28ce93e2073a57476b95541",
    };
}

/// Minimum ("Basic", bundled): bge-small-en-v1.5, 384 dims, one fp32 file for CPU and GPU (3.4).
/// Measured: 642 chunks/s on DirectML (b64), ~10 chunks/s CPU fp32 (b8) at 512 tokens.
pub const MINIMUM: TierProfile = TierProfile {
    tier: Tier::Minimum,
    model_id: "BAAI/bge-small-en-v1.5",
    dims: 384,
    pooling: Pooling::Cls,
    query_prefix: BGE_PREFIX,
    doc_prefix: "",
    max_seq_tokens: 512,
    gpu_file: files::BGE_SMALL_FP32_OPT,
    cpu_file: files::BGE_SMALL_FP32_OPT,
    bundled: true,
    batch_gpu: 64,
    batch_cpu: 8,
    concurrency_gpu: 2,
    concurrency_cpu: 1,
    ann: ANN_DEFAULT,
    rerank: false,
    rerank_top_n: 0,
};

/// Standard ("Better"): snowflake-arctic-embed-m-v1.5, 768 dims. fp16 on GPU (496 chunks/s b64),
/// fp32 on CPU until the int8 gate clears (3.4).
pub const STANDARD: TierProfile = TierProfile {
    tier: Tier::Standard,
    model_id: "Snowflake/snowflake-arctic-embed-m-v1.5",
    dims: 768,
    pooling: Pooling::Cls,
    query_prefix: BGE_PREFIX,
    doc_prefix: "",
    max_seq_tokens: 512,
    gpu_file: files::ARCTIC_M15_FP16,
    cpu_file: files::ARCTIC_M15_FP32,
    bundled: false,
    batch_gpu: 64,
    batch_cpu: 8,
    concurrency_gpu: 2,
    concurrency_cpu: 1,
    ann: AnnParams {
        nprobes: 24,
        ..ANN_DEFAULT
    },
    rerank: false,
    rerank_top_n: 0,
};

/// High ("Best"): snowflake-arctic-embed-l, 1024 dims, dGPU ≥ 6 GiB. fp16 on GPU (176 chunks/s
/// b32); the fp32 CPU file is reachable only through the Advanced override.
pub const HIGH: TierProfile = TierProfile {
    tier: Tier::High,
    model_id: "Snowflake/snowflake-arctic-embed-l",
    dims: 1024,
    pooling: Pooling::Cls,
    query_prefix: BGE_PREFIX,
    doc_prefix: "",
    max_seq_tokens: 512,
    gpu_file: files::ARCTIC_L_FP16,
    cpu_file: files::ARCTIC_L_FP32,
    bundled: false,
    batch_gpu: 32,
    batch_cpu: 8,
    concurrency_gpu: 2,
    concurrency_cpu: 1,
    ann: AnnParams {
        hnsw_m: 24,
        hnsw_ef_construction: 200,
        nprobes: 32,
        refine_factor: 10,
        ..ANN_DEFAULT
    },
    rerank: true,
    rerank_top_n: 50,
};

#[cfg(test)]
mod tests;
