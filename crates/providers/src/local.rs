//! Local embedding provider: fastembed user-defined model (CLS pooling, bge query prefix) on the
//! statically linked ONNX Runtime, CPU always, DirectML when the [`EpPlan`] asks for it.
//!
//! Session ladder (Amendment 2, 3.7; §2.8). Never relies on ORT's implicit fallback:
//! 1. Verify the model bytes against their sha256 pins ([`crate::models::load_verified`]).
//! 2. Build a **CPU** session and run the 2-chunk canary on it (finite, `dims` check). A CPU
//!    failure is the only hard error: `ModelIncompatible` for a downloaded tier model (M2 keeps the
//!    current index), `Internal` for the bundled one.
//! 3. If the plan is DirectML and the GPU is allowed: load the verified `DirectML.dll`
//!    ([`crate::dml`], signature + version ≥ 1.15), build a DirectML session with
//!    `error_on_failure`, run the canary, require finite output and cosine ≥ [`CANARY_MIN_COS`]
//!    vs the CPU canary per vector. For `ProbeDirectMl` (iGPU / small dGPU), also require DirectML
//!    ≥ `probe_min_speedup`× the CPU on a timed batch. Any failure: keep the CPU session, emit
//!    `EpChanged` with the reason. Silent.
//! 4. During a run, a DirectML error (device removed, TDR, driver fault) rebuilds a fresh CPU
//!    session from re-verified files, re-runs the batch and emits `EpChanged{reason:
//!    RuntimeFault}`. The caller never sees the GPU error. (The engine counts these faults: 3 in
//!    24 h keep the adapter on the CPU until its driver changes, `sr_common::tier::GpuFaultLedger`.)
//!
//! When DirectML is active, documents run on the GPU and **queries on the CPU session** kept from
//! step 2 (Amendment 4.27: one short query is faster on the CPU, and it never waits behind a GPU
//! batch). GPU batches are sized by `TierProfile` (capped by VRAM) and halved while a batch takes
//! longer than 500 ms ([`GpuBatchGovernor`]); GPU work pauses while [`LocalOptions::gpu_pause`]
//! says a fullscreen app is in front.
//!
//! **Outputs (O3).** fastembed's own extraction is f32-only. We call `TextEmbedding::transform`
//! and pool the raw ORT values ourselves, accepting `f32` and `f16` outputs (an fp16 graph exported
//! without `keep_io_types` emits f16 `last_hidden_state`). The pinned Standard/High fp16 files emit
//! f32 (measured, [`LoadReport::output_dtype`]), so both paths stay covered by tests.
//!
//! All model calls are blocking and run on tokio's blocking pool; one session per role behind a
//! mutex (ORT parallelises inside a run: intra-op threads = max(1, physical − 2) on the CPU).

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fastembed::{
    ExecutionProviderDispatch, InitOptionsUserDefined, Pooling as FePooling, SingleBatchOutput,
    TextEmbedding, TokenizerFiles, UserDefinedEmbeddingModel,
};
use futures::future::BoxFuture;
use sr_common::Result;
use sr_common::error::{Error, ErrorCode};
use sr_common::tier::{
    EpActive, EpPlan, GpuBatchGovernor, ModelFile, Pooling, Precision, Thresholds, TierProfile,
};
use tokio_util::sync::CancellationToken;

use crate::dml::{self, DmlModule};
use crate::embed::{
    EmbedKind, EmbeddingProfile, EmbeddingProvider, check_vectors, model_incompatible,
};
use crate::events::{EpReason, EventSink, NoopSink, ProviderEvent, internal};
use crate::models::{ModelPaths, VerifiedModel, load_verified};

/// Canary bar: DirectML vs CPU cosine per vector (Amendment 2).
pub const CANARY_MIN_COS: f32 = 0.999;

/// While paused for a fullscreen app, re-check this often.
pub const GPU_PAUSE_POLL: Duration = Duration::from_secs(2);

/// The 2-chunk canary: one short query-like text and one long multi-sentence chunk (exercises a
/// longer sequence shape than the short one).
pub const CANARY: [&str; 2] = [
    "How do I rebuild the vector index after changing the embedding model?",
    "Selene Retrieval indexes local folders for search. Each file is split into chunks of about \
     four hundred tokens with a small overlap, every chunk is embedded once, and identical text \
     anywhere in a collection reuses the stored vector. Searches combine dense vectors with BM25 \
     keyword scores using reciprocal rank fusion, then hydrate line ranges and snippets from the \
     catalog. fn main() { let index = open(\"C:\\\\data\\\\index\"); index.search(\"query\", 10); }",
];

