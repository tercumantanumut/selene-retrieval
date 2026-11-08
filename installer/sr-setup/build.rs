//! Static VC++ runtime (hybrid CRT, Amendment 4.1): the same shared helper as sr-bridge/sr-app.

include!("../../crates/common/build/static_crt.rs");
// M5-I: application manifest (longPathAware, supportedOS, asInvoker).
include!("../manifests/embed.rs");

fn main() {
    static_vc_runtime();
    embed_console_manifest(&["sr-setup"]);
}
