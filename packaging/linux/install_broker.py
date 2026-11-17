#!/usr/bin/env python3
"""Install the explicitly selected Linux broker and its fixed polkit action."""
import argparse
import hashlib
import os
from pathlib import Path
import stat
import tempfile


BROKER = Path("/usr/libexec/glass-terminal/plyra-privilege-broker")
POLICY = Path("/usr/share/polkit-1/actions/org.plyra.glass-terminal.policy")


def protected_directory(path):
    if path.parent != path:
        protected_directory(path.parent)
    try:
        metadata = path.lstat()
    except FileNotFoundError:
        path.mkdir(mode=0o755)
        path.chmod(0o755)
        metadata = path.lstat()
    if not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != 0 or metadata.st_mode & 0o022:
        raise RuntimeError(f"Installation directory must be root-owned and protected: {path}")


def install_file(source, destination, mode, expected_sha256=None):
    protected_directory(destination.parent)
    if destination.exists() or destination.is_symlink():
        metadata = destination.lstat()
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != 0 or metadata.st_mode & 0o022:
            raise RuntimeError(f"Existing installation must be a protected root-owned file: {destination}")
    source_fd = os.open(source, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    temporary = None
    try:
        with os.fdopen(source_fd, "rb") as incoming:
            if not stat.S_ISREG(os.fstat(incoming.fileno()).st_mode):
                raise RuntimeError("Installation input must be a regular file")
            if expected_sha256 and incoming.read(4) != b"\x7fELF":
                raise RuntimeError("The selected broker must be a Linux ELF executable")
            incoming.seek(0)
            handle, temporary = tempfile.mkstemp(prefix=".glass-install-", dir=destination.parent)
            with os.fdopen(handle, "wb") as outgoing:
                digest = hashlib.sha256()
                while chunk := incoming.read(64 * 1024):
                    digest.update(chunk)
                    outgoing.write(chunk)
                if expected_sha256 and digest.hexdigest() != expected_sha256:
                    raise RuntimeError("Copied broker does not match the expected SHA-256")
                outgoing.flush()
                os.fsync(outgoing.fileno())
                os.fchmod(outgoing.fileno(), mode)
                os.fchown(outgoing.fileno(), 0, 0)
            os.replace(temporary, destination)
            temporary = None
            directory_fd = os.open(destination.parent, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(directory_fd)
            finally:
                os.close(directory_fd)
    finally:
        if temporary is not None:
            os.unlink(temporary)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--broker", type=Path, required=True)
    parser.add_argument("--sha256", required=True)
    options = parser.parse_args()
    if os.geteuid() != 0:
        parser.error("Installation requires an explicitly authorized root invocation")
    expected = options.sha256.lower()
    if len(expected) != 64 or any(character not in "0123456789abcdef" for character in expected):
        parser.error("--sha256 must contain the expected 64-digit broker SHA-256")
    policy_source = Path(__file__).resolve().with_name(POLICY.name)
    install_file(options.broker, BROKER, 0o755, expected)
    install_file(policy_source, POLICY, 0o644)
    print(f"Installed {BROKER} and {POLICY}")


if __name__ == "__main__":
    main()

# FVPROV1 3557 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiOTgyYzU3ZjYwZjFiYTNlN2ViZjAwYzBjMzFjMDA2ZTJjMzIzZGJjMWJlYzMyZGQ1ZmYxYjRmMjc0ZDhiNWY1MiIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiNTk4OGQ4Njk4MTljMGVkMTQ5N2JiZTE1ZDMwNTQ4YTU0NWViMzQwMzQxYjI4ZWFlZGJiZGRlOTY2MDM5YjRlMzNhZGU3ZWNlNTJmOWE4M2U3ZmU3YWI1Yzc0NWQ0MzhmNzU4ZDIxNzIzMjQwZGQ2MjdkZjc4NjU3MjY5ODI4MDcifV19XQ== FVPROV1-END