/// Test-only fault injection (forced-failure tests). `None` in production.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FaultInjection {
    #[default]
    None,
    /// DirectML canary output replaced by NaN after a real DirectML session ran it.
    DmlCanaryNonFinite,
    /// DirectML canary vectors perturbed so the cosine vs CPU falls below the bar.
    DmlCanaryMismatch,
    /// The first DirectML embed run fails (driver fault / device removed simulation).
    DmlRunFailsOnce,
    /// Every DirectML batch takes 550 ms longer (drives the > 500 ms halving rule on a fast GPU).
    DmlSlowBatches,
}

/// "Pause GPU work now" (fullscreen D3D app / presentation in front). Called before every GPU
/// batch; must be cheap (the engine passes `sr_providers::dxgi::foreground_busy`).
pub type GpuPauseHook = Arc<dyn Fn() -> bool + Send + Sync>;

/// Everything [`LocalEmbedder::load`] needs. Build with [`LocalOptions::new`].
#[derive(Clone)]
pub struct LocalOptions {
    pub tier: &'static TierProfile,
    /// The sticky variant file (the index's `variant_sha256`; for a new index
    /// `tier.file_for_new_index(gpu_resolved)`).
    pub file: ModelFile,
    pub paths: ModelPaths,
    /// From `select_tier` (TierDecision.ep), adjusted by `sr_common::tier::effective_ep`.
    pub ep: EpPlan,
    /// Advanced "Use graphics card": `true` = Auto (default), `false` = Off.
    pub use_gpu: bool,
    /// Dedicated VRAM of the plan's adapter (caps the GPU batch, 3.6). `None` = uncapped tier batch.
    pub dedicated_vram: Option<u64>,
    pub physical_cores: u32,
    /// Where `DirectML.dll` may come from (default: exe dir, then System32).
    pub dml_search_dirs: Vec<PathBuf>,
    pub thresholds: Thresholds,
    /// Pause GPU batches while this returns true (`None` = never pause).
    pub gpu_pause: Option<GpuPauseHook>,
    #[doc(hidden)]
    pub fault: FaultInjection,
}

impl std::fmt::Debug for LocalOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalOptions")
            .field("model", &self.tier.model_id)
            .field("file", &self.file.path)
            .field("ep", &self.ep)
            .field("use_gpu", &self.use_gpu)
            .field("dedicated_vram", &self.dedicated_vram)
            .field("physical_cores", &self.physical_cores)
            .field("gpu_pause", &self.gpu_pause.is_some())
            .field("fault", &self.fault)
            .finish_non_exhaustive()
    }
}

impl LocalOptions {
    pub fn new(tier: &'static TierProfile, file: ModelFile, paths: ModelPaths, ep: EpPlan) -> Self {
        Self {
            tier,
            file,
            paths,
            ep,
            use_gpu: true,
            dedicated_vram: None,
            physical_cores: std::thread::available_parallelism()
                .map(|n| (n.get() / 2).max(1) as u32)
                .unwrap_or(1),
            dml_search_dirs: dml::default_search_dirs(),
            thresholds: Thresholds::default(),
            gpu_pause: None,
            fault: FaultInjection::None,
        }
    }

    /// The bundled Minimum model from `<app_dir>\models\bge-small-en-v1.5`.
    pub fn bundled(app_dir: &std::path::Path, ep: EpPlan) -> Self {
        let t = &sr_common::tier::MINIMUM;
        Self::new(t, t.gpu_file, ModelPaths::bundled(app_dir), ep)
    }

    fn cpu_threads(&self) -> usize {
        TierProfile::cpu_threads(self.physical_cores) as usize
    }

    /// Batch for `ep`: tier batch; on DirectML capped by VRAM (`gpu_batch_for_vram`).
    pub fn batch_for(&self, ep: EpActive) -> usize {
        let b = match (ep, self.dedicated_vram) {
            (EpActive::DirectMl, Some(v)) => self.tier.gpu_batch_for_vram(v, &self.thresholds),
            _ => self.tier.batch(ep),
        };
        b.max(1) as usize
    }
}

/// How the provider ended up on its EP (diagnostics).
#[derive(Debug, Clone, PartialEq)]
pub struct LoadReport {
    pub ep: EpActive,
    pub reason: EpReason,
    pub detail: Option<String>,
    pub dml: Option<DmlModule>,
    /// Lowest DirectML-vs-CPU canary cosine (when DirectML was tried far enough).
    pub canary_min_cos: Option<f32>,
    /// DirectML / CPU speed ratio from the probe (ProbeDirectMl only).
    pub probe_speedup: Option<f32>,
    /// Element type of the model's embedding output on the CPU canary (`"f32"` / `"f16"`), O3.
    pub output_dtype: Option<String>,
    pub load_ms: u64,
}

struct Session {
    model: TextEmbedding,
    ep: EpActive,
}

/// The local provider. Cheap to share (`Arc` inside).
pub struct LocalEmbedder {
    inner: Arc<Inner>,
    report: LoadReport,
}

