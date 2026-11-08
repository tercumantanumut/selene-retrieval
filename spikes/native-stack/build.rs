// M0-B: delay-load the DirectML/D3D12/DXGI imports that pyke's static ORT (always built with the
// DirectML EP on Windows) adds, so the CPU-only path never maps them. Disable with SPIKE_NO_DELAYLOAD=1.
fn main() {
    println!("cargo:rerun-if-env-changed=SPIKE_NO_DELAYLOAD");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var_os("SPIKE_NO_DELAYLOAD").is_none()
    {
        for dll in ["DirectML.dll", "d3d12.dll", "dxgi.dll"] {
            println!("cargo:rustc-link-arg-bins=/DELAYLOAD:{dll}");
        }
        println!("cargo:rustc-link-arg-bins=delayimp.lib");
    }
}
