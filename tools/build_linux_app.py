#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""Build or package the native Linux application and its explicit installers."""
import argparse
import hashlib
import json
from pathlib import Path
import platform
import shutil
import struct
import subprocess
import tempfile

from build_macos_app import source_snapshot as common_source_snapshot


TARGETS = {
    "x86_64-unknown-linux-gnu": 62,
    "aarch64-unknown-linux-gnu": 183,
    "riscv64gc-unknown-linux-gnu": 243,
    "powerpc64le-unknown-linux-gnu": 21,
}
NATIVE_TARGETS = {
    "x86_64": "x86_64-unknown-linux-gnu",
    "aarch64": "aarch64-unknown-linux-gnu",
    "riscv64": "riscv64gc-unknown-linux-gnu",
    "ppc64le": "powerpc64le-unknown-linux-gnu",
}


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def source_snapshot(root):
    sources = common_source_snapshot(root)
    paths = [Path(__file__)]
    paths.extend(path for path in (root / "packaging/linux").rglob("*")
                 if path.is_file() and "__pycache__" not in path.parts)
    for path in paths:
        sources[str(path.relative_to(root))] = sha256(path)
    return dict(sorted(sources.items()))


def verify_elf(path, machine):
    if not path.is_file() or path.is_symlink():
        raise ValueError(f"required regular executable is unavailable: {path}")
    with path.open("rb") as stream:
        header = stream.read(64)
    if len(header) < 64 or header[:5] != b"\x7fELF\x02" or header[5] not in (1, 2):
        raise ValueError(f"required 64-bit ELF executable is invalid: {path}")
    endian = "<" if header[5] == 1 else ">"
    kind, found_machine = struct.unpack_from(endian + "HH", header, 16)
    if kind not in (2, 3) or found_machine != machine:
        raise ValueError(f"ELF type/machine does not match the Linux target: {path}")
    if struct.unpack_from(endian + "Q", header, 24)[0] == 0:
        raise ValueError(f"ELF executable has no entry point: {path}")


def build(root, release, messages_path):
    command = ["cargo", "build", "--locked", "-p", "glass-terminal-desktop",
               "-p", "privilege-broker", "--message-format=json-render-diagnostics"]
    if release:
        command.append("--release")
    files = {}
    with messages_path.open("w", encoding="utf-8") as log:
        process = subprocess.Popen(command, cwd=root, stdout=subprocess.PIPE, text=True)
        for line in process.stdout:
            log.write(line)
            message = json.loads(line)
            if message.get("reason") == "compiler-artifact":
                name = message.get("target", {}).get("name")
                if name in ("glass-terminal", "plyra-privilege-broker") and message.get("executable"):
                    files[name] = Path(message["executable"])
            if message.get("reason") == "build-script-executed" and "session-manager" in message.get("package_id", ""):
                helper = dict(message.get("env", [])).get("PLYRA_BUILT_HELPER")
                if helper:
                    files["plyra-terminal-helper"] = Path(helper)
        if process.wait() != 0:
            raise RuntimeError("native Linux Cargo build failed")
    if len(files) != 3:
        raise RuntimeError("Cargo did not provide the application, broker and native PTY helper")
    return files


