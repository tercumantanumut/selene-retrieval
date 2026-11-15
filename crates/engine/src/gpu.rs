//! GPU on by default (Amendment 2; docs/02 §2.8, "Execution-provider selection"). Owner: M2-G.
//!
//! * [`GpuSystemProbe`]: the real machine **with** GPUs (`sr_providers::dxgi`: DXGI + D3D12 FL 11.0,
//!   UMA, WARP excluded, driver version). It replaces `hw::SystemProbe` as the default probe, so
//!   `select_tier` sees the adapters (Amendment 4.13).
//! * [`GpuContext::detect`]: detection + `select_tier`, then [`sr_common::tier::effective_ep`] with
//!   the persisted [`GpuState`] (3-faults ledger, per-driver probe cache) and the Advanced
//!   "Use graphics card" switch → the plan for every **new** embedding session.
//! * [`GpuContext::apply`]: fills `LocalOptions` (plan, VRAM batch cap, physical cores, the
//!   fullscreen back-off hook `dxgi::foreground_busy`).
//! * [`GpuContext::sink`]: wraps the provider event sink; every `EpChanged{RuntimeFault}` (device
//!   removed / TDR / DirectML run error, already recovered on the CPU by the provider) is recorded
//!   in the ledger. The 3rd fault in 24 h keeps that adapter on the CPU until its driver changes.
//! * [`GpuContext::record_load`]: caches the iGPU / small-dGPU probe outcome per driver version.
//!
//! Everything here is silent: no user-visible error, only `ep.changed` (diagnostics) and logs.
//! Queries always embed on the CPU (4.27; the provider keeps a CPU session for them).

use std::path::Path;
use std::sync::{Arc, Mutex};

use sr_catalog::Catalog;
use sr_catalog::repo::meta;
use sr_common::error::Error;
use sr_common::tier::{
    EpActive, EpPlan, EpPlanReason, GpuInfo, GpuKey, GpuState, HwInfo, ProbeResult, TierDecision,
    effective_ep,
};
use sr_providers::{EpReason, EventSink, ProviderEvent};

use crate::clock::SharedClock;
use crate::hw::{HwProbe, SystemProbe, TierChoices};

/// The real machine including its GPUs (≈ 0.2–1 s: one D3D12 device per adapter).
#[derive(Debug, Default, Clone, Copy)]
pub struct GpuSystemProbe;

impl HwProbe for GpuSystemProbe {
    fn probe(&self, data_dir: &Path) -> HwInfo {
        let mut hw = SystemProbe.probe(data_dir);
        hw.gpus = sr_providers::dxgi::enumerate();
        hw
    }
}

/// Persisted GPU policy state (catalog `meta`, key [`GpuState::META_KEY`]); default if absent or
/// unreadable (a bad value only forgets old faults / probe results).
pub fn load_state(c: &rusqlite::Connection) -> Result<GpuState, Error> {
    Ok(meta::get(c, GpuState::META_KEY)?
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default())
}

pub fn save_state(c: &rusqlite::Connection, s: &GpuState) -> Result<(), Error> {
    let v = serde_json::to_string(s)
        .map_err(|e| Error::new(sr_common::error::ErrorCode::Internal).with_source(e))?;
    meta::set(c, GpuState::META_KEY, &v)
}

/// Detection result + the GPU plan for new sessions.
#[derive(Clone)]
pub struct GpuContext {
    pub hw: HwInfo,
    pub decision: TierDecision,
    /// Plan after the user switch, fault ledger and probe cache.
    pub plan: EpPlan,
    pub reason: EpPlanReason,
    /// The plan's adapter (VRAM cap, ledger key).
    pub adapter: Option<GpuInfo>,
    pub use_gpu: bool,
    state: Arc<Mutex<GpuState>>,
    cat: Arc<Catalog>,
    clock: SharedClock,
}

impl std::fmt::Debug for GpuContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuContext")
            .field("tier", &self.decision.tier)
            .field("detected", &self.decision.ep)
            .field("plan", &self.plan)
            .field("reason", &self.reason)
            .field("adapter", &self.adapter.as_ref().map(|a| &a.name))
            .finish()
    }
}

