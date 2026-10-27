# Native iOS port (experimental)

This is an initial native iPhone/iPad host for SolidCraft, not a WebView or a
website packaged as an app. The Rust CAD engine and the existing desktop egui UI
are shared with the desktop app. The host uses eframe/winit and wgpu's Metal
backend, with a small Swift entry point and UIKit document picker.

The target is **arm64 iPhoneOS 16.4 or newer**, not the iOS Simulator. The product
is `SolidCraft`, with bundle identifier `labs.tangency.solidcraft.ios`. The IPA is
deliberately **unsigned**: there is no provisioning profile, `_CodeSignature`
directory or Mach-O code-signature command. It is not directly installable as a
normally signed device app; device deployment requires a separate signing and
provisioning step appropriate to the destination device.

## Current scope and limitations

- The initial port reuses the desktop layout. Landscape orientation and an iPad
  are recommended; this is not yet an iPhone-optimised UI.
- Basic touch input, pinch recognition and two-finger pan recognition are
  enabled. Gesture usability, rendering, lifecycle and performance still require
  physical-device testing.
- Open/import uses the native UIKit document picker **as a copy**. Selected
  files are copied into the sandbox's `Documents/Imports` folder before the Rust
  engine opens them. They are not edited at the provider's original location.
- Save/export writes into the app's `Documents` folder, exposed in the Files app
  by `UIFileSharingEnabled` and `LSSupportsOpeningDocumentsInPlace`. This is not a
  native destination-picker/export-sheet implementation. Opening-in-place is a
  bundle capability, not a claim that imports modify original provider files.
- Save As/export never silently overwrites a previous destination: a collision
  gets a numbered filename. Ordinary Save updates the design's established path.
- All open designs are checkpointed synchronously when iOS resigns active, with
  their recovery locks retained for resume. Preferences also have a synchronous
  atomic copy because eframe's ordinary settings flush uses a worker thread.
- Desktop external CLI agents and slicers, desktop update/system integrations,
  and image clipboard operations are unsupported.
- **No physical-device execution has been tested.** A successful cross-build and
  static IPA validation do not demonstrate that the app launches, renders or
  imports/exports correctly on an iPhone or iPad.

## Build on the configured Linux VM

Run from the repository root:

```sh
rustup target add aarch64-apple-ios
bash packaging/ios/build.sh
```

The build-time icon step uses `/usr/bin/python3` and Pillow (`python3-pil`).
It sizes the existing attributed SolidCraft app mark into ignored
`target/ios-icons/`; it does not introduce new artwork.

`build.sh` uses the shared Cargo workspace's `solidcraft-ios` static library and
then packages the Swift host with `xtool dev build --configuration release --ipa`.
It uses the repository `target/` directory and bounds compilation to two jobs.
The script relies on the tools and Swift SDK configuration already installed on
this VM; it is not a portable toolchain installer:

| Component | Configured version/location |
| --- | --- |
| Swift | 6.3.3, `/home/exedev/toolchains/swift-6.3.3-RELEASE-ubuntu24.04` |
| xtool | 1.24.0, installed from `/home/exedev/toolchains/xtool-1.24.0`, on `PATH` |
| Official Darwin cross-tools | 1.1.0, `/home/exedev/toolchains/darwin-tools` |
| Theos iPhoneOS SDK directory | `/home/exedev/toolchains/ios-sdk/iPhoneOS16.5.sdk` |
| SDK's internal identity | `iphoneos16.4` / iOS 16.4 in `SDKSettings.json` |
| Swift SDK target triple | `arm64-apple-ios`, registered in `darwin.artifactbundle` |
| Deployment minimum / packaged identity | iPhoneOS 16.4 |

The existing
`/home/exedev/wgpu-in-app/Apple/xtool/configure-sdk.py` configured the Swift SDK
artifact bundle in `~/.swiftpm/swift-sdks/darwin.artifactbundle` and Swift resources
in `/home/exedev/toolchains/ios-resources-6.3`. It supplies the Theos SDK's missing
Dispatch/os API notes and configures the Darwin linker and librarian. Recreate
that setup before building on another machine; simply setting `SDKROOT` is not
sufficient for the Swift cross-build. Despite its directory name, this SDK
identifies itself internally as **16.4**, not 16.5. The app's deployment target
and packaged minimum are also **16.4**.

The cross-linker is `ld64.lld`, with ad-hoc signing disabled. lld warns that iOS
support is experimental; this warning is not evidence of device compatibility.
The package includes `LICENSE-MIT`, `LICENSE-APACHE` and
`THIRD-PARTY-LICENSES.txt`. After xtool packaging, the build deflates the ZIP
entries without changing bundle bytes or executable permissions, then runs the
static verifier. The output is `packaging/ios/xtool/SolidCraft.ipa`.

## Static verification and regression tests

Locate the generated IPA under `packaging/ios/` and pass its actual path:

```sh
find packaging/ios -name '*.ipa' -print
python3 packaging/ios/verify_ipa.py /path/to/SolidCraft.ipa
python3 -m unittest discover -s packaging/tests -p 'test_ios.py' -v
CARGO_TARGET_DIR=target/path-tests cargo test --locked -p solidcraft-ios --lib
```

The verifier uses only Python's standard library. It checks ZIP CRC integrity,
bundle layout and identifier, deployment/platform metadata, both Files-sharing
flags, nonempty bundled licence files, a thin arm64 Mach-O executable, iPhoneOS
`LC_BUILD_VERSION` with minimum 16.4, and the absence of signing material.
Every dylib load dependency must use `/System/Library/`, `/usr/lib/` or `@rpath/`;
macOS `/Versions/` frameworks and AppKit are rejected. UIKit and Metal must be
linked. Malformed load commands and duplicate/unsafe ZIP paths are rejected.
Success prints the package SHA-256 and explicitly states that verification is
static only. The reusable `verify_ipa(path)` function returns package metadata
and raises `VerificationError` on failure.

The unittest suite creates tiny synthetic IPA/Mach-O fixtures to test acceptance
and rejection without an Apple SDK, signing identity or physical device. These
fixtures are intentionally not runnable applications.
The host-side Rust tests exercise sandbox-only save destinations, preservation
of existing designs and bounded collision handling without linking UIKit.
`recovery_ui` also contains a suspend-flush regression test for background tabs.

Repository policy requires **`cargo xtask ci` before every commit**. Run the
packaging tests in addition to that gate, and record actual cross-build,
verification and CI outcomes separately; none is implied by these instructions.

### Checks performed on this VM

- Native cross-build and unsigned arm64/iPhoneOS 16.4 package verification:
  **passed**. Physical-device execution remains untested.
- Host-side save-path tests: **3 passed**.
- Synthetic IPA verifier regression tests: **17 passed**.
- `cargo fmt --all -- --check`, `shellcheck packaging/ios/build.sh` and
  `git diff --check`: passed.
- `cargo xtask ci`: formatting, assets, icons, licences, sorted lists, skills,
  layering and UI-container gates passed. The full-workspace clippy stage was
  stopped at four-minute and two-minute resource budgets while checking
  dependencies.
  **Full CI, the UI recovery regression, and wasm validation are not claimed to
  have passed.**