struct Inner {
    profile: EmbeddingProfile,
    opts: Arc<LocalOptions>,
    /// Documents: the GPU session when DirectML is active, otherwise the only (CPU) session.
    doc: Mutex<Session>,
    /// CPU session kept for queries while `doc` is on DirectML (4.27). `None`: queries use `doc`.
    query: Mutex<Option<Session>>,
    max_batch: AtomicUsize,
    governor: Mutex<GpuBatchGovernor>,
    sink: Arc<dyn EventSink>,
    faults_left: AtomicUsize,
}

impl std::fmt::Debug for LocalEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalEmbedder")
            .field("model", &self.inner.profile.model)
            .field("variant", &self.inner.profile.variant_sha256)
            .field("ep", &self.active_ep())
            .finish()
    }
}

fn fe_pooling(p: Pooling) -> FePooling {
    match p {
        Pooling::Cls => FePooling::Cls,
        Pooling::Mean => FePooling::Mean,
    }
}

fn build_session(
    vm: &VerifiedModel,
    eps: Vec<ExecutionProviderDispatch>,
    threads: usize,
) -> std::result::Result<TextEmbedding, String> {
    let tok = TokenizerFiles {
        tokenizer_file: vm.tokenizer_json.clone(),
        config_file: vm.config_json.clone(),
        special_tokens_map_file: vm.special_tokens_map_json.clone(),
        tokenizer_config_file: vm.tokenizer_config_json.clone(),
    };
    let model = UserDefinedEmbeddingModel::new(vm.onnx.clone(), tok)
        .with_pooling(fe_pooling(vm.tier.pooling));
    let opts = InitOptionsUserDefined::new()
        .with_max_length(vm.tier.max_seq_tokens as usize)
        .with_intra_threads(threads.max(1))
        .with_execution_providers(eps);
    TextEmbedding::try_new_from_user_defined(model, opts).map_err(|e| e.to_string())
}

fn cpu_session(vm: &VerifiedModel, threads: usize) -> std::result::Result<TextEmbedding, String> {
    build_session(vm, Vec::new(), threads)
}

#[cfg(windows)]
fn dml_session(vm: &VerifiedModel, device_id: u32) -> std::result::Result<TextEmbedding, String> {
    let ep = ort::ep::DirectML::default()
        .with_device_id(device_id as i32)
        .build()
        .error_on_failure();
    // DML EP requirements (memory pattern off, sequential) are applied by fastembed when it sees
    // a DirectML dispatch. Intra-op threads only matter for the few CPU-assigned nodes.
    build_session(vm, vec![ep], 2)
}

/// DirectML is Windows-only. Never reached: `dml::load_directml` already reports `Unsupported`
/// here, so the plan runs on the CPU. macOS: no CoreML EP either (port phase 2 decision): the
/// CPU is fast on Apple silicon, and CoreML would need its own cosine >= 0.999 proof per model
/// variant, a compiled-model cache and a silent fallback first.
#[cfg(not(windows))]
fn dml_session(_vm: &VerifiedModel, _device_id: u32) -> std::result::Result<TextEmbedding, String> {
    Err("DirectML is only available on Windows".into())
}

// ---------------------------------------------------------------------------------- outputs (O3)

/// Output names tried in order when a model has several outputs (fastembed's precedence).
const OUTPUT_NAMES: [&str; 3] = ["text_embeds", "last_hidden_state", "sentence_embedding"];

/// Raw ORT output → `(shape, f32 data, dtype name)`. f16 is widened exactly (every f16 is an f32).
pub(crate) fn extract_f32(
    v: &ort::value::DynValue,
) -> std::result::Result<(Vec<i64>, Vec<f32>, &'static str), String> {
    use ort::value::TensorElementType as T;
    match v.dtype().tensor_type() {
        Some(T::Float32) => {
            let (shape, data) = v.try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
            Ok((shape.to_vec(), data.to_vec(), "f32"))
        }
        Some(T::Float16) => {
            let (shape, data) = v
                .try_extract_tensor::<half::f16>()
                .map_err(|e| e.to_string())?;
            Ok((
                shape.to_vec(),
                data.iter().map(|x| x.to_f32()).collect(),
                "f16",
            ))
        }
        other => Err(format!("unsupported embedding output type {other:?}")),
    }
}

