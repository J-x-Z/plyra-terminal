#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""Collect locked dependency notices and source-pinned upstream supplements."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import tomllib


NOTICE_NAME = re.compile(
    r"^(licen[cs]e|copying|notice|copyright|unlicense)(?:$|[._-])", re.IGNORECASE
)
NOTICE_DIRECTORIES = {"license", "licenses", "licence", "licences", "legal"}
BUNDLED_NOTICES = (
    ("Plyra project code", "LICENSE"),
    ("DejaVu Sans Mono font", "assets/terminal-fonts/LICENSE.txt"),
    ("Geist UI font", "assets/ui-fonts/OFL.txt"),
    ("libvterm", "assets/terminal/LICENSE.libvterm"),
    ("musl runtime", "assets/terminal/LICENSE.musl"),
    ("compiler-rt runtime", "assets/terminal/LICENSE.compiler-rt"),
    ("Windows Zig 0.17.0 runtime", "assets/terminal/upstream-notices/runtime/zig-0.17.0/LICENSE.Zig.txt"),
    ("Windows MinGW-w64 runtime", "assets/terminal/upstream-notices/runtime/zig-0.17.0/COPYING.mingw-w64.txt"),
    ("Windows LLVM libunwind runtime", "assets/terminal/upstream-notices/runtime/zig-0.17.0/LICENSE.libunwind.txt"),
    ("Windows winpthreads MinGW-w64 notice", "assets/terminal/upstream-notices/runtime/zig-0.17.0/LICENSE.winpthreads-mingw.txt"),
    ("Windows winpthreads Intel notice", "assets/terminal/upstream-notices/runtime/zig-0.17.0/LICENSE.winpthreads-mutex.txt"),
    ("Windows winpthreads public-domain notice", "assets/terminal/upstream-notices/runtime/zig-0.17.0/NOTICE.winpthreads-public-domain.txt"),
    (
        "Rust 1.98.0-nightly standard-library copyright collection",
        "assets/terminal/upstream-notices/runtime/rust-1.98.0-nightly-f28ac764c/COPYRIGHT-library.html",
    ),
    (
        "Rust toolchain MIT license template",
        "assets/terminal/upstream-notices/runtime/rust-1.98.0-nightly-f28ac764c/LICENSE-MIT.txt",
    ),
    (
        "Rust toolchain Apache-2.0 license",
        "assets/terminal/upstream-notices/runtime/rust-1.98.0-nightly-f28ac764c/LICENSE-APACHE.txt",
    ),
    (
        "Rust compiler-builtins runtime",
        "assets/terminal/upstream-notices/runtime/rust-1.98.0-nightly-f28ac764c/LICENSE.compiler-builtins.txt",
    ),
    (
        "Rust stdarch copyright and MIT notice",
        "assets/terminal/upstream-notices/runtime/rust-1.98.0-nightly-f28ac764c/LICENSE.stdarch-MIT.txt",
    ),
    (
        "Rust portable-simd copyright and MIT notice",
        "assets/terminal/upstream-notices/runtime/rust-1.98.0-nightly-f28ac764c/LICENSE.portable-simd-MIT.txt",
    ),
)


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def notice_files(package: dict) -> list[Path]:
    root = Path(package["manifest_path"]).parent.resolve()
    if not root.is_dir():
        raise ValueError(f"Registry source directory is unavailable for {package['id']}")
    candidates: set[Path] = set()
    for entry in root.iterdir():
        if entry.is_file() and NOTICE_NAME.match(entry.name):
            candidates.add(entry)
        if entry.is_dir() and entry.name.casefold() in NOTICE_DIRECTORIES:
            candidates.update(path for path in entry.rglob("*") if path.is_file())
    if package.get("license_file"):
        declared = Path(package["license_file"])
        candidates.add(declared if declared.is_absolute() else root / declared)
    if package["name"] == "epaint_default_fonts":
        for filename in (
            "Hack-Regular.txt", "OFL.txt", "UFL.txt", "emoji-icon-font-mit-license.txt"
        ):
            candidates.add(root / "fonts" / filename)
    result = []
    for path in sorted(candidates):
        if not path.resolve().is_relative_to(root):
            raise ValueError(f"Notice path leaves its package directory: {path}")
        if not path.is_file():
            raise ValueError(f"Declared notice file is missing: {path}")
        if b"\0" in path.read_bytes():
            raise ValueError(f"Notice candidate contains binary data: {path}")
        result.append(path)
    return result


