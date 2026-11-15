# Contributing to Selene Retrieval

Thank you for helping. This file explains how to build the project, run the checks and send a change.

Selene Retrieval runs on **Windows 10 and 11, x64**. A **macOS (Apple silicon) preview** builds and runs too; see [Building on macOS](#building-on-macos). Windows-only checks (`check-crt`, `check-delayload`, the installers) need Windows.

## What we care about

- **An average user can do it without documentation.** One installer, sensible defaults, a first-run wizard, one-click setup, plain-language errors with one next action.
- **Security is never weakened for simplicity.** Sandboxing, path checks, loopback-only networking and keyring secrets stay on and invisible.
- **Every change answers one question:** "Can an average user do this without documentation?"

The full text is in [docs/product-principles.md](docs/product-principles.md).

## Tools you need

1. **Git for Windows.** The scripts and CI use Git Bash. The checkout keeps LF line endings (`.gitattributes`).
2. **Visual Studio 2022 Build Tools 17.14 (MSVC 14.44)** with the workload `Microsoft.VisualStudio.Workload.VCTools` and a Windows 11 SDK component. Older MSVC toolsets fail to link the bundled ONNX Runtime; CI refuses anything below 14.44. One way to install them (we have not run this exact line):

   ```powershell
   winget install Microsoft.VisualStudio.2022.BuildTools --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --add Microsoft.VisualStudio.Component.VC.Tools.x86.x64 --add Microsoft.VisualStudio.Component.Windows11SDK.26100"
   ```

3. **Rust** through [rustup](https://rustup.rs). In the checkout, `rustup toolchain install` reads `rust-toolchain.toml` and installs the pinned toolchain with rustfmt and clippy (rustup 1.28 or newer).
4. **protoc 25.1** (the Protocol Buffers compiler), on `PATH` or through the `PROTOC` environment variable. LanceDB compiles `.proto` files at build time. CI pins 25.1; get `protoc-25.1-win64.zip` from the [protobuf releases](https://github.com/protocolbuffers/protobuf/releases/tag/v25.1).
5. **Node.js 22.13 or newer**, for the app UI in `ui/`. `ui/.nvmrc` names the version CI uses.
6. **.NET 6 or newer**, only to build the MSI (WiX runs as a .NET tool).
7. **cargo-deny 0.20.2**, for the license and advisory check: `cargo install cargo-deny --locked --version 0.20.2`.

Python is not needed to build or test. It is used only by the evaluations in `eval/` (see [eval/README.md](eval/README.md)).

The build also uses `curl.exe`, `tar.exe` and Windows PowerShell from `System32`. Windows 10 (1803 or newer) and Windows 11 include them.

## Build and test

Run these from the repository root, in Git Bash or PowerShell:

```text
rustup toolchain install
cargo xtask fetch-models      # the bundled model files for the model tests (sha256-checked)
cargo xtask fetch-models --all  # also the two larger tier models (about 1.3 GB) for a few tests;
                                # without them those tests skip unless SR_REQUIRE_MODELS=1
cargo xtask fetch-pdfium      # pdfium.dll into vendor-bin/pdfium-7881/ (sha256-checked)
cargo build                   # the engine and the bridge (not the desktop app)
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all --check
cargo deny --locked check
```

The document-extraction tests find `vendor-bin/pdfium-7881/pdfium.dll` by themselves. Set `SR_PDFIUM_DLL` to use another copy. Tests that need a DirectX 12 graphics card skip without one.

The UI checks, in `ui/`:

```text
npm ci
npm run lint
npm test
npm run build
```

### Build the installer

`cargo xtask package` embeds the built UI, so run `npm run build` in `ui/` first.

```text
cargo xtask package           # unsigned NSIS installer in target/package/
cargo xtask package --msi     # unsigned MSI in target/package/ (needs .NET 6 or newer)
```

Each run also writes `SHA256SUMS.txt` and `package-report.json` next to the installer (the MSI run: `SHA256SUMS-msi.txt` and `package-report-msi.json`). The installer is not code-signed, so Windows SmartScreen warns when you run it.

### Downloads during the build

If you are behind a firewall, these hosts must be reachable:

- `index.crates.io` and `static.crates.io` (Rust crates)
- `cdn.pyke.io` (ONNX Runtime, through the `ort` crate)
- `huggingface.co` and the `hf.co` download hosts it redirects to (model files)
- `github.com` and its release download hosts (pdfium, NSIS)
- `api.nuget.org` (DirectML, WiX)
- `download.visualstudio.microsoft.com` (the VC++ runtime files the installer ships)
- `go.microsoft.com` and `msedge.sf.dl.delivery.mp.microsoft.com` (WebView2 setup)
- `registry.npmjs.org` (UI packages)

Every file the `cargo xtask fetch-*` commands download is pinned and checked against its SHA-256 hash, except the WebView2 bootstrapper, which Microsoft updates in place; it is accepted only with a valid Microsoft signature.

### The Selene checkout

`cargo xtask sync-providers` and the evaluations read the [Selene](https://github.com/tercumantanumut/selene) sources from a checkout next to this repository (`../selene`). Pass `--selene <dir>` to use another one. Nothing else needs it.

### Optional: a local MSVC toolset

`spikes/extract-sandbox/msvc-env.sh` sets up the environment for an MSVC 14.44 toolset that was downloaded by hand into that folder. You don't need it if you installed the Build Tools above. It also pins Windows SDK `10.0.22621.0` and sets `CARGO_TARGET_DIR` to `spikes/extract-sandbox/target`, so override `CARGO_TARGET_DIR` after sourcing it if you want the normal `target/` folder.

## Building on macOS

Apple silicon, macOS 13 or newer (Amendment 18 in [docs/00-review-and-decisions.md](docs/00-review-and-decisions.md)). You need:

- Rust from [rustup](https://rustup.rs) (the pinned toolchain installs itself from `rust-toolchain.toml`), the Xcode Command Line Tools (`xcode-select --install`), Node.js 22 and `protoc` (`brew install protobuf`).
- Every Windows code path stays as it is, behind `cfg(windows)`; Mac code goes behind `cfg(unix)` or `cfg(target_os = "macos")`, never in place of Windows code.

Build the app bundle and the disk image:

```sh
(cd ui && npm ci && npm run build)
cargo xtask package-mac            # target/package/mac/Selene Retrieval.app + SeleneRetrieval-<v>-arm64.dmg
```

`package-mac` fetches the pinned pdfium and the bundled model, builds with the `dist` profile, puts programs in `Contents/MacOS`, pdfium in `Contents/Frameworks` and the model in `Contents/Resources`, signs every binary ad hoc (or with `SR_MAC_SIGN_IDENTITY`), verifies the signature and checks with `otool -L` that only system libraries are linked. `--no-build` repackages existing binaries; `--no-dmg` stops after the app.

Run the tests of the crates you changed (`cargo test -p <crate>`). The parser sandbox suites need the probe feature: `cargo test -p sr-extract --features test-probes --test supervisor --test hostile`.

## Before you open a pull request

Run the checks from [Build and test](#build-and-test): fmt, clippy, test, `cargo deny`, and the UI checks if you changed `ui/`. CI also runs these:

- `cargo xtask check-bridge-deps` (the small bridge must not link the heavy native stack)
- `cargo xtask sync-providers --check --selene <dir>` (the provider list matches Selene)
- fmt, clippy and test for `installer/sr-setup` (use `--manifest-path installer/sr-setup/Cargo.toml`) if you changed the installer

## Pull requests

- Keep each pull request small and focused on one change.
- Describe the change as a user sees it: what is different on screen, in the AI app or in the installer.
- Say how you tested it.
- Answer the standing question: "Can an average user do this without documentation?" If the answer is no, explain why and how the risk is handled.
- Don't weaken a security default to make something simpler.

AI-assisted contributions are welcome when you have read and tested the change yourself. Agent guidance for this repository is in [CLAUDE.md](CLAUDE.md).

## Security issues

Never report a vulnerability in a public issue or pull request. Follow [SECURITY.md](SECURITY.md).

## License

Selene Retrieval is licensed under the [Apache License 2.0](LICENSE). Unless you say otherwise, any contribution you submit is licensed under the same terms (section 5 of the license).

Please follow our [Code of Conduct](CODE_OF_CONDUCT.md).