/// Pool `[batch, seq, dims]` (CLS row or masked mean) or pass `[batch, dims]` through, then
/// L2-normalise. `mask(i, j)` = token `j` of row `i` is real (mean pooling only).
pub(crate) fn pool(
    shape: &[i64],
    data: &[f32],
    pooling: Pooling,
    mask: impl Fn(usize, usize) -> bool,
) -> std::result::Result<Vec<Vec<f32>>, String> {
    let dims: Vec<usize> = shape
        .iter()
        .map(|&d| usize::try_from(d).map_err(|_| format!("bad output shape {shape:?}")))
        .collect::<std::result::Result<_, _>>()?;
    let want: usize = dims.iter().product();
    if want != data.len() {
        return Err(format!(
            "output shape {shape:?} does not match {} values",
            data.len()
        ));
    }
    let rows: Vec<Vec<f32>> = match dims.as_slice() {
        [b, d] => (0..*b).map(|i| data[i * d..(i + 1) * d].to_vec()).collect(),
        [b, s, d] => (0..*b)
            .map(|i| {
                let base = i * s * d;
                match pooling {
                    Pooling::Cls => data[base..base + d].to_vec(),
                    Pooling::Mean => {
                        let mut acc = vec![0f32; *d];
                        let mut n = 0f32;
                        for j in 0..*s {
                            if mask(i, j) {
                                n += 1.0;
                                for (a, x) in
                                    acc.iter_mut().zip(&data[base + j * d..base + (j + 1) * d])
                                {
                                    *a += x;
                                }
                            }
                        }
                        acc.iter_mut().for_each(|a| *a /= n.max(1.0));
                        acc
                    }
                }
            })
            .collect(),
        _ => return Err(format!("expected a 2-D or 3-D output, got {shape:?}")),
    };
    Ok(rows.into_iter().map(|r| normalize(&r)).collect())
}

fn normalize(v: &[f32]) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.iter().map(|x| x / (n + 1e-12)).collect()
}

fn pool_batch(
    b: &SingleBatchOutput,
    pooling: Pooling,
) -> std::result::Result<(Vec<Vec<f32>>, &'static str), String> {
    let value = if b.outputs.len() == 1 {
        b.outputs.first().map(|(_, v)| v)
    } else {
        OUTPUT_NAMES
            .iter()
            .find_map(|n| b.outputs.iter().find(|(k, _)| k == n).map(|(_, v)| v))
    }
    .ok_or_else(|| {
        format!(
            "no embedding output among {:?}",
            b.outputs.iter().map(|(k, _)| k).collect::<Vec<_>>()
        )
    })?;
    let (shape, data, dtype) = extract_f32(value)?;
    let mask = &b.attention_mask_array;
    let rows = pool(&shape, &data, pooling, |i, j| {
        mask.get((i, j)).is_some_and(|m| *m != 0)
    })?;
    Ok((rows, dtype))
}

/// One model call over `texts` as a single batch.
fn run_once(
    model: &mut TextEmbedding,
    texts: &[String],
    pooling: Pooling,
) -> std::result::Result<(Vec<Vec<f32>>, &'static str), String> {
    if texts.is_empty() {
        return Ok((Vec::new(), "f32"));
    }
    let out = model
        .transform(texts, Some(texts.len()))
        .map_err(|e| e.to_string())?;
    let mut rows = Vec::with_capacity(texts.len());
    let mut dtype = "f32";
    for b in out.into_raw() {
        let (r, d) = pool_batch(&b, pooling)?;
        rows.extend(r);
        dtype = d;
    }
    Ok((rows, dtype))
}

/// `texts` in batches of `batch` (CPU path and load-time checks).
fn run(
    model: &mut TextEmbedding,
    texts: &[String],
    batch: usize,
    pooling: Pooling,
) -> std::result::Result<Vec<Vec<f32>>, String> {
    let mut out = Vec::with_capacity(texts.len());
    for part in texts.chunks(batch.max(1)) {
        out.extend(run_once(model, part, pooling)?.0);
    }
    Ok(out)
}

fn cos(a: &[f32], b: &[f32]) -> f32 {
    let (mut d, mut na, mut nb) = (0f32, 0f32, 0f32);
    for (x, y) in a.iter().zip(b) {
        d += x * y;
        na += x * x;
        nb += y * y;
    }
    d / (na.sqrt() * nb.sqrt()).max(1e-12)
}

/// Minimum per-row cosine between two embedding sets (−1 if shapes differ).
pub fn min_cosine(a: &[Vec<f32>], b: &[Vec<f32>]) -> f32 {
    if a.len() != b.len() || a.iter().zip(b).any(|(x, y)| x.len() != y.len()) {
        return -1.0;
    }
    a.iter().zip(b).map(|(x, y)| cos(x, y)).fold(1.0, f32::min)
}

fn canary_texts() -> Vec<String> {
    CANARY.iter().map(|s| (*s).to_owned()).collect()
}

/// ~400-token synthetic chunks for the iGPU probe timing.
fn probe_texts(n: usize) -> Vec<String> {
    let base = CANARY[1];
    (0..n)
        .map(|i| format!("{i} {base} {base} {base}"))
        .collect()
}

// ------------------------------------------------------------------------------------ the ladder

struct Built {
    doc: Session,
    query: Option<Session>,
    report: LoadReport,
}

