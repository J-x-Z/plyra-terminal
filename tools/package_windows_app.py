#!/usr/bin/env python3
"""Package existing Windows binaries and produce their reviewable hash manifest."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import struct

TARGET_MACHINES = {
    "x86_64-pc-windows-gnu": 0x8664,
    "x86_64-pc-windows-msvc": 0x8664,
    "aarch64-pc-windows-msvc": 0xAA64,
}


def pe_machine(path):
    with path.open("rb") as stream:
        if stream.read(2) != b"MZ":
            raise ValueError(f"binary is not a PE image: {path}")
        stream.seek(0x3C)
        offset = stream.read(4)
        if len(offset) != 4:
            raise ValueError(f"truncated PE header: {path}")
        stream.seek(struct.unpack("<I", offset)[0])
        header = stream.read(6)
        if len(header) != 6 or header[:4] != b"PE\0\0":
            raise ValueError(f"invalid PE signature: {path}")
        return struct.unpack("<H", header[4:])[0]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--target", choices=TARGET_MACHINES, required=True)
    arguments = parser.parse_args()
    source = arguments.bin_dir.resolve()
    files = [source / "glass-terminal.exe", source / "plyra-privilege-broker.exe"]
    files.extend(sorted(source.glob("*.dll")))
    for path in files:
        if not path.is_file() or path.is_symlink():
            parser.error(f"required regular binary is unavailable: {path}")
        try:
            machine = pe_machine(path)
        except (OSError, ValueError) as error:
            parser.error(str(error))
        if machine != TARGET_MACHINES[arguments.target]:
            parser.error(f"PE machine 0x{machine:04x} does not match {arguments.target}: {path}")
    repository = Path(__file__).resolve().parents[1]
    files.extend([
        repository / "assets/terminal/GLASS_THIRD_PARTY_NOTICES.txt",
        repository / "assets/app-icon/glass-terminal.ico",
        repository / "packaging/windows/install-shortcut.ps1",
        repository / "LICENSE",
    ])
    for path in files:
        if not path.is_file() or path.is_symlink():
            parser.error(f"required package file is unavailable: {path}")
    output = arguments.output_dir.resolve()
    output.mkdir(parents=True, exist_ok=False)
    manifest = {"schema": "glass-terminal.windows-package.v1", "target": arguments.target,
                "pe_machine": TARGET_MACHINES[arguments.target], "files": []}
    for path in files:
        destination = output / path.name
        shutil.copy2(path, destination)
        data = destination.read_bytes()
        manifest["files"].append({"name": path.name, "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()})
    manifest_path = output / "package-manifest.json"
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    shutil.copy2(Path(__file__).with_name("install_windows_broker.ps1"), output / "install_windows_broker.ps1")
    print(f"Package {output}")
    print(f"Manifest SHA256 {hashlib.sha256(manifest_path.read_bytes()).hexdigest()}")


if __name__ == "__main__":
    main()

# FVPROV1 3211 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiMGNhMjEzZTJhZDE4YWJhNGY5MTQ0MzhlYjA5NjM1N2UyYWI0YTU2NTQzZjBjZjcwZTI5M2E1NzMwZGFjODNmOSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiOTNiMjIwZmNhYmUzYjI1YzQxMTJmYjcxZjNlYTFkNWZkYTNhM2Y2NWYzNzkzZTU5OTJjNmQzZTVkYjk4ZDhlNWJiYzA5NGM0NDQ3ODllYmZjMWM2YWU3MGU5ZjJkYzVjODRlYTBjM2Q1YWFlMDJiYzIxOWE1ZDE4MDE4YjIxMGIifV19XQ== FVPROV1-END