impl GpuContext {
    /// Detect (off the async threads) and decide.
    pub async fn detect(
        probe: Arc<dyn HwProbe>,
        data_dir: &Path,
        cat: Arc<Catalog>,
        clock: SharedClock,
        choices: TierChoices,
    ) -> Result<Self, Error> {
        let dir = data_dir.to_path_buf();
        let (hw, decision) = tokio::task::spawn_blocking(move || crate::hw::decide(&*probe, &dir))
            .await
            .map_err(|e| Error::new(sr_common::error::ErrorCode::Internal).with_source(e))?;
        let idx = match decision.ep {
            EpPlan::Cpu => None,
            EpPlan::DirectMl { dxgi_index } | EpPlan::ProbeDirectMl { dxgi_index } => {
                Some(dxgi_index)
            }
        };
        let adapter = idx.and_then(|i| hw.gpus.iter().find(|g| g.dxgi_index == i).cloned());
        let mut state = cat.read_async(load_state).await?;
        if let Some(a) = &adapter
            && state.ledger.forget_stale_driver(GpuKey::of(a))
        {
            tracing::info!(target: "sr_engine::gpu", adapter = %a.name, driver = %a.driver_version_string(), "graphics driver changed: earlier GPU faults forgotten");
            let s = state.clone();
            cat.write(move |c| save_state(c, &s)).await?;
        }
        let (plan, reason) = effective_ep(&decision, &hw.gpus, choices.use_gpu, &state);
        tracing::info!(target: "sr_engine::gpu", tier = ?decision.tier, detected = ?decision.ep, ?plan, ?reason, adapter = ?adapter.as_ref().map(|a| &a.name), "embedding device plan");
        Ok(Self {
            hw,
            decision,
            plan,
            reason,
            adapter,
            use_gpu: choices.use_gpu,
            state: Arc::new(Mutex::new(state)),
            cat,
            clock,
        })
    }

    /// Fixed context (tests, or a provider that is not local).
    pub fn fixed(
        hw: HwInfo,
        decision: TierDecision,
        use_gpu: bool,
        state: GpuState,
        cat: Arc<Catalog>,
        clock: SharedClock,
    ) -> Self {
        let (plan, reason) = effective_ep(&decision, &hw.gpus, use_gpu, &state);
        let adapter = match decision.ep {
            EpPlan::Cpu => None,
            EpPlan::DirectMl { dxgi_index } | EpPlan::ProbeDirectMl { dxgi_index } => {
                hw.gpus.iter().find(|g| g.dxgi_index == dxgi_index).cloned()
            }
        };
        Self {
            hw,
            decision,
            plan,
            reason,
            adapter,
            use_gpu,
            state: Arc::new(Mutex::new(state)),
            cat,
            clock,
        }
    }

