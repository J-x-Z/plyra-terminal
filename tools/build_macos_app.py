#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""Build, sign and optionally notarize the Apple Silicon application bundle."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import plistlib
import re
import shutil
import subprocess
import tempfile


def executable_info(path):
    path = path.resolve(strict=True)
    if not path.is_file() or not os.access(path, os.X_OK):
        raise ValueError(f"runtime component must be an executable file: {path}")
    architectures = subprocess.run(["/usr/bin/lipo", "-archs", str(path)],
                                   check=True, capture_output=True, text=True).stdout.split()
    if architectures != ["arm64"]:
        raise ValueError(f"runtime component must be native arm64: {path}")
    load_commands = subprocess.run(["/usr/bin/otool", "-l", str(path)],
                                   check=True, capture_output=True, text=True).stdout
    minimum = re.search(r"\bminos\s+([0-9.]+)", load_commands)
    if minimum is None:
        minimum = re.search(r"LC_VERSION_MIN_MACOSX\s+cmdsize\s+\d+\s+version\s+([0-9.]+)", load_commands)
    if minimum is None:
        raise ValueError(f"runtime component has no macOS deployment version: {path}")
    return {"source": str(path), "source_sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
            "minimum_macos_version": minimum.group(1)}


def version_key(version):
    parts = tuple(int(part) for part in version.split("."))
    return parts + (0,) * (3 - len(parts))


def source_snapshot(root):
    paths = [root / name for name in (
        "Cargo.toml", "Cargo.lock", ".cargo/config.toml", "tools/build_macos_app.py",
        "tools/mobile/terminal/vendor/PROVENANCE.json",
        "assets/terminal/GLASS_THIRD_PARTY_NOTICES.txt",
        "assets/app-icon/GlassTerminal.icns", "assets/app-icon/glass-terminal.png",
    )]
    for directory, suffixes in (
        ("crates", {".rs", ".toml", ".wgsl", ".json", ".jsonl"}),
        ("vendor", {".rs", ".toml", ".json", ".md", ".txt", ".vert", ".frag"}),
        ("apps/desktop", {".rs", ".toml"}),
        ("tools/mobile/terminal", {".c", ".h", ".inc"}),
        ("assets/ui-fonts", {".ttf", ".txt", ".json"}),
        ("assets/terminal-fonts", {".ttf", ".txt", ".json"}),
    ):
        paths.extend(path for path in (root / directory).rglob("*") if path.suffix in suffixes)
    return {str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in sorted(set(paths)) if path.is_file()}