fn load_blocking(opts: &LocalOptions) -> Result<Built> {
    let t0 = Instant::now();
    let pooling = opts.tier.pooling;
    let fail_cpu = |what: &str, detail: String| -> Error {
        let code = if opts.tier.bundled {
            ErrorCode::Internal
        } else {
            ErrorCode::ModelIncompatible
        };
        let mut e = Error::new(code).with_detail(format!("{what}: {detail}"));
        e.ctx.model = Some(opts.tier.model_id.to_owned());
        e
    };
    dml::harden_dll_search();
    let vm = load_verified(opts.tier, &opts.file, &opts.paths)?;

    // 2. CPU session + canary (always: it is the fallback, the query session and the reference).
    let mut cpu = cpu_session(&vm, opts.cpu_threads()).map_err(|e| fail_cpu("CPU session", e))?;
    let canary = canary_texts();
    let (cpu_canary, dtype) =
        run_once(&mut cpu, &canary, pooling).map_err(|e| fail_cpu("CPU canary", e))?;
    check_vectors(opts.tier.dims, canary.len(), &cpu_canary).map_err(|e| {
        let mut e = model_incompatible(e.ctx.detail.clone().unwrap_or_default());
        e.ctx.model = Some(opts.tier.model_id.to_owned());
        e
    })?;
    let output_dtype = Some(dtype.to_owned());

    let on_cpu = |cpu: TextEmbedding,
                  reason: EpReason,
                  detail: Option<String>,
                  dml: Option<DmlModule>,
                  cos: Option<f32>,
                  sp: Option<f32>| Built {
        doc: Session {
            model: cpu,
            ep: EpActive::Cpu,
        },
        query: None,
        report: LoadReport {
            ep: EpActive::Cpu,
            reason,
            detail,
            dml,
            canary_min_cos: cos,
            probe_speedup: sp,
            output_dtype: output_dtype.clone(),
            load_ms: t0.elapsed().as_millis() as u64,
        },
    };

    // 3. DirectML, if planned and allowed.
    let (device, probe) = match opts.ep {
        EpPlan::Cpu => return Ok(on_cpu(cpu, EpReason::CpuPlan, None, None, None, None)),
        EpPlan::DirectMl { dxgi_index } => (dxgi_index, false),
        EpPlan::ProbeDirectMl { dxgi_index } => (dxgi_index, true),
    };
    if !opts.use_gpu {
        return Ok(on_cpu(
            cpu,
            EpReason::DisabledByUser,
            None,
            None,
            None,
            None,
        ));
    }
    if opts.file.precision == Precision::Int8 {
        return Ok(on_cpu(
            cpu,
            EpReason::CpuPinnedVariant,
            None,
            None,
            None,
            None,
        ));
    }
    let module = match dml::load_directml(&opts.dml_search_dirs) {
        Ok(m) => m,
        Err(e) => {
            let reason = match e {
                dml::DmlError::Unsupported => EpReason::Unsupported,
                dml::DmlError::Missing => EpReason::DllMissing,
                // Too old (O2) is reported as untrusted: same outcome (CPU), detail says why.
                dml::DmlError::Untrusted { .. } | dml::DmlError::TooOld { .. } => {
                    EpReason::DllUntrusted
                }
                dml::DmlError::LoadFailed { .. } => EpReason::DllLoadFailed,
            };
            return Ok(on_cpu(cpu, reason, Some(e.to_string()), None, None, None));
        }
    };
    let mut gpu = match dml_session(&vm, device) {
        Ok(s) => s,
        Err(e) => {
            return Ok(on_cpu(
                cpu,
                EpReason::RegistrationFailed,
                Some(e),
                Some(module),
                None,
                None,
            ));
        }
    };
    let mut gpu_canary = match run_once(&mut gpu, &canary, pooling) {
        Ok((v, _)) => v,
        Err(e) => {
            return Ok(on_cpu(
                cpu,
                EpReason::RegistrationFailed,
                Some(format!("canary run: {e}")),
                Some(module),
                None,
                None,
            ));
        }
    };
    match opts.fault {
        FaultInjection::DmlCanaryNonFinite => gpu_canary[0][0] = f32::NAN,
        FaultInjection::DmlCanaryMismatch => {
            for v in &mut gpu_canary {
                v.reverse();
            }
        }
        _ => {}
    }
    if gpu_canary.iter().flatten().any(|x| !x.is_finite())
        || gpu_canary
            .iter()
            .any(|v| v.len() != opts.tier.dims as usize)
    {
        return Ok(on_cpu(
            cpu,
            EpReason::CanaryNonFinite,
            None,
            Some(module),
            None,
            None,
        ));
    }
    let min_cos = min_cosine(&gpu_canary, &cpu_canary);
    if min_cos < CANARY_MIN_COS {
        return Ok(on_cpu(
            cpu,
            EpReason::CanaryMismatch,
            Some(format!("min cos {min_cos:.6}")),
            Some(module),
            Some(min_cos),
            None,
        ));
    }
    let mut speedup = None;
    if probe {
        let texts = probe_texts(8);
        let time = |m: &mut TextEmbedding| -> std::result::Result<f64, String> {
            run(m, &texts, texts.len(), pooling)?; // warm-up (DirectML compiles per shape)
            let t = Instant::now();
            run(m, &texts, texts.len(), pooling)?;
            Ok(t.elapsed().as_secs_f64())
        };
        let (g, c) = match (time(&mut gpu), time(&mut cpu)) {
            (Ok(g), Ok(c)) => (g, c),
            (Err(e), _) | (_, Err(e)) => {
                return Ok(on_cpu(
                    cpu,
                    EpReason::RegistrationFailed,
                    Some(format!("probe: {e}")),
                    Some(module),
                    Some(min_cos),
                    None,
                ));
            }
        };
        let s = (c / g.max(1e-9)) as f32;
        speedup = Some(s);
        if s < opts.thresholds.probe_min_speedup {
            return Ok(on_cpu(
                cpu,
                EpReason::ProbeTooSlow,
                Some(format!("speedup {s:.2}")),
                Some(module),
                Some(min_cos),
                speedup,
            ));
        }
    }
    // DirectML passed: documents on the GPU, queries stay on the verified CPU session (4.27).
    Ok(Built {
        doc: Session {
            model: gpu,
            ep: EpActive::DirectMl,
        },
        query: Some(Session {
            model: cpu,
            ep: EpActive::Cpu,
        }),
        report: LoadReport {
            ep: EpActive::DirectMl,
            reason: EpReason::Planned,
            detail: None,
            dml: Some(module),
            canary_min_cos: Some(min_cos),
            probe_speedup: speedup,
            output_dtype,
            load_ms: t0.elapsed().as_millis() as u64,
        },
    })
}

