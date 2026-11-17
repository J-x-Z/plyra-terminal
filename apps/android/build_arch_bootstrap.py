#!/usr/bin/env python3
"""Derive an Android userspace bootstrap from the pinned, signed Arch ARM image.

Only the Linux kernel and firmware packages are removed. Their package-owned
payload and pacman registrations are removed together; all retained packages,
versions and required dependency providers remain unchanged. No rootfs is ever
extracted onto the build host, and nothing is downloaded or executed from it.
"""

import argparse
import collections
import contextlib
import copy
import gzip
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import tarfile
import time


SOURCE_SHA256 = "42a4eeaa038994ffd31fa173256ef2f0ef511358eeb41b9ea1f8626391b9b319"
SOURCE_BYTES = 829367415
SIGNER = "68B3537F39A313B3E574D06777193F152BDBE6A6"
ARCHIVE_NAME = "ArchLinuxARM-aarch64-bootstrap.tar.gz"
REQUIRED_PACKAGES = {
    "base", "filesystem", "bash", "glibc", "pacman", "archlinux-keyring",
    "archlinuxarm-keyring", "linux-api-headers",
}
REQUIRED_ELF = {
    "usr/bin/bash", "usr/bin/pacman", "usr/lib/libc.so.6",
    "usr/lib/ld-linux-aarch64.so.1",
}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def name_without_prefix(name):
    name = name.removeprefix("./").rstrip("/")
    require(not name.startswith("/") and ".." not in PurePosixPath(name).parts,
            f"Unsafe archive path: {name!r}")
    return name


def kind(member):
    if member.isfile():
        return "file"
    if member.isdir():
        return "directory"
    if member.issym():
        return "symlink"
    if member.islnk():
        return "hardlink"
    raise ValueError(f"Unsupported archive member type: {member.name!r}")


class HashingReader:
    def __init__(self, file):
        self.file = file
        self.sha256 = hashlib.sha256()
        self.bytes = 0

    def read(self, size=-1):
        data = self.file.read(size)
        self.sha256.update(data)
        self.bytes += len(data)
        return data


@contextlib.contextmanager
def source_tar(path):
    """Verify the exact compressed source while reading and checking gzip CRC."""
    with path.open("rb") as raw:
        reader = HashingReader(raw)
        with gzip.GzipFile(fileobj=reader, mode="rb") as inflated:
            with tarfile.open(fileobj=inflated, mode="r|", bufsize=1024 * 1024) as tar:
                yield tar
            while inflated.read(1024 * 1024):
                pass
        require(reader.bytes == SOURCE_BYTES, "Pinned source byte count changed")
        require(reader.sha256.hexdigest() == SOURCE_SHA256, "Pinned source SHA256 changed")


def fields(data):
    result, key = {}, None
    for line in data.decode("utf-8").splitlines():
        if line.startswith("%") and line.endswith("%"):
            key = line.strip("%")
            result[key] = []
        elif line and key:
            result[key].append(line)
    return result


def dependency_name(dependency):
    return re.split(r"[<>=]", dependency, maxsplit=1)[0]


def analyze_source(source):
    packages, entries, presets = {}, {}, {}
    with source_tar(source) as tar:
        for member in tar:
            name = name_without_prefix(member.name)
            require(name not in entries, f"Duplicate source entry: {name}")
            require(not member.sparse, "Sparse source entries require an explicit policy")
            entries[name] = {"type": kind(member), "size": member.size,
                             "target": member.linkname}
            parts = name.split("/")
            if (len(parts) == 6 and parts[:4] == ["var", "lib", "pacman", "local"]
                    and parts[-1] in {"desc", "files"}):
                require(member.isfile() and member.size <= 2 * 1024 * 1024,
                        f"Unexpected package metadata: {name}")
                packages.setdefault(parts[4], {})[parts[5]] = fields(tar.extractfile(member).read())
            if name == "etc/mkinitcpio.d/linux-aarch64.preset":
                require(member.isfile() and member.size <= 65536, "Unexpected kernel preset")
                presets[name] = tar.extractfile(member).read().decode("utf-8")
    require(packages, "Source has no pacman local database")
    require(all("desc" in package and "files" in package for package in packages.values()),
            "Incomplete pacman package metadata")
    return packages, entries, presets


