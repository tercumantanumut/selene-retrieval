//! Links the VC++ runtime statically for this exe (Amendment 3.16); see the helper for details.

include!("../common/build/static_crt.rs");
// M5-I: application manifest (longPathAware, supportedOS, asInvoker) for the shipped bins.
include!("../../installer/manifests/embed.rs");

fn main() {
    static_vc_runtime();
    embed_console_manifest(&["sr-bridge", "sr-launch"]);
}