impl LocalEmbedder {
    /// Verify the model, build sessions per the ladder above and emit the first `EpChanged`.
    /// Runs on the blocking pool.
    pub async fn load(opts: LocalOptions, sink: Arc<dyn EventSink>) -> Result<Self> {
        let opts = Arc::new(opts);
        let o = opts.clone();
        let built = tokio::task::spawn_blocking(move || load_blocking(&o))
            .await
            .map_err(|e| internal(format!("embedder load task: {e}")))??;
        Ok(Self::from_built(opts, built, sink))
    }

    /// Blocking variant (startup code that is not on a runtime yet).
    pub fn load_blocking(opts: LocalOptions, sink: Arc<dyn EventSink>) -> Result<Self> {
        let opts = Arc::new(opts);
        let built = load_blocking(&opts)?;
        Ok(Self::from_built(opts, built, sink))
    }

    fn from_built(opts: Arc<LocalOptions>, built: Built, sink: Arc<dyn EventSink>) -> Self {
        let profile = EmbeddingProfile::local(opts.tier, &opts.file);
        let r = &built.report;
        tracing::info!(model = opts.tier.model_id, ep = ?r.ep, reason = ?r.reason, detail = ?r.detail, output = ?r.output_dtype, load_ms = r.load_ms, "local embedder ready");
        sink.emit(ProviderEvent::EpChanged {
            model_id: profile.model.clone(),
            from: None,
            to: r.ep,
            reason_code: r.reason,
            detail: r.detail.clone(),
        });
        let faults = usize::from(opts.fault == FaultInjection::DmlRunFailsOnce);
        let start = opts.batch_for(built.doc.ep);
        let floor = opts.tier.batch_cpu as usize;
        Self {
            inner: Arc::new(Inner {
                max_batch: AtomicUsize::new(start),
                governor: Mutex::new(GpuBatchGovernor::new(start as u32, floor as u32)),
                profile,
                doc: Mutex::new(built.doc),
                query: Mutex::new(built.query),
                sink,
                faults_left: AtomicUsize::new(faults),
                opts,
            }),
            report: built.report,
        }
    }

    /// How the provider got onto its first EP.
    pub fn load_report(&self) -> &LoadReport {
        &self.report
    }

    /// EP documents currently run on (changes to CPU after a runtime fault). Queries always run
    /// on the CPU (4.27).
    pub fn active_ep(&self) -> EpActive {
        self.inner.doc.lock().map(|s| s.ep).unwrap_or(EpActive::Cpu)
    }

    /// Current document batch size (halves on slow GPU batches; CPU batch after a fault).
    pub fn current_batch(&self) -> usize {
        self.inner.max_batch.load(Ordering::Relaxed)
    }

    /// Synchronous embed (blocking). The async trait method runs this on the blocking pool.
    pub fn embed_blocking(&self, kind: EmbedKind, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.inner.embed_blocking(kind, texts, None)
    }

    /// [`Self::embed_blocking`] that gives up with `Cancelled` while paused for a fullscreen app
    /// once `cancel` fires (a GPU run that already started still finishes).
    pub fn embed_blocking_cancellable(
        &self,
        kind: EmbedKind,
        texts: &[&str],
        cancel: &CancellationToken,
    ) -> Result<Vec<Vec<f32>>> {
        self.inner.embed_blocking(kind, texts, Some(cancel))
    }
}

