# Source to build with the locally downloaded MSVC 14.44 (VS 17.14) toolset; no admin needed.
# cl.exe and link.exe also need their message DLLs (bin/Hostx64/x64/1033/*ui.dll, package
# Microsoft.VC.14.44.17.14.Tools.HostX64.TargetX64.Res.base, en-US, sha256 603375e4... in
# vendor/msvc-dl/vsman.json); without them cl.exe stops with C1510 on its first message.
# Paths are relative to this file, so the checkout can live anywhere.
R="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
M=$R/vendor/msvc-dl/msvc/Contents/VC/Tools/MSVC/14.44.35207
MW=$(cygpath -m "$M")
K="C:/Program Files (x86)/Windows Kits/10"
SDK=10.0.22621.0
export PATH="$HOME/.cargo/bin:$M/bin/Hostx64/x64:$(cygpath -u "$K")/bin/$SDK/x64:$PATH"  # SDK bin: mt.exe/rc.exe (M5 manifests embed via link /MANIFEST:EMBED)
export VCINSTALLDIR="$(cygpath -w $R/vendor/msvc-dl/msvc/Contents/VC)\\"
export VCToolsInstallDir="$(cygpath -w $M)\\"
export LIB="$MW/lib/x64;$K/Lib/$SDK/ucrt/x64;$K/Lib/$SDK/um/x64"
export INCLUDE="$MW/include;$K/Include/$SDK/ucrt;$K/Include/$SDK/um;$K/Include/$SDK/shared"
export CARGO_TARGET_DIR="$(cygpath -w $R/target)"