def main():
    root = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=root.parent / "plyra-terminal-builds/glass-g1/linux/PlyraTerminal")
    parser.add_argument("--release", action="store_true")
    parser.add_argument("--version", default="0.1.0")
    parser.add_argument("--build-number", default="1")
    parser.add_argument("--target", choices=TARGETS)
    parser.add_argument("--bin-dir", type=Path, help="Package existing native binaries without compiling")
    parser.add_argument("--helper", type=Path, help="Native PTY helper when --bin-dir has no sibling helper")
    parser.add_argument("--aether-shell", type=Path, help="Bundle an existing native Aether Shell executable")
    parser.add_argument("--system-bridge", type=Path, help="Bundle the matching glass-system-bridge executable")
    args = parser.parse_args()
    target = args.target or NATIVE_TARGETS.get(platform.machine())
    if target not in TARGETS:
        parser.error("select a supported Linux --target")
    if not args.version or not args.build_number:
        parser.error("version and build number must be nonempty")
    if args.helper and not args.bin_dir:
        parser.error("--helper requires --bin-dir")
    if bool(args.aether_shell) != bool(args.system_bridge):
        parser.error("--aether-shell and --system-bridge must be supplied together")
    components = {}
    for name, path in (("aether-shell", args.aether_shell), ("glass-system-bridge", args.system_bridge)):
        if path is not None:
            source = path.resolve()
            verify_elf(source, TARGETS[target])
            components[name] = {"source": str(source), "source_sha256": sha256(source)}
    if args.bin_dir is None and (platform.system() != "Linux" or target != NATIVE_TARGETS.get(platform.machine())):
        parser.error("compile on the native Linux target, or provide --bin-dir and --target")
    output = args.output.resolve()
    marker = output / "glass-build.json"
    if output.exists() and (not marker.is_file() or json.loads(marker.read_text()).get("source_root") != str(root)):
        parser.error("existing output was not produced from this repository")
    output.parent.mkdir(parents=True, exist_ok=True)
    sources = source_snapshot(root)
    with tempfile.TemporaryDirectory(prefix="glass-linux-", dir=output.parent) as temporary:
        staging = Path(temporary)
        messages = staging / "cargo-messages.jsonl"
        if args.bin_dir:
            binaries = {name: args.bin_dir.resolve() / name for name in
                        ("glass-terminal", "plyra-terminal-helper", "plyra-privilege-broker")}
            if args.helper:
                binaries["plyra-terminal-helper"] = args.helper.resolve()
        else:
            binaries = build(root, args.release, messages)
        binaries.update({name: Path(component["source"]) for name, component in components.items()})
        for file in binaries.values():
            verify_elf(file, TARGETS[target])
        if source_snapshot(root) != sources:
            raise RuntimeError("build inputs changed during compilation")
        bundle = staging / "bundle"
        (bundle / "bin").mkdir(parents=True)
        for name, file in binaries.items():
            destination = bundle / "bin" / name
            shutil.copyfile(file, destination)
            destination.chmod(0o755)
            verify_elf(destination, TARGETS[target])
            if name in components:
                packaged_hash = sha256(destination)
                if packaged_hash != components[name]["source_sha256"]:
                    raise RuntimeError(f"runtime component changed during packaging: {file}")
                components[name]["packaged_sha256"] = packaged_hash
        if components:
            shell_root = root.parent / "aether-shell"
            licenses = bundle / "aether-shell-licenses"
            licenses.mkdir()
            shutil.copyfile(shell_root / "NOTICE", licenses / "NOTICE")
            for source in sorted((shell_root / "licenses").glob("*")):
                if source.is_file():
                    shutil.copyfile(source, licenses / source.name)
        for name in ("install.sh", "uninstall.sh", "installer.py", "README.md"):
            shutil.copyfile(root / "packaging/linux" / name, bundle / name)
            (bundle / name).chmod(0o755 if name.endswith(".sh") else 0o644)
        for name in ("org.plyra.glass-terminal.policy", "org.plyra.glass-terminal.desktop"):
            shutil.copyfile(root / "packaging/linux" / name, bundle / name)
        for source, name in ((root / "LICENSE", "LICENSE"),
                             (root / "assets/app-icon/glass-terminal.png", "glass-terminal.png"),
                             (root / "assets/terminal/GLASS_THIRD_PARTY_NOTICES.txt", "THIRD_PARTY_NOTICES.txt")):
            shutil.copyfile(source, bundle / name)
        marker_data = {
            "schema": "glass-terminal.linux-build.v1", "source_root": str(root),
            "target": target, "version": args.version, "build_number": args.build_number,
            "profile": "release" if args.release else "dev", "signing": "unsigned",
            "runtime_components": components,
            "source_files_sha256": sources,
            "source_snapshot_sha256": hashlib.sha256(json.dumps(sources, sort_keys=True).encode()).hexdigest(),
        }
        (bundle / "glass-build.json").write_text(json.dumps(marker_data, indent=2) + "\n", encoding="utf-8")
        files = [{"name": str(path.relative_to(bundle)), "bytes": path.stat().st_size, "sha256": sha256(path)}
                 for path in sorted(bundle.rglob("*")) if path.is_file()]
        manifest = {"schema": "glass-terminal.linux-package.v1", "target": target,
                    "elf_machine": TARGETS[target], "files": files}
        (bundle / "package-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
        previous = staging / "previous"
        if output.exists():
            output.rename(previous)
        try:
            bundle.rename(output)
        except OSError:
            if previous.exists():
                previous.rename(output)
            raise
        if messages.exists():
            shutil.copyfile(messages, output.parent / (output.name + ".cargo-messages.jsonl"))
    print(output)
    print(f"Manifest SHA256 {sha256(output / 'package-manifest.json')}")


if __name__ == "__main__":
    main()

# FVPROV1 9828 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNzUwMTM2YzQwMWQyZThiOWNlZjk3ODE2NTRjMDlkNDEyYTRhMDAxYzNjYmQ0NzQ1ODhmMDM2ODYxYjc4MGQ5YyIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMWE0N2ZjNmQxOTI3NTVjMWMyMGMyZGVjMDk3Mzg1NTkzZjFmYTU4MGIzM2M5NWJmODA4NTUxZmFiNjU5MjM4MTNlNGY3OTZlOWY1NGI5YjgzNWQyMDZjZmVjN2M2MDg4MTU0MWI5NDlmM2EyOGZhYzg3ZDYyYjcxZDVjOGRlMGIifV19XQ== FVPROV1-END