def append_notice(output: list[str], title: str, filename: str, path: Path) -> None:
    raw = path.read_bytes()
    try:
        original = raw.decode("utf-8")
        encoding = "UTF-8"
    except UnicodeDecodeError:
        original = raw.decode("latin-1")
        encoding = "ISO-8859-1 byte mapping"
    output.extend(
        [
            "=" * 78,
            title,
            f"File {filename}",
            f"Original file SHA-256 {hashlib.sha256(raw).hexdigest()}",
            f"Text decoding {encoding}",
            "-" * 78,
            original,
        ]
    )
    if not original.endswith("\n"):
        output.append("")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--metadata", required=True, type=Path)
    arguments = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    lockfile = root / "Cargo.lock"
    metadata = json.loads(arguments.metadata.read_text())
    lock = tomllib.loads(lockfile.read_text())
    workspace = tomllib.loads((root / "Cargo.toml").read_text())
    patch_paths = {
        (root / patch["path"]).resolve()
        for patch in workspace.get("patch", {}).get("crates-io", {}).values()
        if "path" in patch
    }
    for path in patch_paths:
        if not path.is_relative_to(root / "vendor"):
            raise ValueError(f"Local dependency patch is outside the vendored source directory: {path}")
    vendored = [
        package
        for package in metadata["packages"]
        if package.get("source") is None
        and Path(package["manifest_path"]).resolve().parent in patch_paths
    ]
    if {Path(package["manifest_path"]).resolve().parent for package in vendored} != patch_paths:
        raise ValueError("Cargo metadata omits a local dependency patch. Refresh with --locked --offline --all-features.")
    vendored_ids = {package["id"] for package in vendored}
    vendored_versions = {(package["name"], package["version"]) for package in vendored}
    packages = sorted(
        (
            package
            for package in metadata["packages"]
            if (package.get("source") or "").startswith("registry+")
            or package["id"] in vendored_ids
        ),
        key=lambda package: (package["name"], package["version"], package.get("source") or ""),
    )
    locked_packages = {
        (package["name"], package["version"], package.get("source"))
        for package in lock["package"]
        if package.get("source", "").startswith("registry+")
        or (package.get("source") is None and (package["name"], package["version"]) in vendored_versions)
    }
    metadata_packages = {
        (package["name"], package["version"], package.get("source"))
        for package in packages
    }
    if locked_packages != metadata_packages:
        missing = sorted(locked_packages - metadata_packages)
        extra = sorted(metadata_packages - locked_packages)
        raise ValueError(
            f"Cargo metadata differs from Cargo.lock. Refresh metadata with --locked --offline --all-features. "
            f"Missing {missing}. Extra {extra}."
        )

    supplement_root = root / "assets/terminal/upstream-notices"
    supplement_manifest = supplement_root / "manifest.json"
    supplements = json.loads(supplement_manifest.read_text())["groups"] if supplement_manifest.is_file() else []
    selected_versions = {(package["name"], package["version"]): package for package in packages}
    supplemented_versions = set()
    for group in supplements:
        for package in group["packages"]:
            key = (package["name"], package["version"])
            selected = selected_versions.get(key)
            if selected is None or selected.get("license") != package["license"]:
                raise ValueError(f"Upstream notice supplement differs from the locked package: {key}")
            supplemented_versions.add(key)
        for notice in group["files"]:
            path = (root / notice["path"]).resolve()
            commit = notice["commit"]
            if not path.is_relative_to(supplement_root) or sha256(path) != notice["sha256"]:
                raise ValueError(f"Upstream notice supplement path or hash differs: {path}")
            if not re.fullmatch(r"[0-9a-f]{40}", commit) or f"/{commit}/" not in notice["source_url"]:
                raise ValueError(f"Upstream notice URL is not pinned to its recorded commit: {path}")

    inventory = [
        "# Locked dependency licenses",
        "",
        "Generated from Cargo.lock and `cargo metadata --locked --offline --all-features --format-version 1`.",
        "The inventory includes registry packages and declared vendored patches locked for every target platform and workspace feature.",
        "License expressions reproduce each package's Cargo metadata declaration, including alternatives.",
        "",
        f"Cargo.lock SHA-256 `{sha256(lockfile)}`.",
        f"Metadata JSON SHA-256 `{sha256(arguments.metadata)}`.",
        "",
        "The Source column links to exact published crate versions or the provenance of local patches. Registry source identifiers appear below.",
        "",
    ]
    for source in sorted({package["source"] for package in packages if package.get("source")}):
        inventory.append(f"- `{source}`")
    inventory.extend(
        [
            "",
            "The collected original texts are packaged in `assets/terminal/GLASS_THIRD_PARTY_NOTICES.txt`.",
            "The collection includes the project's GPL text and existing font, libvterm, and runtime notices.",
            "Source-pinned upstream supplements retain their own provenance and historical-document labels.",
            "",
            "| Package | Version | Declared license | Source | Original notice files |",
            "| --- | --- | --- | --- | --- |",
        ]
    )
    notices = [
        "Plyra Terminal third-party license and notice texts",
        "",
        "This collection contains notices shipped with registry sources and declared vendored patches for every locked platform.",
        "Cargo package license expressions are reproduced as declared, including alternatives.",
        f"Cargo.lock SHA-256 {sha256(lockfile)}",
        "",
        "Project and bundled asset notices",
        "",
    ]
    for label, filename in BUNDLED_NOTICES:
        append_notice(notices, label, filename, root / filename)

    missing_notices = []
    original_file_count = 0
    registry_file_count = 0
    for package in packages:
        files = notice_files(package)
        original_file_count += len(files)
        if package.get("source"):
            registry_file_count += len(files)
        package_root = Path(package["manifest_path"]).parent
        name = package["name"]
        version = package["version"]
        license_expression = package.get("license") or "Unspecified"
        filenames = [path.relative_to(package_root).as_posix() for path in files]
        source = package.get("source")
        source_description = f"Registry source {source}"
        source_link = f"[crates.io](https://crates.io/crates/{name}/{version})"
        if source is None:
            relative = package_root.resolve().relative_to(root).as_posix()
            source = f"vendored path {relative}"
            source_description = f"Vendored source {source}"
            source_link = f"[vendored patch](../{relative}/PROVENANCE.json)"
        inventory.append(
            f"| {name} | {version} | {license_expression} | "
            f"{source_link} | "
            f"{', '.join(f'`{filename}`' for filename in filenames) or 'Missing'} |"
        )
        notices.extend(
            [
                "",
                "=" * 78,
                f"Package {name} {version}",
                f"Declared license {license_expression}",
                source_description,
                f"Published crate https://crates.io/crates/{name}/{version}",
                f"Repository {package.get('repository') or 'Unspecified'}",
                "",
            ]
        )
        for path, filename in zip(files, filenames):
            append_notice(notices, f"Package {name} {version}", filename, path)
        if not files:
            missing_notices.append(f"{name} {version}")
            notices.append(f"Declared package authors {', '.join(package.get('authors') or []) or 'Unspecified'}")
            notices.append(
                "No LICENSE, LICENCE, COPYING, NOTICE, COPYRIGHT, or UNLICENSE file was present "
                "in the published package root or its license directories."
            )

    inventory.extend(
        [
            "",
            "## Collection coverage",
            "",
            f"Collected {original_file_count} notice files across {len(packages) - len(vendored)} registry and {len(vendored)} vendored package versions.",
            f"{len(missing_notices)} package versions provided no notice file in the inspected locations.",
            f"Fixed upstream supplements are collected for {len(supplemented_versions)} of those package versions.",
            "The corresponding metadata declarations and repository URLs remain in the packaged notice collection.",
            "",
        ]
    )
    for package in missing_notices:
        inventory.append(f"- {package}")
    inventory.extend(
        [
            "",
            "## Fixed upstream supplements",
            "",
            "These files come from recorded upstream commits. They supplement the published package layout.",
            "Historical license texts are labeled separately from documents at the package's current commit.",
            "The checked-in manifest and byte hashes are in `assets/terminal/upstream-notices/manifest.json`.",
            "",
            "| Packages | Upstream commit | Original supplemental documents | Scope |",
            "| --- | --- | --- | --- |",
        ]
    )
    for group in supplements:
        labels = ", ".join(f"{package['name']} {package['version']}" for package in group["packages"])
        documents = ", ".join(
            f"[{notice['upstream_path']}]({notice['source_url']}) ({notice['kind']})"
            for notice in group["files"]
        )
        scope = "Fixed upstream documents"
        if group.get("contains_current_commit_full_license_text") is False:
            scope = "Current commit provides a license declaration and links. Historical MIT text is separate."
        inventory.append(f"| {labels} | `{group['commit']}` | {documents} | {scope} |")
        notices.extend(["", "=" * 78, f"Fixed upstream supplements for {labels}"])
        for notice in group["files"]:
            notices.extend(
                [
                    f"Source URL {notice['source_url']}",
                    f"Upstream commit {notice['commit']}",
                    f"Document kind {notice['kind']}",
                    notice.get("applies_as", "Original document from the recorded upstream commit."),
                ]
            )
            append_notice(
                notices, f"Upstream supplement for {labels}",
                notice["upstream_path"], root / notice["path"],
            )
    supplement_labels = {f"{name} {version}" for name, version in supplemented_versions}
    unresolved = [package for package in missing_notices if package not in supplement_labels]
    inventory.extend(
        [
            "",
            "## Remaining original-notice gaps",
            "",
            "The following package versions have metadata declarations and source references but no original notice text located by this collection.",
            "",
        ]
    )
    inventory.extend(f"- {package}" for package in unresolved)
    inventory.extend(
        [
            "",
            "## Reproduce",
            "",
            "Refresh the metadata JSON after dependency changes and rerun the collector.",
            "",
            "```sh",
            "mkdir -p target",
            "cargo metadata --locked --offline --all-features --format-version 1 > target/metadata.json",
            "python3 tools/glass_licenses.py --metadata target/metadata.json",
            "```",
            "",
        ]
    )
    inventory_path = root / "docs/DEPENDENCY_LICENSES.md"
    notices_path = root / "assets/terminal/GLASS_THIRD_PARTY_NOTICES.txt"
    inventory_path.write_text("\n".join(inventory), encoding="utf-8", newline="")
    notices_path.write_text("\n".join(notices) + "\n", encoding="utf-8", newline="")
    print(
        json.dumps(
            {
                "registry_packages": len(packages) - len(vendored),
                "registry_notice_files": registry_file_count,
                "vendored_packages": len(vendored),
                "packages_without_notice_files": len(missing_notices),
                "supplemented_package_versions": len(supplemented_versions),
                "packages_without_notice_files_after_supplements": len(unresolved),
                "notices_bytes": notices_path.stat().st_size,
                "inventory": str(inventory_path),
                "notices": str(notices_path),
            },
            indent=2,
        )
    )


if __name__ == "__main__":
    main()

# FVPROV1 17262 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNjliNGIyNTE1MzlhNGZmNjBmN2NkZjU4MGJkZWI0MDFlZDUyOGQ2OTgyYWRkZDUyNjIxZTlhOTVkMjBiZmExNSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiYzllM2YzZmFiZTQ1NTM4NWNlMzY0ZDFiOGU5Mzg2NWQ5YmI2YzM5ZGUxYTQ5ZTlhNDczZDgyMmU5OTExMzMxOWVlODNkZjg5NjA0NjA1NGU0NWQ1NTI2ZDdmYjdkZmM2N2NlMzA2NzdmZDk0ZGQzMDQ3ZDNhMGY1NjM5NDY0MGYifV19XQ== FVPROV1-END