def main():
    root = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=root.parent / "plyra-terminal-builds/glass-g1/macos/Plyra Terminal.app")
    parser.add_argument("--release", action="store_true", help="Build the optimized application")
    parser.add_argument("--signing-identity", default=os.environ.get("GLASS_CODESIGN_IDENTITY", "-"), help="codesign identity or '-' for local ad-hoc signing")
    parser.add_argument("--notary-profile", default=os.environ.get("GLASS_NOTARY_PROFILE"), help="Existing notarytool keychain profile; submit to Apple and staple before replacing the output")
    parser.add_argument("--version", default="0.1.0", help="CFBundleShortVersionString")
    parser.add_argument("--build-number", default="1", help="CFBundleVersion")
    parser.add_argument("--aether-shell", type=Path, help="Bundle an existing native Aether Shell executable")
    parser.add_argument("--system-bridge", type=Path, help="Bundle the matching glass-system-bridge executable")
    arguments = parser.parse_args()
    if arguments.notary_profile and arguments.signing_identity == "-":
        parser.error("notarization requires a Developer ID signing identity")
    if not arguments.version or not arguments.build_number:
        parser.error("version and build number must be nonempty")
    if platform.system() != "Darwin" or platform.machine() != "arm64":
        parser.error("the current macOS target is Apple Silicon")
    if bool(arguments.aether_shell) != bool(arguments.system_bridge):
        parser.error("--aether-shell and --system-bridge must be supplied together")
    components = {}
    try:
        for name, path in (("aether-shell", arguments.aether_shell),
                           ("glass-system-bridge", arguments.system_bridge)):
            if path:
                components[name] = executable_info(path)
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        parser.error(str(error))
    destination = arguments.output.resolve()
    if destination.suffix != ".app":
        parser.error("output must end in .app")
    marker = destination / "Contents/Resources/glass-build.json"
    if destination.exists() and (not marker.is_file() or json.loads(marker.read_text()).get("source_root") != str(root)):
        parser.error("existing output is not a bundle produced from this repository")

    sources = source_snapshot(root)
    command = ["cargo", "build", "--locked", "-p", "glass-terminal-desktop", "-p", "privilege-broker", "--message-format=json-render-diagnostics"]
    if arguments.release:
        command.append("--release")
    build_environment = dict(os.environ)
    build_environment.setdefault("MACOSX_DEPLOYMENT_TARGET", "11.0")
    process = subprocess.Popen(command, cwd=root, env=build_environment, stdout=subprocess.PIPE, text=True)
    executable = None
    helper = None
    broker = None
    for line in process.stdout:
        message = json.loads(line)
        if message.get("reason") == "compiler-artifact" and message.get("target", {}).get("name") == "glass-terminal":
            executable = message.get("executable")
        if message.get("reason") == "compiler-artifact" and message.get("target", {}).get("name") == "plyra-privilege-broker":
            broker = message.get("executable")
        if message.get("reason") == "build-script-executed" and "session-manager" in message.get("package_id", ""):
            helper = dict(message.get("env", [])).get("PLYRA_BUILT_HELPER")
    if process.wait() != 0 or not executable or not helper or not broker:
        raise SystemExit("build failed or did not return the application, PTY helper and broker")
    if source_snapshot(root) != sources:
        raise SystemExit("build inputs changed during compilation; rebuild the stable source state")
    compiled_components = {
        name: executable_info(Path(path))
        for name, path in (("glass-terminal", executable),
                           ("plyra-terminal-helper", helper),
                           ("plyra-privilege-broker", broker))
    }

    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="glass-bundle-", dir=destination.parent) as temporary:
        bundle = Path(temporary) / destination.name
        contents = bundle / "Contents"
        binaries = contents / "MacOS"
        resources = contents / "Resources"
        binaries.mkdir(parents=True)
        resources.mkdir()
        shutil.copy2(executable, binaries / "glass-terminal")
        shutil.copy2(helper, binaries / "plyra-terminal-helper")
        shutil.copy2(broker, binaries / "plyra-privilege-broker")
        shutil.copy2(root / "assets/terminal/GLASS_THIRD_PARTY_NOTICES.txt", resources / "THIRD_PARTY_NOTICES.txt")
        shutil.copy2(root / "assets/app-icon/GlassTerminal.icns", resources / "GlassTerminal.icns")
        for name, component in components.items():
            source = Path(component["source"])
            if hashlib.sha256(source.read_bytes()).hexdigest() != component["source_sha256"]:
                raise SystemExit(f"runtime component changed during the build: {source}")
            shutil.copy2(source, binaries / name)
        if components:
            shell_root = root.parent / "aether-shell"
            licenses = resources / "aether-shell-licenses"
            licenses.mkdir()
            shutil.copy2(shell_root / "NOTICE", licenses / "NOTICE")
            for source in sorted((shell_root / "licenses").glob("*")):
                if source.is_file():
                    shutil.copy2(source, licenses / source.name)
        deployment_versions = ["11.0"]
        deployment_versions.extend(item["minimum_macos_version"] for item in compiled_components.values())
        deployment_versions.extend(item["minimum_macos_version"] for item in components.values())
        minimum_macos = max(deployment_versions, key=version_key)
        info = {
            "CFBundleIdentifier": "org.plyra.glass-terminal",
            "CFBundleName": "Plyra Terminal",
            "CFBundleDisplayName": "Plyra Terminal",
            "CFBundleExecutable": "glass-terminal",
            "CFBundleIconFile": "GlassTerminal.icns",
            "CFBundlePackageType": "APPL",
            "CFBundleShortVersionString": arguments.version,
            "CFBundleVersion": arguments.build_number,
            "NSHighResolutionCapable": True,
            "LSMinimumSystemVersion": minimum_macos,
            "NSPrincipalClass": "NSApplication",
        }
        (contents / "Info.plist").write_bytes(plistlib.dumps(info))
        sign = ["codesign", "--force", "--options", "runtime", "--sign", arguments.signing_identity]
        if arguments.signing_identity != "-":
            sign.append("--timestamp")
        for target in (binaries / "plyra-terminal-helper", binaries / "plyra-privilege-broker",
                       *(binaries / name for name in components)):
            subprocess.run([*sign, str(target)], check=True)
        for name, component in components.items():
            component["packaged_sha256"] = hashlib.sha256((binaries / name).read_bytes()).hexdigest()
        (resources / "glass-build.json").write_text(json.dumps({
            "source_root": str(root), "architecture": "arm64",
            "profile": "release" if arguments.release else "dev",
            "signing": "ad-hoc" if arguments.signing_identity == "-" else "identity",
            "signing_identity": arguments.signing_identity,
            "notarized": bool(arguments.notary_profile),
            "minimum_macos_version": minimum_macos,
            "runtime_components": components,
            "build_executables": compiled_components,
            "source_files_sha256": sources,
            "source_snapshot_sha256": hashlib.sha256(json.dumps(sources, sort_keys=True).encode()).hexdigest(),
        }, indent=2) + "\n")
        subprocess.run([*sign, str(bundle)], check=True)
        subprocess.run(["codesign", "--verify", "--strict", "--deep", str(bundle)], check=True)
        if arguments.notary_profile:
            archive = Path(temporary) / "notarization.zip"
            subprocess.run(["/usr/bin/ditto", "-c", "-k", "--keepParent", "--sequesterRsrc", str(bundle), str(archive)], check=True)
            submitted = subprocess.run(["xcrun", "notarytool", "submit", str(archive), "--keychain-profile", arguments.notary_profile, "--wait", "--output-format", "json"], text=True, capture_output=True, check=True)
            receipt = json.loads(submitted.stdout)
            if receipt.get("status") != "Accepted":
                raise SystemExit(f"notarization failed: {receipt.get('status')} · submission {receipt.get('id')}")
            subprocess.run(["xcrun", "stapler", "staple", str(bundle)], check=True)
            subprocess.run(["xcrun", "stapler", "validate", str(bundle)], check=True)
            subprocess.run(["spctl", "--assess", "--type", "execute", "--verbose=2", str(bundle)], check=True)
        previous = Path(temporary) / "previous.app"
        if destination.exists():
            destination.rename(previous)
        try:
            bundle.rename(destination)
        except OSError:
            if previous.exists():
                previous.rename(destination)
            raise
    print(destination)


if __name__ == "__main__":
    main()

# FVPROV1 11985 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiYzI2YjU0YjYzYjA4ZDlhOTM0OTNmNmJiOTY3M2I3ZGRmMWU5MTc4OTViMjIwZjgyZjE4ZGUwMDY0MDYxYTBkYSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiYzI2ZjBlNDY5ZjEwYmI4NDhhMDRjM2MzYWRhMzk3MDM2YmQ0OGY1MTM1YmExY2YyZmRhOTU4N2VhMWJiN2YyN2ZjNDM4NjhlY2FjZWY1NzA3YmY2YTIwNzcyODA0YjEwYjE4YmY0NzM2NWJmZWZhYjlmNjEyOGQxZDhlNDYxMDIifV19XQ== FVPROV1-END
