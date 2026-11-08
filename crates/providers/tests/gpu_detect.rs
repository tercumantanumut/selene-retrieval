//! DXGI / D3D12 detection on the real machine (M2-G). `SR_REQUIRE_DML=1` turns "no usable GPU"
//! into a failure (the gate machine has an RTX 5090 + WARP).
#![allow(clippy::expect_used, clippy::unwrap_used)]

use sr_common::tier::{GIB, GpuClass, Thresholds, classify, gpu_usable};

fn require_gpu() -> bool {
    std::env::var("SR_REQUIRE_DML").is_ok_and(|v| v == "1")
}

#[test]
fn enumerates_adapters_with_uma_fl11_and_driver_version() {
    let t0 = std::time::Instant::now();
    let gpus = sr_providers::dxgi::enumerate();
    let ms = t0.elapsed().as_millis();
    for g in &gpus {
        println!(
            "#{} {:?} {:04x}:{:04x} vram {} MiB shared {} MiB sw {} fl11 {} uma {} driver {} → {:?}",
            g.dxgi_index,
            g.name,
            g.vendor_id,
            g.device_id,
            g.dedicated_vram >> 20,
            g.shared_mem >> 20,
            g.is_software,
            g.d3d12_fl11,
            g.uma,
            g.driver_version_string(),
            classify(g, &Thresholds::default())
        );
    }
    println!("enumerate: {ms} ms");
    // Indices are DXGI order, unique and dense.
    for (i, g) in gpus.iter().enumerate() {
        assert_eq!(g.dxgi_index as usize, i);
    }
    // Every machine with DXGI has WARP; it is never usable.
    if let Some(w) = gpus.iter().find(|g| g.is_software) {
        assert!(!gpu_usable(w), "WARP must be excluded");
        assert!(
            !w.d3d12_fl11,
            "no D3D12 device is created for software adapters"
        );
    }
    let usable: Vec<_> = gpus.iter().filter(|g| gpu_usable(g)).collect();
    if usable.is_empty() {
        assert!(
            !require_gpu(),
            "SR_REQUIRE_DML=1 but no usable GPU was found: {gpus:?}"
        );
        println!("SKIP: no usable GPU");
        return;
    }
    for g in &usable {
        assert!(g.d3d12_fl11);
        assert_ne!(g.driver_version, 0, "driver version read for {}", g.name);
    }
    if require_gpu() {
        let d = usable
            .iter()
            .find(|g| classify(g, &Thresholds::default()) == GpuClass::Dedicated)
            .expect("the gate machine has a dedicated GPU");
        assert!(!d.uma && d.dedicated_vram >= 4 * GIB);
    }
}

#[test]
fn enumeration_maps_no_directml_and_foreground_check_is_cheap() {
    let _ = sr_providers::dxgi::enumerate();
    assert!(
        sr_providers::dml::loaded_module_path("DirectML.dll").is_none(),
        "detection must not load DirectML.dll"
    );
    if let Some(p) = sr_providers::dml::loaded_module_path("d3d12.dll") {
        assert!(
            p.to_string_lossy().to_lowercase().contains(r"\system32\"),
            "d3d12.dll from System32 only: {p:?}"
        );
    }
    let t0 = std::time::Instant::now();
    let busy = sr_providers::dxgi::foreground_busy();
    println!("foreground_busy = {busy} ({} µs)", t0.elapsed().as_micros());
    assert!(t0.elapsed().as_millis() < 500);
}
