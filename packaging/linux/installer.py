#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""Explicit user application and polkit helper installation."""
import argparse
from contextlib import contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shlex
import stat
import tempfile


SCHEMA = "glass-terminal.linux-installation.v1"
BROKER = Path("/usr/libexec/glass-terminal/plyra-privilege-broker")
POLICY = Path("/usr/share/polkit-1/actions/org.plyra.glass-terminal.policy")


def digest(data):
    return hashlib.sha256(data).hexdigest()


def regular_file(path, owner=None):
    metadata = path.lstat()
    if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1:
        raise ValueError(f"expected an unlinked regular installation file: {path}")
    if owner is not None and (metadata.st_uid != owner or metadata.st_mode & 0o022):
        raise ValueError(f"installation file ownership or mode changed: {path}")
    return path.read_bytes()


def protected_directory(path, create):
    for component in reversed((path, *path.parents)):
        if not component.exists():
            if not create:
                continue
            component.mkdir(mode=0o755)
        metadata = component.lstat()
        if (not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != 0
                or metadata.st_mode & 0o022):
            raise ValueError(f"system parent must be a protected root-owned directory: {component}")


def package_files(bundle):
    manifest = json.loads(regular_file(bundle / "package-manifest.json"))
    if manifest.get("schema") != "glass-terminal.linux-package.v1":
        raise ValueError("unsupported package manifest")
    files = {}
    for item in manifest["files"]:
        relative = Path(item["name"])
        if relative.is_absolute() or ".." in relative.parts or relative in files:
            raise ValueError("invalid or duplicate package path")
        data = regular_file(bundle / relative)
        if len(data) != item["bytes"] or digest(data) != item["sha256"]:
            raise ValueError(f"package hash mismatch: {relative}")
        files[relative] = data
    return files


def atomic_write(path, data, mode):
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary = tempfile.mkstemp(prefix=".glass-install-", dir=path.parent)
    try:
        with os.fdopen(descriptor, "wb") as stream:
            os.fchmod(stream.fileno(), mode)
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


@contextmanager
def installation_lock(directory):
    descriptor = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
    try:
        fcntl.flock(descriptor, fcntl.LOCK_EX)
        yield
    finally:
        os.close(descriptor)


def before_image(path, owner):
    try:
        data = regular_file(path, owner)
        return data, stat.S_IMODE(path.stat().st_mode)
    except FileNotFoundError:
        return None


def apply_changes(changes, owner):
    previous = {path: before_image(path, owner) for path in changes}
    applied = []
    try:
        for path, value in changes.items():
            if value is None:
                path.unlink()
            else:
                atomic_write(path, *value)
            applied.append(path)
    except BaseException:
        for path in reversed(applied):
            value = previous[path]
            if value is None:
                path.unlink(missing_ok=True)
            else:
                atomic_write(path, *value)
        raise


def optional_user_file(path, directory):
    try:
        relative = path.relative_to(directory)
    except ValueError:
        return False
    if ".." in relative.parts:
        return False
    return relative in (Path("bin/aether-shell"), Path("bin/glass-system-bridge")) or (
        len(relative.parts) == 2 and relative.parts[0] == "aether-shell-licenses")


def installed_files(receipt_path, allowed, owner, verify_hashes=True, optional_directory=None,
                    optional_paths=()):
    try:
        receipt = json.loads(regular_file(receipt_path, owner))
    except FileNotFoundError:
        return {}
    if receipt.get("schema") != SCHEMA or receipt.get("uid") != owner:
        raise ValueError("unsupported or differently owned installation receipt")
    recorded = {Path(item["path"]): item["sha256"] for item in receipt["files"]}
    required = set(allowed)
    if optional_directory is None:
        valid_paths = set(recorded) == required
    else:
        required.difference_update(optional_paths)
        required = {path for path in required if not optional_user_file(path, optional_directory)}
        valid_paths = required <= set(recorded) and all(
            path in required or path in optional_paths
            or optional_user_file(path, optional_directory) for path in recorded)
    if not valid_paths:
        raise ValueError("installation receipt paths do not match this installation")
    for path, expected in recorded.items():
        current = digest(regular_file(path, owner))
        if verify_hashes and current != expected:
            raise ValueError(f"installed file was modified: {path}")
    return recorded


def user_icon(prefix):
    return prefix / "share/icons/hicolor/256x256/apps/org.plyra.glass-terminal.png"


def desktop_entry(data, executable, icon):
    command = str(executable).replace("\\", "\\\\").replace('"', '\\"').replace("`", "\\`").replace("$", "\\$")
    command = command.replace("%", "%%")
    icon_value = str(icon).replace("\\", "\\\\")
    return (data.decode("utf-8").replace("Exec=glass-terminal\n", f'Exec="{command}"\n')
            .replace("Icon=org.plyra.glass-terminal\n", f"Icon={icon_value}\n").encode("utf-8"))