    pub fn state(&self) -> GpuState {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Whether a new index would be built on the GPU (picks the fp16 vs CPU file, §2.8 sticky
    /// variant). A pending iGPU probe counts as GPU: the fp16/fp32 files are EP-portable.
    pub fn gpu_resolved(&self) -> bool {
        self.plan != EpPlan::Cpu
    }

    /// Fill the GPU fields of a local provider's options.
    #[cfg(feature = "local")]
    pub fn apply(&self, o: &mut sr_providers::LocalOptions) {
        self.apply_with_wait(o, None);
    }

    /// [`Self::apply`], recording full-screen waits in `wait` (R-UX32: status line + the
    /// 5-minute processor fallback, see [`crate::fullscreen`]).
    #[cfg(feature = "local")]
    pub fn apply_with_wait(
        &self,
        o: &mut sr_providers::LocalOptions,
        wait: Option<&Arc<crate::fullscreen::FullscreenWait>>,
    ) {
        o.ep = self.plan;
        o.use_gpu = self.use_gpu;
        o.dedicated_vram = self.adapter.as_ref().map(|a| a.dedicated_vram);
        if self.hw.physical_cores > 0 {
            o.physical_cores = self.hw.physical_cores;
        }
        // Debug builds only (ignored in release, like every dev hook): tests that run the real
        // daemon on a shared desktop must not stall because someone has a full-screen window open
        // (M2 tester: a 1-file index waited 10 min behind QUNS_BUSY and the test timed out).
        if sr_client::naming::dev_env("SR_NO_GPU_PAUSE").is_none() {
            o.gpu_pause = Some(match wait {
                Some(w) => w.hook(Arc::new(sr_providers::dxgi::foreground_busy)),
                None => Arc::new(sr_providers::dxgi::foreground_busy),
            });
        }
    }

    /// Wrap a freshly loaded local embedder with the 5-minute processor fallback (R-UX32) when
    /// it runs on the GPU with the pause hook installed; otherwise return it as it is. The CPU
    /// session is built from the **same** options (same file), on the CPU EP, without a pause.
    #[cfg(feature = "local")]
    pub fn with_fullscreen_fallback(
        &self,
        e: sr_providers::LocalEmbedder,
        opts: &sr_providers::LocalOptions,
        sink: Arc<dyn EventSink>,
        wait: &Arc<crate::fullscreen::FullscreenWait>,
    ) -> Arc<dyn sr_providers::EmbeddingProvider> {
        if e.active_ep() != EpActive::DirectMl || opts.gpu_pause.is_none() {
            return Arc::new(e);
        }
        let mut cpu = opts.clone();
        cpu.ep = EpPlan::Cpu;
        cpu.use_gpu = false;
        cpu.gpu_pause = None;
        let build: crate::fullscreen::CpuBuilder = Arc::new(move || {
            let (cpu, sink) = (cpu.clone(), sink.clone());
            Box::pin(async move {
                let c = sr_providers::LocalEmbedder::load(cpu, sink).await?;
                Ok(Arc::new(c) as Arc<dyn sr_providers::EmbeddingProvider>)
            })
        });
        Arc::new(crate::fullscreen::FullscreenFallback::new(
            Arc::new(e),
            build,
            Arc::new(sr_providers::dxgi::foreground_busy),
            wait.clone(),
        ))
    }

    /// Provider event sink that records GPU runtime faults, then forwards everything to `inner`.
    pub fn sink(&self, inner: Arc<dyn EventSink>) -> Arc<dyn EventSink> {
        Arc::new(FaultLedgerSink {
            inner,
            key: self.adapter.as_ref().map(GpuKey::of),
            state: self.state.clone(),
            cat: self.cat.clone(),
            clock: self.clock.clone(),
            persist: Arc::new(Mutex::new(())),
        })
    }

    /// Cache the outcome of a one-off iGPU probe (only when one actually ran; a missing or
    /// untrusted `DirectML.dll` says nothing about the adapter and is not cached).
    #[cfg(feature = "local")]
    pub fn record_load(&self, r: &sr_providers::LoadReport) -> Result<(), Error> {
        if self.reason != EpPlanReason::ProbePending {
            return Ok(());
        }
        let Some(a) = &self.adapter else {
            return Ok(());
        };
        let use_gpu = match r.reason {
            EpReason::Planned => r.ep == EpActive::DirectMl,
            EpReason::ProbeTooSlow
            | EpReason::CanaryMismatch
            | EpReason::CanaryNonFinite
            | EpReason::RegistrationFailed => false,
            _ => return Ok(()),
        };
        let pr = ProbeResult {
            key: GpuKey::of(a),
            use_gpu,
            speedup: r.probe_speedup,
            at_ms: self.clock.now_ms(),
        };
        tracing::info!(target: "sr_engine::gpu", adapter = %a.name, use_gpu, speedup = ?r.probe_speedup, "integrated-GPU probe result cached for this driver");
        let s = {
            let mut g = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            g.probes.put(pr);
            g.clone()
        };
        self.cat.write_blocking(move |c| save_state(c, &s))
    }
}

/// Records `EpChanged{DirectMl → Cpu, RuntimeFault}` in the fault ledger (persisted off-thread:
/// `emit` must not block the embedding thread for a catalog write).
struct FaultLedgerSink {
    inner: Arc<dyn EventSink>,
    key: Option<GpuKey>,
    state: Arc<Mutex<GpuState>>,
    cat: Arc<Catalog>,
    clock: SharedClock,
    persist: Arc<Mutex<()>>,
}

impl EventSink for FaultLedgerSink {
    fn emit(&self, ev: ProviderEvent) {
        if let (
            ProviderEvent::EpChanged {
                from: Some(EpActive::DirectMl),
                to: EpActive::Cpu,
                reason_code: EpReason::RuntimeFault,
                detail,
                ..
            },
            Some(key),
        ) = (&ev, self.key)
        {
            let now = self.clock.now_ms();
            let (s, blocked) = {
                let mut g = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let b = g.ledger.record(key, now);
                (g.clone(), b)
            };
            if blocked {
                tracing::warn!(target: "sr_engine::gpu", ?detail, "third GPU fault in 24 h: using the processor until the graphics driver changes");
            } else {
                tracing::warn!(target: "sr_engine::gpu", ?detail, faults = s.ledger.recent(key, now), "GPU fault; this batch was redone on the processor");
            }
            drop(s);
            let cat = self.cat.clone();
            let state = self.state.clone();
            let persist = self.persist.clone();
            let spawned = std::thread::Builder::new()
                .name("sr-gpu-ledger".into())
                .spawn(move || {
                    // Serialised, and always the latest state: an older snapshot can never land
                    // after a newer one (it would erase a block).
                    let _g = persist.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    let s = state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone();
                    if let Err(e) = cat.write_blocking(move |c| save_state(c, &s)) {
                        tracing::warn!(target: "sr_engine::gpu", error = %e, "could not persist GPU fault ledger");
                    }
                });
            if spawned.is_err() {
                tracing::warn!(target: "sr_engine::gpu", "could not persist GPU fault ledger (thread)");
            }
        }
        self.inner.emit(ev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sr_common::tier::{GIB, Thresholds, Tier, select_tier};

    fn dgpu() -> GpuInfo {
        GpuInfo {
            name: "Test dGPU".into(),
            vendor_id: 0x10de,
            device_id: 0x2b85,
            dedicated_vram: 8 * GIB,
            shared_mem: 16 * GIB,
            is_software: false,
            d3d12_fl11: true,
            uma: false,
            dxgi_index: 0,
            driver_version: 7,
        }
    }

    fn hw(g: Vec<GpuInfo>) -> HwInfo {
        HwInfo {
            total_ram: 16 * GIB,
            physical_cores: 8,
            logical_cores: 16,
            avx2: true,
            avx512_vnni: false,
            free_disk: 100 * GIB,
            gpus: g,
        }
    }

    fn cat() -> (tempfile::TempDir, Arc<Catalog>) {
        let t = tempfile::tempdir().unwrap();
        let c = Arc::new(Catalog::open(&t.path().join("c.db")).unwrap());
        (t, c)
    }

    #[test]
    fn state_round_trips_through_meta_and_bad_values_reset() {
        let (_t, cat) = cat();
        assert_eq!(cat.read(load_state).unwrap(), GpuState::default());
        let mut s = GpuState::default();
        s.ledger.record(GpuKey::of(&dgpu()), 5);
        let s2 = s.clone();
        cat.write_blocking(move |c| save_state(c, &s2)).unwrap();
        assert_eq!(cat.read(load_state).unwrap(), s);
        cat.write_blocking(|c| meta::set(c, GpuState::META_KEY, "not json"))
            .unwrap();
        assert_eq!(cat.read(load_state).unwrap(), GpuState::default());
    }

    #[test]
    fn runtime_faults_are_recorded_and_the_third_blocks_the_gpu() {
        let (_t, cat) = cat();
        let clock: SharedClock = crate::clock::system();
        let h = hw(vec![dgpu()]);
        // The DirectML fault bookkeeping, on the platform that has DirectML.
        let d = sr_common::tier::select_tier_on(
            &h,
            &Thresholds::default(),
            sr_common::tier::TierPlatform::WINDOWS_X64,
        );
        assert_eq!(d.tier, Tier::High);
        let ctx = GpuContext::fixed(
            h.clone(),
            d.clone(),
            true,
            GpuState::default(),
            cat.clone(),
            clock.clone(),
        );
        assert_eq!(ctx.plan, EpPlan::DirectMl { dxgi_index: 0 });
        let mem = sr_providers::MemorySink::new();
        let sink = ctx.sink(mem.clone());
        let fault = || ProviderEvent::EpChanged {
            model_id: "m".into(),
            from: Some(EpActive::DirectMl),
            to: EpActive::Cpu,
            reason_code: EpReason::RuntimeFault,
            detail: Some("887A0005 device removed".into()),
        };
        for _ in 0..3 {
            sink.emit(fault());
        }
        // Forwarded unchanged.
        assert_eq!(mem.take().len(), 3);
        assert!(ctx.state().ledger.blocks(GpuKey::of(&dgpu())));
        // Persisted (off-thread): a new start plans the CPU for this driver …
        let mut persisted = GpuState::default();
        for _ in 0..50 {
            persisted = cat.read(load_state).unwrap();
            if persisted.ledger.blocked.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let again = GpuContext::fixed(
            h.clone(),
            d.clone(),
            true,
            persisted.clone(),
            cat.clone(),
            clock.clone(),
        );
        assert_eq!(
            (again.plan, again.reason),
            (EpPlan::Cpu, EpPlanReason::FaultLimit)
        );
        // … and the GPU again after a driver update.
        let mut upd = h.clone();
        upd.gpus[0].driver_version = 8;
        let next = GpuContext::fixed(upd, d, true, persisted, cat, clock);
        assert_eq!(next.plan, EpPlan::DirectMl { dxgi_index: 0 });
        // Other events pass through without touching the ledger.
        let before = ctx.state();
        ctx.sink(mem.clone()).emit(ProviderEvent::EpChanged {
            model_id: "m".into(),
            from: None,
            to: EpActive::DirectMl,
            reason_code: EpReason::Planned,
            detail: None,
        });
        assert_eq!(ctx.state(), before);
    }

    #[test]
    fn system_probe_with_gpus_sees_this_machine() {
        let dir = std::env::temp_dir();
        let hw = GpuSystemProbe.probe(&dir);
        for g in &hw.gpus {
            println!(
                "{} {:?} uma {} fl11 {}",
                g.dxgi_index, g.name, g.uma, g.d3d12_fl11
            );
        }
        if std::env::var("SR_REQUIRE_DML").is_ok_and(|v| v == "1") {
            let d = select_tier(&hw, &Thresholds::default());
            assert!(matches!(d.ep, EpPlan::DirectMl { .. }), "{d:?}");
        }
    }
}

#[cfg(all(test, feature = "local"))]
mod probe_cache_tests {
    use super::*;
    use sr_common::tier::{GIB, Thresholds};

    fn igpu() -> GpuInfo {
        GpuInfo {
            name: "Test iGPU".into(),
            vendor_id: 0x8086,
            device_id: 0x46a6,
            dedicated_vram: 128 << 20,
            shared_mem: 16 * GIB,
            is_software: false,
            d3d12_fl11: true,
            uma: true,
            dxgi_index: 0,
            driver_version: 31,
        }
    }

    fn report(ep: EpActive, reason: EpReason, speedup: Option<f32>) -> sr_providers::LoadReport {
        sr_providers::LoadReport {
            ep,
            reason,
            detail: None,
            dml: None,
            canary_min_cos: Some(0.9999),
            probe_speedup: speedup,
            output_dtype: Some("f32".into()),
            load_ms: 1,
        }
    }

    #[test]
    fn probe_outcome_is_cached_per_driver_and_skips_the_next_probe() {
        let t = tempfile::tempdir().unwrap();
        let cat = Arc::new(Catalog::open(&t.path().join("c.db")).unwrap());
        let clock = crate::clock::system();
        let hw = HwInfo {
            total_ram: 16 * GIB,
            physical_cores: 4,
            logical_cores: 8,
            avx2: true,
            avx512_vnni: false,
            free_disk: 100 * GIB,
            gpus: vec![igpu()],
        };
        // The DirectML probe cache, on the platform that has DirectML.
        let d = sr_common::tier::select_tier_on(
            &hw,
            &Thresholds::default(),
            sr_common::tier::TierPlatform::WINDOWS_X64,
        );
        assert_eq!(d.ep, EpPlan::ProbeDirectMl { dxgi_index: 0 });
        let ctx = GpuContext::fixed(
            hw.clone(),
            d.clone(),
            true,
            GpuState::default(),
            cat.clone(),
            clock.clone(),
        );
        assert_eq!(ctx.reason, EpPlanReason::ProbePending);
        // A missing DLL says nothing about the adapter: not cached.
        ctx.record_load(&report(EpActive::Cpu, EpReason::DllMissing, None))
            .unwrap();
        assert!(cat.read(load_state).unwrap().probes.entries.is_empty());
        // Too slow: cached as CPU; the next start plans the CPU without probing.
        ctx.record_load(&report(EpActive::Cpu, EpReason::ProbeTooSlow, Some(0.8)))
            .unwrap();
        let st = cat.read(load_state).unwrap();
        let next = GpuContext::fixed(
            hw.clone(),
            d.clone(),
            true,
            st.clone(),
            cat.clone(),
            clock.clone(),
        );
        assert_eq!(
            (next.plan, next.reason),
            (EpPlan::Cpu, EpPlanReason::ProbeCachedSlow)
        );
        // New driver → probe again; this time it wins → DirectML without probing next time.
        let mut hw2 = hw.clone();
        hw2.gpus[0].driver_version = 32;
        let again = GpuContext::fixed(hw2.clone(), d.clone(), true, st, cat.clone(), clock.clone());
        assert_eq!(again.reason, EpPlanReason::ProbePending);
        again
            .record_load(&report(EpActive::DirectMl, EpReason::Planned, Some(1.9)))
            .unwrap();
        let st = cat.read(load_state).unwrap();
        assert_eq!(st.probes.entries.len(), 1, "one entry per adapter");
        let fast = GpuContext::fixed(hw2, d, true, st, cat, clock);
        assert_eq!(
            (fast.plan, fast.reason),
            (
                EpPlan::DirectMl { dxgi_index: 0 },
                EpPlanReason::ProbeCachedFast
            )
        );
    }
}