impl Inner {
    fn embed_blocking(
        &self,
        kind: EmbedKind,
        texts: &[&str],
        cancel: Option<&CancellationToken>,
    ) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let prepared: Vec<String> = texts
            .iter()
            .map(|t| self.profile.prepare(kind, t))
            .collect();
        let pooling = self.opts.tier.pooling;

        // Queries: the CPU session, never queued behind a GPU batch (4.27).
        if kind == EmbedKind::Query {
            let mut q = self
                .query
                .lock()
                .map_err(|_| internal("query session poisoned"))?;
            if let Some(s) = q.as_mut() {
                let b = self.opts.batch_for(EpActive::Cpu);
                let out = run(&mut s.model, &prepared, b, pooling)
                    .map_err(|e| internal(format!("CPU query embed: {e}")))?;
                drop(q);
                check_vectors(self.profile.dims, texts.len(), &out)?;
                return Ok(out);
            }
        }

        let mut s = self
            .doc
            .lock()
            .map_err(|_| internal("embedder session poisoned"))?;
        let out = if s.ep == EpActive::DirectMl {
            match self.run_gpu(&mut s.model, &prepared, cancel) {
                Ok(v) => v,
                Err(GpuRunError::Cancelled) => return Err(Error::new(ErrorCode::Cancelled)),
                Err(GpuRunError::Fault(e)) => {
                    // 4. Runtime fault on the GPU: fresh CPU session from re-verified files, re-run.
                    tracing::warn!(error = %e, "DirectML run failed; continuing on CPU");
                    let vm = load_verified(self.opts.tier, &self.opts.file, &self.opts.paths)?;
                    let cpu = cpu_session(&vm, self.opts.cpu_threads())
                        .map_err(|e| internal(format!("CPU rebuild after GPU fault: {e}")))?;
                    *s = Session {
                        model: cpu,
                        ep: EpActive::Cpu,
                    };
                    // One CPU session now serves documents and queries (memory).
                    if let Ok(mut q) = self.query.lock() {
                        *q = None;
                    }
                    let b = self.opts.batch_for(EpActive::Cpu);
                    self.max_batch.store(b, Ordering::Relaxed);
                    self.sink.emit(ProviderEvent::EpChanged {
                        model_id: self.profile.model.clone(),
                        from: Some(EpActive::DirectMl),
                        to: EpActive::Cpu,
                        reason_code: EpReason::RuntimeFault,
                        detail: Some(e),
                    });
                    run(&mut s.model, &prepared, b, pooling)
                        .map_err(|e| internal(format!("CPU embed: {e}")))?
                }
            }
        } else {
            let b = self.max_batch.load(Ordering::Relaxed);
            run(&mut s.model, &prepared, b, pooling)
                .map_err(|e| internal(format!("CPU embed: {e}")))?
        };
        drop(s);
        check_vectors(self.profile.dims, texts.len(), &out)?;
        Ok(out)
    }

    /// GPU batches with the fullscreen pause and the > 500 ms halving rule.
    fn run_gpu(
        &self,
        model: &mut TextEmbedding,
        texts: &[String],
        cancel: Option<&CancellationToken>,
    ) -> std::result::Result<Vec<Vec<f32>>, GpuRunError> {
        let pooling = self.opts.tier.pooling;
        let mut out = Vec::with_capacity(texts.len());
        let mut rest = texts;
        while !rest.is_empty() {
            self.wait_while_paused(cancel)?;
            let b = self
                .governor
                .lock()
                .map(|g| g.batch() as usize)
                .unwrap_or(1)
                .max(1);
            let (part, tail) = rest.split_at(b.min(rest.len()));
            rest = tail;
            // One-shot test fault (0 in production).
            if self.faults_left.swap(0, Ordering::AcqRel) > 0 {
                return Err(GpuRunError::Fault(
                    "injected DirectML run failure (FaultInjection::DmlRunFailsOnce)".into(),
                ));
            }
            let t = Instant::now();
            let (v, _) = run_once(model, part, pooling).map_err(GpuRunError::Fault)?;
            if self.opts.fault == FaultInjection::DmlSlowBatches {
                std::thread::sleep(Duration::from_millis(550));
            }
            let ms = t.elapsed().as_millis() as u64;
            out.extend(v);
            if let Ok(mut g) = self.governor.lock()
                && let Some(nb) = g.observe(part.len() as u32, ms)
            {
                self.max_batch.store(nb as usize, Ordering::Relaxed);
                tracing::info!(target: "sr_providers::gpu", batch = nb, ms, "GPU batch over 500 ms: batch size halved");
            }
        }
        Ok(out)
    }

    fn wait_while_paused(
        &self,
        cancel: Option<&CancellationToken>,
    ) -> std::result::Result<(), GpuRunError> {
        let Some(hook) = &self.opts.gpu_pause else {
            return Ok(());
        };
        let mut logged = false;
        while hook() {
            if cancel.is_some_and(CancellationToken::is_cancelled) {
                return Err(GpuRunError::Cancelled);
            }
            if !logged {
                tracing::info!(target: "sr_providers::gpu", "fullscreen app in front: GPU indexing paused");
                logged = true;
            }
            std::thread::sleep(GPU_PAUSE_POLL);
        }
        if logged {
            tracing::info!(target: "sr_providers::gpu", "GPU indexing resumed");
        }
        Ok(())
    }
}

