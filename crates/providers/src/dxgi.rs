//! GPU enumeration for the tier decision (Amendments 1, 2, 4.13; §2.8) and the "fullscreen app in
//! front" check (good citizenship).
//!
//! [`enumerate`] walks `IDXGIFactory1::EnumAdapters1` and, for every hardware adapter, creates a
//! D3D12 device at feature level 11.0 (the DirectML minimum) and reads
//! `D3D12_FEATURE_DATA_ARCHITECTURE1.UMA`, the reliable integrated-vs-dedicated signal (APUs can
//! report a multi-GiB BIOS carve-out as `DedicatedVideoMemory`). WARP / software adapters are
//! reported with `is_software` and never get a device. The user-mode driver version comes from
//! `IDXGIAdapter::CheckInterfaceSupport(IDXGIDevice)`; it keys the probe cache and the 3-faults rule.
//!
//! No admin, no network, about 0.2 s (one D3D12 device per adapter): run it off the UI thread.
//! `dxgi.dll` / `d3d12.dll` are resolved from System32 only ([`crate::sys`]); nothing is imported
//! by name, so sr-daemon's delay-load rule (4.8) holds and `DirectML.dll` is not touched.
//! Never fails: any error yields fewer adapters (and so, at worst, the CPU).

use sr_common::tier::GpuInfo;

/// Adapters, in DXGI order (`dxgi_index` = DirectML device id). Empty off Windows.
pub fn enumerate() -> Vec<GpuInfo> {
    imp::enumerate()
}

/// A fullscreen Direct3D app (game) or presentation is in the foreground, or the user is busy
/// (`SHQueryUserNotificationState` = `QUNS_RUNNING_D3D_FULL_SCREEN` | `QUNS_BUSY` |
/// `QUNS_PRESENTATION_MODE`). GPU indexing pauses while this is true (§2.8).
pub fn foreground_busy() -> bool {
    imp::foreground_busy()
}

#[cfg(not(all(windows, feature = "gpu-detect")))]
mod imp {
    use super::GpuInfo;
    pub fn enumerate() -> Vec<GpuInfo> {
        Vec::new()
    }
    pub fn foreground_busy() -> bool {
        false
    }
}

#[cfg(all(windows, feature = "gpu-detect"))]
#[allow(unsafe_code)] // Win32 FFI: DXGI / D3D12 COM, SHQueryUserNotificationState. Reviewed (M2-G).
mod imp {
    use std::ffi::c_void;