def removal_plan(packages, entries, presets):
    removed, kept, owners = {}, {}, collections.defaultdict(set)
    for directory, package in packages.items():
        desc = package["desc"]
        name, version = desc["NAME"][0], desc["VERSION"][0]
        base = desc.get("BASE", [name])[0]
        is_boot_package = name == "linux-aarch64" or (
            base == "linux-firmware" and (name == base or name.startswith(base + "-")))
        record = {"name": name, "version": version, "directory": directory}
        (removed if is_boot_package else kept)[directory] = record
        for owned in package["files"].get("FILES", []):
            owners[name_without_prefix(owned)].add(directory)
    require(any(record["name"] == "linux-aarch64" for record in removed.values()),
            "Pinned kernel package not found")
    require(REQUIRED_PACKAGES <= {record["name"] for record in kept.values()},
            "A required userspace package would be missing")

    removed_provides, retained_provides = set(), set()
    for directory, package in packages.items():
        desc = package["desc"]
        provides = {desc["NAME"][0]} | {
            dependency_name(item) for item in desc.get("PROVIDES", [])}
        (removed_provides if directory in removed else retained_provides).update(provides)
    required_count, unresolved, touched = 0, [], []
    for directory in kept:
        for dependency in packages[directory]["desc"].get("DEPENDS", []):
            required_count += 1
            name = dependency_name(dependency)
            if name not in retained_provides:
                unresolved.append([kept[directory]["name"], dependency])
            if name in removed_provides:
                touched.append([kept[directory]["name"], dependency])
    require(not unresolved, f"Retained dependency provider missing: {unresolved}")
    # No retained dependency even names a removed provider. Consequently its
    # original provider version and every version constraint are unaffected;
    # this is not a replacement for pacman's version comparison implementation.
    require(not touched, f"Removed provider participates in a retained dependency: {touched}")

    drop = {name for name in entries if owners[name] and owners[name] <= removed.keys()}
    db_roots = {f"var/lib/pacman/local/{directory}" for directory in removed}
    drop.update(name for name in entries
                if any(name == root or name.startswith(root + "/") for root in db_roots))

    generated = set()
    for preset_path, text in presets.items():
        require(owners[preset_path] <= removed.keys(), "Kernel preset is shared with retained package")
        for line in text.splitlines():
            image = re.fullmatch(r'\s*(?:default|fallback)_image="(/boot/[^"\n]+)"\s*', line)
            if image:
                path = name_without_prefix(image[1].lstrip("/"))
                if path in entries and not owners[path]:
                    generated.add(path)
    module_roots = {
        "/".join(name.split("/")[:4]) for name, package_owners in owners.items()
        if name.startswith("usr/lib/modules/") and package_owners
        and package_owners <= removed.keys() and len(name.split("/")) >= 4}
    for name in entries:
        if (not owners[name] and any(name == root or name.startswith(root + "/")
                                     for root in module_roots)):
            generated.add(name)
    drop.update(generated)

    # Keep shared parent directories, including ones needed by unowned files.
    parent_directories = set()
    for name in entries.keys() - drop:
        parent_directories.update(str(parent) for parent in PurePosixPath(name).parents)
    drop.difference_update({name for name in drop
                            if entries[name]["type"] == "directory" and name in parent_directories})
    for name in entries.keys() - drop:
        if entries[name]["type"] == "hardlink":
            target = name_without_prefix(entries[name]["target"])
            require(target in entries and target not in drop,
                    f"Retained hardlink would lose its target: {name} -> {target}")
    require(not any(owners[path] & kept.keys() for path in drop),
            "Removal would change retained package-owned files")
    require(REQUIRED_ELF <= entries.keys() - drop, "Required glibc/Shell ELF missing")
    proof = {"retained_required_dependency_edges": required_count,
             "missing_retained_providers": unresolved,
             "retained_dependencies_naming_removed_providers": touched,
             "provider_versions": "All retained packages and provider versions unchanged",
             "source_package_count": len(packages), "retained_package_count": len(kept),
             "removed_package_count": len(removed)}
    return drop, sorted(removed.values(), key=lambda item: item["name"]), kept, generated, proof


