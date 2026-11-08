//! Static VC++ runtime (hybrid CRT, Amendment 4.1): msiexec loads this DLL on machines without the
//! VC++ redistributable. Same shared helper as sr-bridge / sr-setup.

include!("../../crates/common/build/static_crt.rs");

fn main() {
    static_vc_runtime();
}