def destinations(args, bundle, files):
    if args.administrator:
        if os.geteuid() != 0:
            raise ValueError("administrator installation requires explicit root authorization")
        if args.prefix is not None:
            raise ValueError("the administrator helper uses the fixed system installation paths")
        create = args.operation == "install"
        protected_directory(BROKER.parent, create)
        protected_directory(POLICY.parent, create)
        targets = {BROKER: (files.get(Path("bin/plyra-privilege-broker")), 0o555),
                   POLICY: (files.get(Path(POLICY.name)), 0o644)}
        return targets, BROKER.parent / "installation.json", Path("/usr"), ()
    if os.geteuid() == 0:
        raise ValueError("run the user application installer as the intended ordinary user")
    prefix = (args.prefix or Path.home() / ".local").expanduser().resolve()
    if not prefix.is_absolute() or any(ord(value) < 32 for value in str(prefix)):
        raise ValueError("user prefix must be an absolute path without control characters")
    directory = prefix / "lib/glass-terminal"
    targets = {}
    for name in ("glass-terminal", "plyra-terminal-helper", "plyra-privilege-broker"):
        targets[directory / "bin" / name] = (files.get(Path("bin") / name), 0o755)
    for name in ("LICENSE", "THIRD_PARTY_NOTICES.txt", "glass-build.json"):
        targets[directory / name] = (files.get(Path(name)), 0o644)
    optional_binaries = (Path("bin/aether-shell"), Path("bin/glass-system-bridge"))
    included = [path in files for path in optional_binaries]
    if any(included):
        if not all(included) or Path("aether-shell-licenses/NOTICE") not in files:
            raise ValueError("Aether Shell requires its matching System API bridge and notices")
        for path in optional_binaries:
            targets[directory / path] = (files[path], 0o755)
        for path, data in files.items():
            if optional_user_file(directory / path, directory) and path not in optional_binaries:
                targets[directory / path] = (data, 0o644)
    executable = directory / "bin/glass-terminal"
    launcher = f"#!/bin/sh\nexec {shlex.quote(str(executable))} \"$@\"\n".encode("utf-8")
    targets[prefix / "bin/glass-terminal"] = (launcher, 0o755)
    icon = user_icon(prefix)
    targets[icon] = (files.get(Path("glass-terminal.png")), 0o644)
    desktop = files.get(Path("org.plyra.glass-terminal.desktop"))
    targets[prefix / "share/applications/org.plyra.glass-terminal.desktop"] = (
        desktop_entry(desktop, executable, icon) if desktop is not None else None, 0o644)
    lock_directory = prefix / "lib"
    lock_directory.mkdir(parents=True, exist_ok=True)
    # Existing installations predate the launcher icon and can upgrade in place.
    return targets, directory / "installation.json", lock_directory, (icon,)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("install", "uninstall"))
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--user", action="store_true")
    mode.add_argument("--administrator", action="store_true")
    parser.add_argument("--prefix", type=Path)
    args = parser.parse_args()
    bundle = Path(__file__).resolve().parent
    files = package_files(bundle) if args.operation == "install" else {}
    targets, receipt_path, lock_directory, optional_paths = destinations(args, bundle, files)
    with installation_lock(lock_directory):
        recorded = installed_files(
            receipt_path, targets, os.geteuid(), verify_hashes=not args.administrator,
            optional_directory=None if args.administrator else receipt_path.parent,
            optional_paths=optional_paths)
        if args.operation == "uninstall":
            if not recorded and args.administrator:
                # Preserve the source-tree installer's fixed-path lifecycle.
                recorded = {path: digest(regular_file(path, 0)) for path in targets
                            if path.exists() or path.is_symlink()}
            if not recorded:
                print("Plyra Terminal installation is already absent")
                return
            changes = {path: None for path in recorded}
            if receipt_path.exists():
                changes[receipt_path] = None
        else:
            if any(data is None for data, mode in targets.values()):
                raise ValueError("required package file is missing")
            for path in targets:
                if path not in recorded and not args.administrator and (path.exists() or path.is_symlink()):
                    raise ValueError(f"existing file has no Plyra Terminal installation receipt: {path}")
            receipt = {"schema": SCHEMA, "uid": os.geteuid(),
                       "package_sha256": digest(regular_file(bundle / "package-manifest.json")),
                       "files": [{"path": str(path), "sha256": digest(data)}
                                 for path, (data, mode) in targets.items()]}
            changes = {path: None for path in recorded if path not in targets}
            changes.update(targets)
            changes[receipt_path] = (json.dumps(receipt, indent=2).encode("utf-8") + b"\n", 0o644)
        apply_changes(changes, os.geteuid())
        removed_directories = {path.parent for path, value in changes.items() if value is None}
        if removed_directories:
            for directory in sorted(removed_directories, key=lambda path: len(path.parts), reverse=True):
                if directory != receipt_path.parent and receipt_path.parent not in directory.parents:
                    continue
                try:
                    directory.rmdir()
                except OSError:
                    pass
    print(f"Plyra Terminal {args.operation} completed")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, json.JSONDecodeError) as error:
        raise SystemExit(str(error))

# FVPROV1 11889 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiODdkZjEwNTI2YWRjMzI2OTllMTA4MWYxNzNlMWJkODhlNDRlMTRlNjAxNmFkMzYzNGI2Yzk1NjkyM2JjYjI0NSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiZTY2NjIyNzE1MzE3NDQ1NTAzYjM4ZGYyZTJhNTFlZjY3MjRkZmY4MWNlNmJkMTZiNGFiOTQ5Y2EwYmRhZTM5MTQ2MzFlZDAxNmRkZGJhMGQ1NzE1Mjk1ODQ2M2I4ZDZkMDFmZGFiYzBmYTM4YzljNGU1YmNkY2Y1ZGNkODZmMGYifV19XQ== FVPROV1-END
