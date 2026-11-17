#!/usr/bin/env python3
"""Build a G1 debug APK using an existing Android SDK and NDK."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import zipfile

TARGETS = {
    "aarch64-linux-android": ("aarch64-linux-android", "arm64-v8a"),
    "x86_64-linux-android": ("x86_64-linux-android", "x86_64"),
    "armv7-linux-androideabi": ("armv7a-linux-androideabi", "armeabi-v7a"),
    "i686-linux-android": ("i686-linux-android", "x86"),
}
ARCH_BOOTSTRAP_ASSET = "assets/arch-bootstrap.tar.gz"
STREAM_BLOCK_BYTES = 1024 * 1024
# ZipFile reserves 5% when choosing ZIP64, even for a stored entry.
ARCH_BOOTSTRAP_MAX_BYTES = zipfile.ZIP64_LIMIT * 100 // 105


def run(command, environment=None):
    print("+", " ".join(str(part) for part in command), flush=True)
    subprocess.run(command, env=environment, check=True)


def arch_runtime_files(directory):
    manifest = directory / "runtime-manifest.json"
    runtime = json.loads(manifest.read_text())
    if (runtime.get("schema"), runtime.get("architecture")) != (
        "glass.android.arch-runtime.v1", "arm64-v8a"
    ):
        raise ValueError("Invalid arm64 Arch runtime manifest")
    binaries = {}
    for name in ("libglass_proot.so", "libglass_proot_loader.so"):
        path = directory / "jniLibs/arm64-v8a" / name
        record = runtime["files"][name]
        if path.stat().st_size != record["bytes"] or (
            hashlib.sha256(path.read_bytes()).hexdigest() != record["sha256"]
        ):
            raise ValueError(f"Arch runtime binary hash mismatch: {path}")
        binaries[f"lib/arm64-v8a/{name}"] = path
    notices = [directory / "NOTICE.txt", manifest]
    notices.extend(sorted((directory / "licenses").glob("*")))
    if len(notices) == 2 or not all(path.is_file() for path in notices):
        raise ValueError("Arch runtime notices and licenses are required")
    for path in notices:
        binaries[f"assets/arch-runtime/{path.relative_to(directory).as_posix()}"] = path
    return binaries


def aether_shell_files(directory):
    manifest = (directory / "manifest.json").read_bytes()
    release = json.loads(manifest)
    if (release.get("schema"), release.get("architecture"), release.get("target")) != (
        "aether.shell.android-package.v1", "arm64-v8a", "aarch64-linux-android"
    ):
        raise ValueError("Invalid Android arm64 Aether Shell manifest")
    files = release["files"]
    required = {"aether-shell", "NOTICE", "licenses/AetherLang-Apache-2.0.txt"}
    if not isinstance(files, dict) or not required <= files.keys():
        raise ValueError("Aether Shell executable, notice, and AetherLang license are required")
    assets = {"assets/aether-shell/manifest.json": manifest}
    for name, expected in files.items():
        if name not in ("aether-shell", "NOTICE") and not re.fullmatch(
            r"licenses/[A-Za-z0-9_-][A-Za-z0-9_.-]*", name
        ):
            raise ValueError(f"Invalid Aether Shell package file: {name}")
        content = (directory / name).read_bytes()
        if hashlib.sha256(content).hexdigest() != expected:
            raise ValueError(f"Aether Shell package hash mismatch: {name}")
        if name == "aether-shell":
            if (
                len(content) < 64 or content[:6] != b"\x7fELF\x02\x01"
                or content[16:18] not in (b"\x02\x00", b"\x03\x00")
                or content[18:20] != b"\xb7\x00" or not any(content[24:32])
            ):
                raise ValueError("Aether Shell must be an ARM64 ELF executable")
            assets["lib/arm64-v8a/libaether_shell.so"] = content
        else:
            assets[f"assets/aether-shell/{name}"] = content
    return assets


def verified_arch_bootstrap(path, expected_sha256):
    """Verify a pinned gzip without loading its potentially large payload into RAM."""
    if not re.fullmatch(r"[0-9a-fA-F]{64}", expected_sha256):
        raise ValueError("--arch-bootstrap-sha256 must contain exactly 64 hexadecimal characters")
    expected_sha256 = expected_sha256.lower()
    digest = hashlib.sha256()
    count = 0
    if not path.is_file():
        raise ValueError("Arch bootstrap must be a regular file")
    with path.open("rb") as source:
        metadata = os.fstat(source.fileno())
        if not stat.S_ISREG(metadata.st_mode):
            raise ValueError("Arch bootstrap must be a regular file")
        # Android APKs must not acquire a ZIP64 entry for this asset.
        if not 18 <= metadata.st_size <= ARCH_BOOTSTRAP_MAX_BYTES:
            raise ValueError("Arch bootstrap gzip length must be 18 bytes to the APK ZIP32 limit")
        while block := source.read(STREAM_BLOCK_BYTES):
            if count == 0 and block[:3] != b"\x1f\x8b\x08":
                raise ValueError("Arch bootstrap must have a gzip compression header")
            count += len(block)
            if count > metadata.st_size:
                raise ValueError("Arch bootstrap length changed during verification")
            digest.update(block)
    if count != metadata.st_size:
        raise ValueError("Arch bootstrap length changed during verification")
    if digest.hexdigest() != expected_sha256:
        raise ValueError(f"Arch bootstrap SHA256 mismatch: {path}")
    return {"path": path, "bytes": count, "sha256": expected_sha256}


def package_arch_bootstrap(archive, bootstrap):
    """Store the verified gzip verbatim so Android AssetManager.openFd can map it."""
    if ARCH_BOOTSTRAP_ASSET in archive.namelist():
        raise ValueError("APK already contains an Arch bootstrap asset")
    info = zipfile.ZipInfo(ARCH_BOOTSTRAP_ASSET)
    info.compress_type = zipfile.ZIP_STORED
    info.file_size = bootstrap["bytes"]
    info.external_attr = 0o100644 << 16
    digest = hashlib.sha256()
    count = 0
    with bootstrap["path"].open("rb") as source:
        metadata = os.fstat(source.fileno())
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_size != bootstrap["bytes"]:
            raise ValueError("Arch bootstrap length changed before packaging")
        with archive.open(info, "w") as destination:
            while block := source.read(STREAM_BLOCK_BYTES):
                count += len(block)
                if count > bootstrap["bytes"]:
                    raise ValueError("Arch bootstrap length changed during packaging")
                digest.update(block)
                destination.write(block)
    if count != bootstrap["bytes"] or digest.hexdigest() != bootstrap["sha256"]:
        raise ValueError("Arch bootstrap changed after verification; refusing to sign this APK")


def main():
    repository = Path(__file__).resolve().parents[2]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--build-dir",
        type=Path,
        default=repository.parent / "plyra-terminal-builds" / "glass-g1",
    )
    parser.add_argument(
        "--sdk",
        type=Path,
        default=Path(os.environ.get("ANDROID_HOME", Path.home() / "Library/Android/sdk")),
    )
    parser.add_argument("--output-dir", type=Path, help="Keep APK outputs separate while reusing the build cache and debug signing key")
    parser.add_argument("--ndk", type=Path)
    parser.add_argument("--build-tools", default="34.0.0")
    parser.add_argument("--rust-toolchain", default="nightly")
    parser.add_argument("--target", choices=TARGETS, default="aarch64-linux-android")
    parser.add_argument("--java-home", type=Path)
    parser.add_argument("--skip-build", action="store_true")
    parser.add_argument("--arch-runtime", type=Path,
                        help="Include the verified arm64 output of build_arch_runtime.py")
    parser.add_argument("--aether-shell", type=Path,
                        help="Include a verified Aether Shell Android ARM64 package")
    parser.add_argument("--arch-bootstrap", type=Path,
                        help="Embed a verified gzip rootfs at assets/arch-bootstrap.tar.gz (requires --arch-runtime and --arch-bootstrap-sha256)")
    parser.add_argument("--arch-bootstrap-sha256",
                        help="Required SHA256 pin for --arch-bootstrap; both options must be supplied together")
    parser.add_argument("--input-tests-only", action="store_true",
                        help="Run bounded pure Java IME/touch state checks without Cargo, APK or devices")
    arguments = parser.parse_args()
    if (arguments.arch_bootstrap is None) != (arguments.arch_bootstrap_sha256 is None):
        parser.error("--arch-bootstrap and --arch-bootstrap-sha256 must be supplied together")
    if arguments.arch_bootstrap is not None and arguments.arch_runtime is None:
        parser.error("--arch-bootstrap requires --arch-runtime on aarch64-linux-android")
    arch_files = {}
    if arguments.arch_runtime:
        if arguments.target != "aarch64-linux-android":
            parser.error("--arch-runtime currently requires aarch64-linux-android")
        try:
            arch_files = arch_runtime_files(arguments.arch_runtime.resolve())
        except (KeyError, ValueError, OSError) as error:
            parser.error(str(error))
    aether_files = {}
    if arguments.aether_shell is not None:
        if arguments.target != "aarch64-linux-android":
            parser.error("--aether-shell currently requires aarch64-linux-android")
        try:
            aether_files = aether_shell_files(arguments.aether_shell.resolve())
        except (KeyError, TypeError, ValueError, OSError) as error:
            parser.error(str(error))
    arch_bootstrap = None
    if arguments.arch_bootstrap is not None:
        try:
            arch_bootstrap = verified_arch_bootstrap(
                arguments.arch_bootstrap.resolve(), arguments.arch_bootstrap_sha256
            )
        except (ValueError, OSError) as error:
            parser.error(str(error))

    build_dir = arguments.build_dir.resolve()
    sdk = arguments.sdk.resolve()
    ndk = (arguments.ndk or sdk / "ndk/25.1.8937393").resolve()
    tools = sdk / "build-tools" / arguments.build_tools
    toolchain = ndk / "toolchains/llvm/prebuilt/darwin-x86_64/bin"
    compiler_target, abi = TARGETS[arguments.target]
    compiler = toolchain / f"{compiler_target}26-clang"
    java_home = arguments.java_home
    if java_home is None:
        jdk11 = Path("/Library/Java/JavaVirtualMachines/amazon-corretto-11.jdk/Contents/Home")
        java_home = jdk11 if jdk11.is_dir() else Path(
            os.environ.get("JAVA_HOME", "/Applications/Android Studio.app/Contents/jbr/Contents/Home")
        )
    required_tools = (java_home / "bin/java", java_home / "bin/javac")
    if not arguments.input_tests_only:
        required_tools += (
        sdk / "platforms/android-34/android.jar",
        tools / "aapt2",
        tools / "zipalign",
        tools / "apksigner",
        tools / "d8",
        compiler,
        java_home / "bin/keytool",
        )
    for required in required_tools:
        if not required.is_file():
            parser.error(f"required local tool is missing: {required}")

    environment = os.environ.copy()
    environment.update(
        {
            "CARGO_HOME": str(build_dir / "cargo-home"),
            "CARGO_TARGET_DIR": str(build_dir / "target"),
            f"CARGO_TARGET_{arguments.target.upper().replace('-', '_')}_LINKER": str(compiler),
            f"CC_{arguments.target.replace('-', '_')}": str(compiler),
            f"AR_{arguments.target.replace('-', '_')}": str(toolchain / "llvm-ar"),
            "JAVA_HOME": str(java_home),
            "PATH": str(java_home / "bin") + os.pathsep + environment.get("PATH", ""),
        }
    )
    apk_dir = arguments.output_dir.resolve() if arguments.output_dir else build_dir / "android"
    apk_dir.mkdir(parents=True, exist_ok=True)
    test_classes = apk_dir / "input-state-tests"
    test_classes.mkdir(exist_ok=True)
    source_dir = repository / "apps/android/java/org/plyra/glass/terminal"
    run([
        java_home / "bin/javac", "--release", "8", "-d", test_classes,
        source_dir / "TerminalInputState.java",
        source_dir / "TerminalTouchState.java",
        repository / "apps/android/tests/TerminalInputStateTest.java",
        repository / "apps/android/tests/TerminalTouchStateTest.java",
    ], environment)
    run([
        java_home / "bin/java", "-cp", test_classes,
        "org.plyra.glass.terminal.TerminalInputStateTest",
    ], environment)
    run([
        java_home / "bin/java", "-cp", test_classes,
        "org.plyra.glass.terminal.TerminalTouchStateTest",
    ], environment)
    if arguments.input_tests_only:
        return

    if arch_files:
        api_client = apk_dir / "libglass_android_api.so"
        run([
            compiler, "-std=c11", "-O2", "-static", "-fno-emulated-tls",
            "-Wl,-z,max-page-size=16384", "-Wl,-z,noexecstack",
            repository / "apps/android/native/glass_android_api.c",
            "-o", api_client,
        ], environment)
        arch_files[f"lib/{abi}/libglass_android_api.so"] = api_client

    if not arguments.skip_build:
        rustup = Path.home() / ".cargo/bin/rustup"
        if not rustup.is_file():
            rustup_path = shutil.which("rustup")
            if rustup_path is None:
                parser.error("rustup is required to select the Android Rust toolchain")
            rustup = Path(rustup_path)
        environment["RUSTC"] = subprocess.check_output(
            [rustup, "which", "--toolchain", arguments.rust_toolchain, "rustc"],
            text=True,
        ).strip()
        packages = ["--package", "glass-terminal-android"]
        if aether_files:
            packages.extend(["--package", "system-api"])
        run(
            [
                rustup,
                "run",
                arguments.rust_toolchain,
                "cargo",
                "build",
                "--locked",
                "--manifest-path",
                repository / "Cargo.toml",
                *packages,
                "--target",
                arguments.target,
                "--config",
                f'target.{arguments.target}.rustflags=["-C", "link-arg=-Wl,-z,max-page-size=16384"]',
            ],
            environment,
        )

    library = build_dir / "target" / arguments.target / "debug/libglass_terminal_android.so"
    root_agent = build_dir / "target" / arguments.target / "debug/glass-android-root-broker"
    system_bridge = build_dir / "target" / arguments.target / "debug/glass-system-bridge"
    if not library.is_file():
        parser.error(f"Android library has not been built: {library}")
    if not root_agent.is_file():
        parser.error(f"Android root agent has not been built: {root_agent}")
    if aether_files and not system_bridge.is_file():
        parser.error(f"Android System API bridge has not been built: {system_bridge}")

    suffix = "" if abi == "arm64-v8a" else f"-{abi}"
    classes = apk_dir / "classes"
    classes.mkdir(exist_ok=True)
    run([
        java_home / "bin/javac", "--release", "8",
        "-classpath", sdk / "platforms/android-34/android.jar", "-d", classes,
        *sorted(source_dir.glob("*.java")),
    ], environment)
    dex_dir = apk_dir / "dex"
    dex_dir.mkdir(exist_ok=True)
    run([
        tools / "d8", "--min-api", "26", "--lib", sdk / "platforms/android-34/android.jar",
        "--output", dex_dir, *sorted(classes.rglob("*.class")),
    ], environment)
    unsigned_apk = apk_dir / f"glass-terminal-g1{suffix}-unsigned.apk"
    aligned_apk = apk_dir / f"glass-terminal-g1{suffix}-aligned.apk"
    final_apk = apk_dir / f"glass-terminal-g1{suffix}-debug.apk"
    resources = apk_dir / "compiled-resources.zip"
    run([
        tools / "aapt2", "compile", "--dir", repository / "apps/android/res",
        "-o", resources,
    ], environment)
    run(
        [
            tools / "aapt2",
            "link",
            "-o",
            unsigned_apk,
            "--manifest",
            repository / "apps/android/AndroidManifest.xml",
            "-I",
            sdk / "platforms/android-34/android.jar",
            resources,
        ],
        environment,
    )
    with zipfile.ZipFile(unsigned_apk, "a", compression=zipfile.ZIP_DEFLATED) as archive:
        archive.write(library, f"lib/{abi}/libglass_terminal_android.so")
        archive.write(root_agent, f"lib/{abi}/libglass_root_agent.so")
        for name, path in arch_files.items():
            archive.write(path, name)
        for name, content in aether_files.items():
            archive.writestr(name, content)
        if aether_files:
            archive.write(system_bridge, f"lib/{abi}/libglass_system_bridge.so")
        if arch_bootstrap is not None:
            package_arch_bootstrap(archive, arch_bootstrap)
        archive.write(dex_dir / "classes.dex", "classes.dex")
        archive.write(
            repository / "assets/terminal/GLASS_THIRD_PARTY_NOTICES.txt",
            "assets/THIRD_PARTY_NOTICES.txt",
        )
    run([tools / "zipalign", "-p", "-f", "4", unsigned_apk, aligned_apk], environment)

    keystore = build_dir / "android/g1-debug.keystore"
    keystore.parent.mkdir(parents=True, exist_ok=True)
    if not keystore.exists():
        run(
            [
                java_home / "bin/keytool",
                "-genkeypair",
                "-noprompt",
                "-keystore",
                keystore,
                "-storepass",
                "android",
                "-keypass",
                "android",
                "-alias",
                "androiddebugkey",
                "-keyalg",
                "RSA",
                "-keysize",
                "2048",
                "-validity",
                "3650",
                "-dname",
                "CN=Plyra Terminal Debug,O=Plyra,C=US",
            ],
            environment,
        )
    run(
        [
            tools / "apksigner",
            "sign",
            "--ks",
            keystore,
            "--ks-pass",
            "pass:android",
            "--key-pass",
            "pass:android",
            "--out",
            final_apk,
            aligned_apk,
        ],
        environment,
    )
    run([tools / "apksigner", "verify", "--verbose", final_apk], environment)
    run([tools / "aapt2", "dump", "badging", final_apk], environment)
    print(final_apk)


if __name__ == "__main__":
    main()

# FVPROV1 18404 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiN2YwNjcyNzlmNmNhYzhlNWFlMzVkNDE2ZTc1ZTE1NDdlNWNmM2JiYzgxNjE3MzhjNDdiMDQxMTAxNzY5Yzc5OCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiYjU4MDFkMDM2MTE0MDc3YjVmMTg4MmU4ZTc0YmY3ZGM2MTYyM2Q2YzlmMmIyODhmYTgxYzJkNDI0MzAxOTdmZDM4NzI3OThjMTU4ZmM4Yjc3NmU1NTFlYzliZDliYWNiYzQ0MTFkZWE4OTVmN2MwNzA5YmU2OTQ0MmEzNDhmMDAifV19XQ== FVPROV1-END