def entry_digest(digest, member, content_sha256):
    record = [member.name, kind(member), member.size, member.mode, member.uid, member.gid,
              member.uname, member.gname, member.linkname, member.mtime]
    digest.update(json.dumps(record, ensure_ascii=True, separators=(",", ":")).encode())
    digest.update(b"\0" + content_sha256 + b"\0")


def pack(source, output, drop, level):
    semantic, counts, expanded = hashlib.sha256(), collections.Counter(), 0
    with output.open("xb") as raw:
        with gzip.GzipFile(filename="", fileobj=raw, mode="wb", compresslevel=level, mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w|", format=tarfile.PAX_FORMAT,
                              bufsize=1024 * 1024) as target:
                with source_tar(source) as original:
                    for member in original:
                        if name_without_prefix(member.name) in drop:
                            continue
                        data = HashingReader(original.extractfile(member)) if member.isfile() else None
                        target.addfile(copy.copy(member), data)
                        counts[kind(member)] += 1
                        expanded += member.size if member.isfile() else 0
                        entry_digest(semantic, member, data.sha256.digest() if data else b"")
    return {"semantic_sha256": semantic.hexdigest(), "types": dict(counts),
            "entries": sum(counts.values()), "regular_file_expanded_bytes": expanded}


def verify_output(path, expected, kept, entries, drop):
    semantic, seen, elf, registrations = hashlib.sha256(), set(), {}, set()
    with gzip.open(path, "rb") as stream:
        with tarfile.open(fileobj=stream, mode="r|", bufsize=1024 * 1024) as tar:
            for member in tar:
                name = name_without_prefix(member.name)
                require(name not in seen and name in entries and name not in drop,
                        f"Unexpected output entry: {name}")
                seen.add(name)
                content = hashlib.sha256()
                if member.isfile():
                    file = tar.extractfile(member)
                    prefix = file.read(min(member.size, 64))
                    content.update(prefix)
                    for block in iter(lambda: file.read(1024 * 1024), b""):
                        content.update(block)
                    if name in REQUIRED_ELF:
                        require(prefix[:6] == b"\x7fELF\x02\x01" and
                                int.from_bytes(prefix[18:20], "little") == 183,
                                f"Required executable is not AArch64 ELF64: {name}")
                        elf[name] = content.hexdigest()
                if name.startswith("var/lib/pacman/local/") and name.endswith("/desc"):
                    registrations.add(name.split("/")[4])
                entry_digest(semantic, member, content.digest() if member.isfile() else b"")
        while stream.read(1024 * 1024):
            pass
    require(seen == entries.keys() - drop, "Output lost a retained entry")
    require(semantic.hexdigest() == expected["semantic_sha256"], "Output content or metadata changed")
    require(registrations == kept.keys(), "Output pacman registrations do not match retained packages")
    require(elf.keys() == REQUIRED_ELF, "Output missing a required ELF executable")
    return {"passed": True, "semantic_sha256": semantic.hexdigest(),
            "required_aarch64_elf_sha256": elf, "pacman_registrations": len(registrations),
            "gzip_crc_verified_to_eof": True,
            "all_retained_content_and_basic_metadata_verified": True,
            "extended_metadata_policy": "PAX ACL/xattr metadata copied from upstream TarInfo; semantic readback covers content and basic metadata."}