enum GpuRunError {
    Cancelled,
    Fault(String),
}

impl EmbeddingProvider for LocalEmbedder {
    fn profile(&self) -> &EmbeddingProfile {
        &self.inner.profile
    }
    fn max_batch(&self) -> usize {
        self.inner.max_batch.load(Ordering::Relaxed)
    }
    fn embed_batch<'a>(
        &'a self,
        kind: EmbedKind,
        texts: &'a [&'a str],
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<Vec<f32>>>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(Error::new(ErrorCode::Cancelled));
            }
            let inner = self.inner.clone();
            let owned: Vec<String> = texts.iter().map(|s| (*s).to_owned()).collect();
            let c = cancel.clone();
            let task = tokio::task::spawn_blocking(move || {
                let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
                inner.embed_blocking(kind, &refs, Some(&c))
            });
            tokio::select! {
                // A run that already started can't be interrupted; it finishes in the
                // background and its result is dropped.
                () = cancel.cancelled() => Err(Error::new(ErrorCode::Cancelled)),
                r = task => r.map_err(|e| internal(format!("embed task: {e}")))?,
            }
        })
    }
}

/// `Arc<dyn EventSink>` that drops everything.
pub fn noop_sink() -> Arc<dyn EventSink> {
    Arc::new(NoopSink)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cls_and_mean_pooling_normalise() {
        // batch 2, seq 3, dims 2
        let data = [
            3.0, 4.0, /* tok1 */ 1.0, 0.0, /* tok2 */ 9.0, 9.0, // row 0
            0.0, 2.0, /* tok1 */ 0.0, 4.0, /* pad */ 7.0, 7.0, // row 1
        ];
        let cls = pool(&[2, 3, 2], &data, Pooling::Cls, |_, _| true).unwrap();
        assert_eq!(cls[0], vec![0.6, 0.8]);
        assert_eq!(cls[1], vec![0.0, 1.0]);
        // mean over real tokens only (row 1's third token is padding)
        let mean = pool(&[2, 3, 2], &data, Pooling::Mean, |i, j| !(i == 1 && j == 2)).unwrap();
        let m0 = normalize(&[13.0 / 3.0, 13.0 / 3.0]);
        assert!((mean[0][0] - m0[0]).abs() < 1e-6);
        assert!((mean[1][0]).abs() < 1e-6 && (mean[1][1] - 1.0).abs() < 1e-6);
        // already pooled [batch, dims]
        let p = pool(&[1, 2], &[0.0, 5.0], Pooling::Cls, |_, _| true).unwrap();
        assert_eq!(p, vec![vec![0.0, 1.0]]);
        // shape errors
        assert!(pool(&[2, 2], &[1.0], Pooling::Cls, |_, _| true).is_err());
        assert!(pool(&[1, 1, 1, 1], &[1.0], Pooling::Cls, |_, _| true).is_err());
        assert!(pool(&[-1, 2], &[1.0, 2.0], Pooling::Cls, |_, _| true).is_err());
    }

    #[test]
    fn f16_outputs_are_widened_exactly() {
        // O3: an fp16 graph without keep_io_types emits an f16 last_hidden_state.
        let vals: Vec<half::f16> = [0.5f32, -1.25, 3.0, 65504.0, 0.0, 1e-3]
            .iter()
            .map(|x| half::f16::from_f32(*x))
            .collect();
        let t = ort::value::Tensor::from_array(([1usize, 2, 3], vals.clone())).unwrap();
        let dynv = t.into_dyn();
        let (shape, data, dtype) = extract_f32(&dynv).unwrap();
        assert_eq!(dtype, "f16");
        assert_eq!(shape, vec![1, 2, 3]);
        let want: Vec<f32> = vals.iter().map(|x| x.to_f32()).collect();
        assert_eq!(data, want);
        // and f32 passes through
        let t = ort::value::Tensor::from_array(([1usize, 2], vec![1.0f32, 2.0])).unwrap();
        let (_, d, dt) = extract_f32(&t.into_dyn()).unwrap();
        assert_eq!((d, dt), (vec![1.0, 2.0], "f32"));
        // other element types are refused, not misread
        let t = ort::value::Tensor::from_array(([2usize], vec![1i64, 2])).unwrap();
        assert!(extract_f32(&t.into_dyn()).is_err());
    }
}
