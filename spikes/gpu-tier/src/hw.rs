//! Raw Win32 hardware detection (no sysinfo). Needs no admin. Measured ~300 ms on the dev box, dominated by
//! D3D12CreateDevice per adapter; run it off the UI thread (once per start, cached).

use crate::tier::{GpuInfo, HwInfo, round_ram};
use std::path::Path;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D12::{
    D3D12_FEATURE_ARCHITECTURE1, D3D12_FEATURE_DATA_ARCHITECTURE1, D3D12CreateDevice, ID3D12Device,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO,
    IDXGIAdapter1, IDXGIAdapter3, IDXGIFactory1,
};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, GlobalMemoryStatusEx, MEMORYSTATUSEX, RelationProcessorCore,
    SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::core::{HSTRING, Interface};

pub fn total_ram() -> u64 {
    let mut m = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    unsafe { GlobalMemoryStatusEx(&mut m) }.expect("GlobalMemoryStatusEx");
    m.ullTotalPhys
}

/// (physical cores, logical processors) via GetLogicalProcessorInformationEx(RelationProcessorCore).
pub fn cores() -> (u32, u32) {
    let mut len = 0u32;
    unsafe {
        let _ = GetLogicalProcessorInformationEx(RelationProcessorCore, None, &mut len);
    }
    let mut buf = vec![0u8; len as usize];
    unsafe {
        GetLogicalProcessorInformationEx(
            RelationProcessorCore,
            Some(buf.as_mut_ptr() as *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX),
            &mut len,
        )
        .expect("GetLogicalProcessorInformationEx");
    }
    let (mut phys, mut logical) = (0u32, 0u32);
    let mut off = 0usize;
    while off < len as usize {
        let rec = unsafe { &*(buf.as_ptr().add(off) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX) };
        if rec.Relationship == RelationProcessorCore {
            phys += 1;
            let p = unsafe { &rec.Anonymous.Processor };
            for i in 0..p.GroupCount as usize {
                let gm = unsafe { *p.GroupMask.as_ptr().add(i) };
                logical += gm.Mask.count_ones();
            }
        }
        off += rec.Size as usize;
    }
    (phys, logical)
}

pub fn free_disk(path: &Path) -> u64 {
    let mut free = 0u64;
    let p = HSTRING::from(path.as_os_str());
    unsafe { GetDiskFreeSpaceExW(&p, Some(&mut free), None, None) }.expect("GetDiskFreeSpaceExW");
    free
}

pub fn gpus() -> Vec<GpuInfo> {
    let mut out = Vec::new();
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else { return out };
    let mut i = 0u32;
    while let Ok(a) = unsafe { factory.EnumAdapters1(i) } {
        if let Ok(d) = unsafe { a.GetDesc1() } {
            let name = String::from_utf16_lossy(&d.Description).trim_end_matches('\0').to_string();
            let (fl11, uma) = d3d12_probe(&a);
            out.push(GpuInfo {
                name,
                vendor_id: d.VendorId,
                device_id: d.DeviceId,
                dedicated_vram: d.DedicatedVideoMemory as u64,
                shared_mem: d.SharedSystemMemory as u64,
                is_software: (d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0,
                d3d12_fl11: fl11,
                uma,
                dxgi_index: i,
            });
        }
        i += 1;
    }
    out
}

/// (D3D12 feature level 11.0 device creatable, adapter is UMA).
/// UMA = integrated GPU / APU sharing system RAM (D3D12_FEATURE_DATA_ARCHITECTURE1.UMA). This is the reliable
/// integrated-vs-dedicated signal: AMD APUs can report a multi-GiB BIOS carve-out as DedicatedVideoMemory.
fn d3d12_probe(a: &IDXGIAdapter1) -> (bool, bool) {
    let mut dev: Option<ID3D12Device> = None;
    if unsafe { D3D12CreateDevice(a, D3D_FEATURE_LEVEL_11_0, &mut dev) }.is_err() {
        return (false, false);
    }
    let Some(dev) = dev else { return (false, false) };
    let mut arch = D3D12_FEATURE_DATA_ARCHITECTURE1 { NodeIndex: 0, ..Default::default() };
    let uma = unsafe {
        dev.CheckFeatureSupport(
            D3D12_FEATURE_ARCHITECTURE1,
            &mut arch as *mut _ as *mut core::ffi::c_void,
            size_of::<D3D12_FEATURE_DATA_ARCHITECTURE1>() as u32,
        )
    }
    .is_ok()
        && arch.UMA.as_bool();
    (true, uma)
}

pub fn detect(data_dir: &Path) -> HwInfo {
    let (physical_cores, logical_cores) = cores();
    HwInfo {
        total_ram: round_ram(total_ram()),
        physical_cores,
        logical_cores,
        avx2: std::arch::is_x86_feature_detected!("avx2"),
        avx512_vnni: std::arch::is_x86_feature_detected!("avx512vnni"),
        free_disk: free_disk(data_dir),
        gpus: gpus(),
    }
}

/// Process RSS now and peak (working set).
pub fn rss() -> (u64, u64) {
    let mut c = PROCESS_MEMORY_COUNTERS { cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
    unsafe {
        let _ = GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb);
    }
    (c.WorkingSetSize as u64, c.PeakWorkingSetSize as u64)
}

/// This process's local (dedicated) VRAM usage on adapter `idx`, via IDXGIAdapter3::QueryVideoMemoryInfo.
pub fn vram_usage(idx: u32) -> Option<u64> {
    let factory = unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }.ok()?;
    let a = unsafe { factory.EnumAdapters1(idx) }.ok()?;
    let a3: IDXGIAdapter3 = a.cast().ok()?;
    let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
    unsafe { a3.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info) }.ok()?;
    Some(info.CurrentUsage)
}