def file_hash(path):
    digest = hashlib.sha256()
    with path.open("rb") as file:
        for block in iter(lambda: file.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def main():
    repository = Path(__file__).resolve().parents[2]
    default = repository.parent / "plyra-terminal-builds/glass-g1/android"
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path,
                        default=default / "arch-rootfs-arm64/ArchLinuxARM-aarch64-latest.tar.gz")
    parser.add_argument("--output-dir", type=Path, default=default / "arch-bootstrap-arm64")
    parser.add_argument("--compression-level", type=int, choices=range(1, 10), default=6)
    arguments = parser.parse_args()
    os.nice(10)
    builder_source = Path(__file__).read_bytes()
    builder_sha256 = hashlib.sha256(builder_source).hexdigest()
    source, directory = arguments.source.resolve(), arguments.output_dir.resolve()
    require(source.is_file() and source.stat().st_size == SOURCE_BYTES, "Pinned source size mismatch")
    directory.mkdir(parents=True, exist_ok=True)
    output, manifest = directory / ARCHIVE_NAME, directory / "bootstrap-manifest.json"
    if output.exists():
        existing = json.loads(manifest.read_text())
        require(existing.get("schema") == "glass.android.arch-bootstrap.v1" and
                existing["source"]["sha256"] == SOURCE_SHA256 and existing["verification"]["passed"],
                "Existing bootstrap lacks a matching verified manifest")
        require(output.stat().st_size == existing["artifact"]["bytes"] and
                file_hash(output) == existing["artifact"]["sha256"], "Existing bootstrap changed")
        print(json.dumps({"reused": True, "manifest": str(manifest), "artifact": existing["artifact"]}))
        return
    started = time.monotonic()
    packages, entries, presets = analyze_source(source)
    drop, removed, kept, generated, proof = removal_plan(packages, entries, presets)
    partial = directory / (ARCHIVE_NAME + ".partial")
    require(not partial.exists(), "Refusing to overwrite an existing partial bootstrap")
    builder_snapshot = directory / ("builder-" + builder_sha256[:16] + ".py")
    if builder_snapshot.exists():
        require(builder_snapshot.read_bytes() == builder_source, "Builder snapshot changed")
    else:
        builder_snapshot.write_bytes(builder_source)
    print(json.dumps({"phase": "pack", "removed_packages": removed, "dependency_proof": proof}), flush=True)
    packed = pack(source, partial, drop, arguments.compression_level)
    verification = verify_output(partial, packed, kept, entries, drop)
    partial.rename(output)
    result = {"schema": "glass.android.arch-bootstrap.v1", "architecture": "arm64-v8a",
              "distribution": "Arch Linux ARM", "format": "tar.gz",
              "source": {"name": source.name, "bytes": SOURCE_BYTES, "sha256": SOURCE_SHA256,
                         "official_signing_fingerprint": SIGNER,
                         "signature_receipt": str(source.parent / "receipt.json")},
              "artifact": {"name": output.name, "bytes": output.stat().st_size,
                           "sha256": file_hash(output)},
              "derivation": {"removed_packages": removed,
                             "removed_entries": len(drop),
                             "generated_kernel_artifacts_removed": sorted(generated),
                             "compression_level": arguments.compression_level,
                             "gzip_mtime": 0, "no_host_extraction": True,
                             "no_retained_package_payload_or_registration_changes": True},
              "retained_packages": sorted(kept.values(), key=lambda item: item["name"]),
              "dependency_proof": proof, "rootfs": packed, "verification": verification,
              "build_seconds": round(time.monotonic() - started, 3),
              "builder_sha256": builder_sha256, "builder_snapshot": builder_snapshot.name,
              "notes": ["Derivative is not itself upstream-signed; its exact source was GPG verified.",
                        "Android runtime must supply DNS; upstream resolv.conf is a systemd symlink.",
                        "All other userspace, package manager, keyrings and licenses are retained."]}
    manifest.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({"manifest": str(manifest), "artifact": result["artifact"],
                      "rootfs": packed, "dependency_proof": proof, "verified": True}, indent=2))


if __name__ == "__main__":
    main()

# FVPROV1 17689 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNjAxYTk1ZTQyNDFlZGZlYmRhYjVkYzMwNTc3YTEwMmU4NjkwOWJiYTk0MmI4ZjQxOTc1NjBhNzUyOTAzYjhmMSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMjhiYzhkYjJmYmYxZmRiZTgyYjdkNzAxNWE1MmI5Njg1NzA0NGFjM2VmNjlhMDAyN2E5NDQzNjIwYWU2Y2MyNWY2YmFiYjk0OTEyZGRmMzYyYjc3NTI5ODZjZjhiODRkZmE5OGIyMmI0NWQxYjUzMzgyMTllOTczZGQxMTg2MDYifV19XQ== FVPROV1-END
