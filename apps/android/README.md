# Plyra Terminal for Android

This source-first beta builds a debug-signed APK. The app targets Android 14 (API 34) and has a minimum API level of 26. Device checks cover the ARM64 build, including Settings, bundled Arch and Aether Shell on a Galaxy Z Fold6.

## Build the APK

Use macOS with Python 3, a JDK such as JDK 11, Android SDK Platform 34, Build Tools 34.0.0, and NDK 25.1.8937393. Install the matching Rust Android target with rustup.

```sh
rustup target add --toolchain nightly aarch64-linux-android
python3 apps/android/build_apk.py \
  --sdk /path/to/android-sdk \
  --java-home /path/to/jdk-11 \
  --output-dir ../plyra-terminal-builds/android-beta
```

The script runs Java input and touch tests before building and signing `glass-terminal-g1-debug.apk`. It reuses the debug keystore for later builds from the same `--build-dir`. Keep that keystore if you plan to install updates over your first build.

- Use `--sdk`, `--ndk`, `--build-tools` and `--java-home` for local tools, and `--rust-toolchain` to select an installed toolchain.
- Use `--build-dir` for the Cargo cache and signing state, and `--output-dir` for APK outputs. `--skip-build` packages the native libraries already built for the selected target.
- `--input-tests-only` runs the Java checks without building an APK. `--target` also accepts `x86_64-linux-android`, `armv7-linux-androideabi` and `i686-linux-android`. Each target requires its Rust standard library. Device coverage for this beta is ARM64.

## Android, Arch and Aether shells

The standard APK starts Android Shell. To add Arch on ARM64, first build the PRoot runtime and prepare a bootstrap from the verified, signed Arch Linux ARM archive pinned in the bootstrap script.

```sh
python3 apps/android/build_arch_runtime.py --output-dir ../plyra-terminal-builds/arch-runtime
python3 apps/android/build_arch_bootstrap.py \
  --source /path/to/pinned-ArchLinuxARM-aarch64.tar.gz \
  --output-dir ../plyra-terminal-builds/arch-bootstrap
```

The runtime builder accepts `--ndk` and `--python`. Use `--offline` if the pinned source archives are already present in the runtime output's `sources` directory. Verify the upstream archive signature before running the bootstrap builder. The builder checks the source hash and records the derived archive hash in `bootstrap-manifest.json`. Include the runtime's corresponding source, patches, licenses, and relinking materials when redistributing it. [Arch Linux ARM downloads](https://archlinuxarm.org/about/downloads)

Pass the runtime directory with `--arch-runtime`. For offline first launch, also pass the derived archive with `--arch-bootstrap` and its recorded SHA-256 with `--arch-bootstrap-sha256`. The bundled bootstrap expands into app-private storage and starts Bash with glibc. Existing Arch data stays available across compatible updates. With the runtime alone, select **Commands → Arch Linux** to import a verified Arch Linux ARM Generic AArch64 archive through the system picker.

Before installing packages in Arch, initialize its keyring with `pacman-key --init` and `pacman-key --populate archlinuxarm`.

For Aether Shell, run `python3 tools/build_android.py` from the separate Aether Shell checkout. Pass the resulting `build/android-arm64` directory to the APK builder with `--aether-shell`. You can select it for new sessions or run `aether-shell` from a bundled Arch session.

Long-press the terminal and choose **Settings** in **Commands**. Use **Terminal → New sessions** to choose a shell, and **Appearance → Background opacity** to adjust the terminal backdrop. Background blur follows the device's compositor capabilities. From an Arch session, use `glass-android battery` for battery status or `glass-android settings` to open Settings.

Arch runs under the ordinary Android app account. PRoot emulates guest UID 0 and shares the app's permissions. Android host administration requires a supported root manager and explicit authorization. Host administrator operations, successful Arch import on an enforcing device, and runs on 16 KiB page-size devices remain unverified.