    use windows::Win32::Graphics::Direct3D::{D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D12::{
        D3D12_FEATURE_ARCHITECTURE1, D3D12_FEATURE_DATA_ARCHITECTURE1, ID3D12Device,
    };
    use windows::Win32::Graphics::Dxgi::{
        DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1,
    };
    use windows::core::{GUID, HRESULT, Interface};

    use super::GpuInfo;
    use crate::sys::{Proc, system32_proc};

    type CreateDxgiFactory1 = unsafe extern "system" fn(*const GUID, *mut *mut c_void) -> HRESULT;
    type D3d12CreateDevice = unsafe extern "system" fn(
        *mut c_void,
        D3D_FEATURE_LEVEL,
        *const GUID,
        *mut *mut c_void,
    ) -> HRESULT;
    type ShQueryUserNotificationState = unsafe extern "system" fn(*mut i32) -> HRESULT;

    /// More adapters than this is not a real machine; stop enumerating.
    const MAX_ADAPTERS: u32 = 16;

    pub fn enumerate() -> Vec<GpuInfo> {
        let mut out = Vec::new();
        let Some(create) = system32_proc("dxgi.dll", c"CreateDXGIFactory1") else {
            tracing::warn!("dxgi.dll / CreateDXGIFactory1 unavailable: no GPU detection");
            return out;
        };
        // SAFETY: CreateDXGIFactory1 has exactly this documented signature.
        let create: CreateDxgiFactory1 = unsafe { std::mem::transmute::<Proc, _>(create) };
        let mut raw = std::ptr::null_mut();
        // SAFETY: valid IID pointer and out pointer.
        let hr = unsafe { create(&IDXGIFactory1::IID, &mut raw) };
        if hr.is_err() || raw.is_null() {
            tracing::warn!(hr = hr.0, "CreateDXGIFactory1 failed: no GPU detection");
            return out;
        }
        // SAFETY: on success `raw` is an owned IDXGIFactory1 reference (released on drop).
        let factory = unsafe { IDXGIFactory1::from_raw(raw) };
        let d3d = system32_proc("d3d12.dll", c"D3D12CreateDevice")
            // SAFETY: D3D12CreateDevice has exactly this documented signature.
            .map(|p| unsafe { std::mem::transmute::<Proc, D3d12CreateDevice>(p) });
        for i in 0..MAX_ADAPTERS {
            // SAFETY: plain COM call; DXGI_ERROR_NOT_FOUND ends the list.
            let Ok(a) = (unsafe { factory.EnumAdapters1(i) }) else {
                break;
            };
            // SAFETY: plain COM call on a live adapter.
            let Ok(d) = (unsafe { a.GetDesc1() }) else {
                continue;
            };
            let name_len = d.Description.iter().position(|&c| c == 0).unwrap_or(128);
            let is_software = d.Flags & (DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0;
            let (d3d12_fl11, uma) = match (is_software, d3d) {
                (false, Some(f)) => d3d12_probe(f, &a),
                _ => (false, false),
            };
            // SAFETY: plain COM call; returns the UMD version for IDXGIDevice.
            let driver_version = unsafe { a.CheckInterfaceSupport(&IDXGIDevice::IID) }
                .map(|v| v as u64)
                .unwrap_or(0);
            out.push(GpuInfo {
                name: String::from_utf16_lossy(&d.Description[..name_len]),
                vendor_id: d.VendorId,
                device_id: d.DeviceId,
                dedicated_vram: d.DedicatedVideoMemory as u64,
                shared_mem: d.SharedSystemMemory as u64,
                is_software,
                d3d12_fl11,
                uma,
                dxgi_index: i,
                driver_version,
            });
        }
        out
    }

    /// (a D3D12 device at FL 11.0 can be created, the adapter is UMA).
    fn d3d12_probe(create: D3d12CreateDevice, a: &IDXGIAdapter1) -> (bool, bool) {
        let mut raw = std::ptr::null_mut();
        // SAFETY: `a` is a live adapter (borrowed for the call); valid IID and out pointer.
        let hr = unsafe {
            create(
                a.as_raw(),
                D3D_FEATURE_LEVEL_11_0,
                &ID3D12Device::IID,
                &mut raw,
            )
        };
        if hr.is_err() || raw.is_null() {
            return (false, false);
        }
        // SAFETY: on success `raw` is an owned ID3D12Device reference (released on drop).
        let dev = unsafe { ID3D12Device::from_raw(raw) };
        let mut arch = D3D12_FEATURE_DATA_ARCHITECTURE1 {
            NodeIndex: 0,
            ..Default::default()
        };
        // SAFETY: `arch` is the documented struct for this feature, size passed exactly.
        let ok = unsafe {
            dev.CheckFeatureSupport(
                D3D12_FEATURE_ARCHITECTURE1,
                (&raw mut arch).cast(),
                size_of::<D3D12_FEATURE_DATA_ARCHITECTURE1>() as u32,
            )
        }
        .is_ok();
        (true, ok && arch.UMA.as_bool())
    }

    pub fn foreground_busy() -> bool {
        const QUNS_BUSY: i32 = 2;
        const QUNS_RUNNING_D3D_FULL_SCREEN: i32 = 3;
        const QUNS_PRESENTATION_MODE: i32 = 4;
        let Some(p) = system32_proc("shell32.dll", c"SHQueryUserNotificationState") else {
            return false;
        };
        // SAFETY: SHQueryUserNotificationState has exactly this documented signature.
        let f: ShQueryUserNotificationState = unsafe { std::mem::transmute::<Proc, _>(p) };
        let mut state = 0i32;
        // SAFETY: valid out pointer.
        let hr = unsafe { f(&mut state) };
        hr.is_ok()
            && matches!(
                state,
                QUNS_BUSY | QUNS_RUNNING_D3D_FULL_SCREEN | QUNS_PRESENTATION_MODE
            )
    }
}
