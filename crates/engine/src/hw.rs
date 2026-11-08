//! Hardware detection and the tier decision (docs/02 §2.8, Amendments 1, 3.6, 4.10).
//!
//! Silent: the result is stored in catalog `meta.hw_detection` (detected facts) and published as
//! `tier.detected`. User choices (`tier_override`, `use_graphics_card`) live in `settings.toml`
//! (Amendment 4.10). **A changed recommendation never triggers a re-embed by itself.**
//!
//! M1 scope: RAM, physical/logical cores, AVX2/AVX-512 VNNI and free disk on the data volume.
//! GPU enumeration (DXGI + D3D12 feature level / UMA) lands with the GPU path in M2; until then
//! `gpus` is empty, so the plan is `EpPlan::Cpu` (Amendment 2: "M1 runs on the CPU EP through the
//! same EpPlan interface").

use std::path::Path;

use sr_common::config::Settings;
use sr_common::tier::{HwInfo, Thresholds, Tier, TierDecision, round_ram, select_tier};

/// Something that can describe the machine (tests inject fixed values).
pub trait HwProbe: Send + Sync {
    fn probe(&self, data_dir: &Path) -> HwInfo;
}

/// The real machine.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemProbe;

impl HwProbe for SystemProbe {
    fn probe(&self, data_dir: &Path) -> HwInfo {
        let logical = std::thread::available_parallelism()
            .map(|n| u32::try_from(n.get()).unwrap_or(u32::MAX))
            .unwrap_or(1);
        #[cfg(target_arch = "x86_64")]
        let (avx2, vnni) = (
            std::arch::is_x86_feature_detected!("avx2"),
            std::arch::is_x86_feature_detected!("avx512vnni"),
        );
        #[cfg(not(target_arch = "x86_64"))]
        let (avx2, vnni) = (false, false);
        HwInfo {
            total_ram: round_ram(crate::win::total_ram().unwrap_or(0)),
            physical_cores: crate::win::physical_cores().unwrap_or((logical / 2).max(1)),
            logical_cores: logical,
            avx2,
            avx512_vnni: vnni,
            free_disk: crate::win::free_disk(data_dir).unwrap_or(0),
            gpus: Vec::new(),
        }
    }
}

/// User choices for the tier (Amendment 4.10: `settings.toml` `[search]`, Advanced).
///
/// Typed in sr-common (`Search::tier_override`, `Search::use_graphics_card`); the lenient loader
/// has already reset bad values to their defaults (Automatic, graphics card on), so this never
/// fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TierChoices {
    pub tier_override: Option<Tier>,
    pub use_gpu: bool,
}

impl Default for TierChoices {
    fn default() -> Self {
        Self::from_settings(&Settings::default())
    }
}

impl TierChoices {
    pub fn from_settings(s: &Settings) -> Self {
        Self {
            tier_override: s.search.tier_override,
            use_gpu: s.search.use_graphics_card,
        }
    }
}

/// Detect and decide.
pub fn decide(probe: &dyn HwProbe, data_dir: &Path) -> (HwInfo, TierDecision) {
    let hw = probe.probe(data_dir);
    // M2-G: GPUs are enumerated by `gpu::GpuSystemProbe` (the default probe); no M1 "CPU only" note.
    let d = select_tier(&hw, &Thresholds::default());
    (hw, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sr_common::tier::{EpPlan, GIB};

    struct Fixed(HwInfo);
    impl HwProbe for Fixed {
        fn probe(&self, _: &Path) -> HwInfo {
            self.0.clone()
        }
    }

    #[test]
    fn system_probe_is_plausible_and_cpu_only_in_m1() {
        let dir = std::env::temp_dir();
        let (hw, d) = decide(&SystemProbe, &dir);
        assert!(hw.total_ram >= GIB && hw.physical_cores >= 1 && hw.free_disk > 0);
        assert!(hw.gpus.is_empty());
        assert_eq!(d.ep, EpPlan::Cpu);
    }

    #[test]
    fn decision_follows_select_tier() {
        let small = HwInfo {
            total_ram: 4 * GIB,
            physical_cores: 2,
            logical_cores: 4,
            avx2: true,
            avx512_vnni: false,
            free_disk: 100 * GIB,
            gpus: vec![],
        };
        assert_eq!(
            decide(&Fixed(small.clone()), Path::new("C:\\")).1.tier,
            Tier::Minimum
        );
        let big = HwInfo {
            total_ram: 32 * GIB,
            physical_cores: 8,
            ..small
        };
        assert_eq!(
            decide(&Fixed(big), Path::new("C:\\")).1.tier,
            Tier::Standard
        );
    }

    #[test]
    fn choices_come_from_settings_toml() {
        let (s, _, _) = sr_common::config::from_toml_str(
            "[search]\ntier_override = \"better\"\nuse_graphics_card = \"off\"\n",
        );
        let c = TierChoices::from_settings(&s);
        assert_eq!(c.tier_override, Some(Tier::Standard));
        assert!(!c.use_gpu);
        let d = TierChoices::from_settings(&Settings::default());
        assert_eq!(
            d,
            TierChoices {
                tier_override: None,
                use_gpu: true
            }
        );
        assert_eq!(TierChoices::default(), d);
        // Bad values: defaults, no failure (the loader reset them).
        let (s, issues, _) = sr_common::config::from_toml_str(
            "[search]\ntier_override = 7\nuse_graphics_card = \"sometimes\"\n",
        );
        assert_eq!(issues.len(), 2, "{issues:?}");
        assert_eq!(TierChoices::from_settings(&s), d);
        // Typed fields, not leftovers in the unknown-key map.
        assert!(s.search.extra.is_empty());
    }
}
