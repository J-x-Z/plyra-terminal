#!/usr/bin/env python3
"""Build the pinned, rootless arm64 Arch userspace launcher (not an Arch rootfs).

Uses the existing Android NDK 25.1, one compiler job, and under 3 MiB of pinned
source downloads. No Cargo, Gradle, emulator, root, or device is used. Outputs:
  jniLibs/arm64-v8a/libglass_proot.so         Android PIE executable
  jniLibs/arm64-v8a/libglass_proot_loader.so freestanding guest ELF loader
  runtime-manifest.json, build.log, licenses/, sources/, patches/, relink/

Package both .so-named *executables* as extracted APK native libraries. They are
not JNI libraries and must not be dlopen'ed. Launch libglass_proot.so by absolute
nativeLibraryDir path, set PROOT_LOADER to the adjacent loader's absolute path,
and set PROOT_TMP_DIR to a unique, private, writable directory per session.
Never copy the loader to writable app data: Android 10+ executable restrictions
apply there. This build separates it specifically for that restriction.

Pass --kill-on-exit so exiting the initial guest kills its traced descendants.
For teardown, SIGQUIT the tracer and wait/reap; upstream ignores SIGTERM in its
event loop. The narrowly recorded EXITKILL patch also kills tracees if the
tracer is killed. Do not treat PRoot as a security boundary or actual root.

Suggested initial argv: --kill-on-exit -0 -r ROOTFS -w /root /bin/bash --login.
Bind only explicit device files needed by the guest (/dev/null, /dev/zero,
/dev/random, /dev/urandom, /dev/tty); add /proc only when required. Do not bind
host /data, /storage, /sdcard, /system or /apex wholesale. Set a guest PATH/HOME,
TERM, UTF-8 locale and an app-managed resolv.conf. Initialize the guest's pacman
keyring explicitly; do not boot the rootfs kernel or systemd. No 32-bit guests
are built. Runtime execution and Android 14 guest loading require device tests.

Example:
  python3 apps/android/build_arch_runtime.py --output-dir /external/arch-runtime-arm64
  python3 apps/android/build_arch_runtime.py --offline --output-dir /external/arch-runtime-arm64

Source/patches/build instructions and PRoot relink inputs are retained beside
the binaries. Ship those corresponding-source files and license notices when
redistributing this external GPL/LGPL program. The source archives are never
silently updated: all versions and SHA-256 pins below must match.
"""

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shlex
import shutil
import struct
import subprocess
import sys
import tarfile
import time
import urllib.request
import zipfile


SOURCES = {
    "proot": {
        "version": "5.1.107.102",
        "commit": "52d65a4dc140ca554d241a1438ddd4f6b00c9813",
        "file": "proot-5.1.107.102.zip",
        "url": "https://github.com/termux/proot/archive/v5.1.107.102.zip",
        "sha256": "30d12a6e2cc4147123756ff58edea262d3083bbc238c12d0ab9b39e3d1c40852",
        "license": "GPL-2.0-or-later; combined runtime conveyed under GPL-3.0-or-later",
    },
    "talloc": {
        "version": "2.5.0",
        "file": "talloc-2.5.0.tar.gz",
        "url": "https://www.samba.org/ftp/talloc/talloc-2.5.0.tar.gz",
        "sha256": "912afa237510ae542a7733998eb18a12bcda35ab6729c8e2ddb43e8d0ebab007",
        "license": "LGPL-3.0-or-later (talloc/replace source headers); archive includes other GPL tools",
    },
}
GPL3_TEXT = {
    "file": "GPL-3.0.txt",
    "url": "https://raw.githubusercontent.com/gcc-mirror/gcc/releases/gcc-14.2.0/COPYING3",
    "sha256": "8ceb4b9ee5adedde47b31e975c1d90c73ad27b6b165a1dcd80c7c545eb65b903",
}

# Equivalent to the published Termux talloc cross answers, plus little-endian
# aarch64's WORDS_BIGENDIAN=false (the target is fixed, never host-detected).
CROSS_ANSWERS = '''Checking uname sysname type: "Linux"
Checking uname machine type: "aarch64"
Checking uname release type: "dontcare"
Checking uname version type: "dontcare"
Checking simple C program: OK
building library support: OK
Checking for large file support: OK
Checking for -D_FILE_OFFSET_BITS=64: OK
Checking for WORDS_BIGENDIAN: FAIL
Checking for C99 vsnprintf: OK
Checking for HAVE_SECURE_MKSTEMP: OK
rpath library support: OK
-Wl,--version-script support: FAIL
Checking correct behavior of strtoll: OK
Checking correct behavior of strptime: OK
Checking for HAVE_IFACE_GETIFADDRS: OK
Checking for HAVE_IFACE_IFCONF: OK
Checking for HAVE_IFACE_IFREQ: OK
Checking getconf LFS_CFLAGS: OK
Checking for large file support without additional flags: OK
Checking for working strptime: OK
Checking for HAVE_SHARED_MMAP: OK
Checking for HAVE_MREMAP: OK
Checking for HAVE_INCOHERENT_MMAP: OK
Checking getconf large file support flags work: OK
'''

EXITKILL_BEFORE = "\t\t\t\tPTRACE_O_TRACEEXIT);"
EXITKILL_AFTER = "\t\t\t\tPTRACE_O_TRACEEXIT\t|\n\t\t\t\tPTRACE_O_EXITKILL);"
EXITKILL_PATCH = '''--- a/src/tracee/event.c
+++ b/src/tracee/event.c
@@ -531,7 +531,8 @@
 				PTRACE_O_TRACEVFORKDONE	|
 				PTRACE_O_TRACEEXEC	|
 				PTRACE_O_TRACECLONE	|
-				PTRACE_O_TRACEEXIT);
+				PTRACE_O_TRACEEXIT	|
+				PTRACE_O_EXITKILL);
 
 			/* Distinguish some events from others and
 			 * automatically trace each new process with
'''


def sha256(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def acquire(source, directory, offline):
    path = directory / source["file"]
    if path.exists():
        if sha256(path) != source["sha256"]:
            raise RuntimeError(f"cached source hash mismatch; preserve and inspect: {path}")
        return path
    if offline:
        raise RuntimeError(f"offline source missing: {path}")
    temporary = path.with_suffix(path.suffix + ".partial")
    request = urllib.request.Request(source["url"], headers={"User-Agent": "GlassTerminal-ArchRuntimeBuilder/1"})
    try:
        with urllib.request.urlopen(request, timeout=40) as response, temporary.open("xb") as stream:
            size = 0
            while chunk := response.read(65536):
                size += len(chunk)
                if size > 8 * 1024 * 1024:
                    raise RuntimeError("pinned source exceeded 8 MiB acquisition limit")
                stream.write(chunk)
        if sha256(temporary) != source["sha256"]:
            raise RuntimeError(f"downloaded source hash mismatch: {source['file']}")
        temporary.replace(path)
    finally:
        temporary.unlink(missing_ok=True)
    return path


def extract(archive, parent):
    # Sources are hash pinned; still reject path escapes before extracting.
    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive) as container:
            for member in container.infolist():
                resolved = (parent / member.filename).resolve()
                if not resolved.is_relative_to(parent.resolve()):
                    raise RuntimeError("source ZIP contains an escaping path")
            container.extractall(parent)
    else:
        with tarfile.open(archive) as container:
            container.extractall(parent, filter="data")


def verify_sources(archives, workspace):
    """Bind reused build sources to archive bytes plus our single exact patch."""
    checked = 0
    for name, archive in archives.items():
        def compare(filename, expected):
            nonlocal checked
            if name == "proot" and filename.endswith("/src/tracee/event.c"):
                expected = expected.replace(EXITKILL_BEFORE.encode(), EXITKILL_AFTER.encode())
            path = workspace / filename
            if path.is_symlink() or not path.is_file() or path.read_bytes() != expected:
                raise RuntimeError(f"cached source differs from pinned archive/reviewed patch: {path}")
            checked += 1
        if archive.suffix == ".zip":
            with zipfile.ZipFile(archive) as container:
                for member in container.infolist():
                    if not member.is_dir():
                        compare(member.filename, container.read(member))
        else:
            with tarfile.open(archive) as container:
                for member in container.getmembers():
                    if member.isfile():
                        compare(member.name, container.extractfile(member).read())
    return {"schema": "glass.android.arch-runtime-source-readback.v1", "checked_regular_files": checked,
            "allowed_transformation": "patches/0001-android-tracee-exitkill.patch", "passed": True}


def inspect_elf(path, executable_type):
    data = path.read_bytes()
    if data[:6] != b"\x7fELF\x02\x01":
        raise RuntimeError(f"not a little-endian ELF64: {path}")
    kind, machine = struct.unpack_from("<HH", data, 16)
    if (kind, machine) != (executable_type, 183):
        raise RuntimeError(f"wrong ELF type/machine {kind}/{machine}: {path}")
    program_offset = struct.unpack_from("<Q", data, 32)[0]
    entry_size, count = struct.unpack_from("<HH", data, 54)
    segments, interpreter = [], None
    for index in range(count):
        item = struct.unpack_from("<IIQQQQQQ", data, program_offset + index * entry_size)
        kind_, flags, offset, address, _, filesz, _, alignment = item
        if kind_ == 1:
            if alignment < 16384 or offset % alignment != address % alignment or flags & 3 == 3:
                raise RuntimeError(f"invalid 16 KiB or W^X LOAD segment: {path}")
            segments.append({"offset": offset, "virtual_address": address, "alignment": alignment, "flags": flags})
        elif kind_ == 3:
            interpreter = data[offset:offset + filesz].rstrip(b"\0").decode()
        elif kind_ == 0x6474E551 and flags & 1:
            raise RuntimeError(f"executable GNU_STACK: {path}")
    if not segments:
        raise RuntimeError(f"ELF has no LOAD segments: {path}")
    if executable_type == 3 and interpreter != "/system/bin/linker64":
        raise RuntimeError(f"unexpected Android interpreter: {interpreter}")
    if executable_type == 2 and interpreter is not None:
        raise RuntimeError("freestanding loader must have no interpreter")
    return {"type": executable_type, "machine": "AArch64", "interpreter": interpreter, "load_segments": segments}


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    default = Path(__file__).resolve().parents[3] / "plyra-terminal-builds/glass-g1/android/arch-runtime-arm64"
    parser.add_argument("--output-dir", type=Path, default=default)
    parser.add_argument("--ndk", type=Path, default=Path.home() / "Library/Android/sdk/ndk/25.1.8937393")
    parser.add_argument("--python", type=Path, default=Path("/opt/homebrew/bin/python3.11"), help="Host Python for upstream waf (3.11 recommended)")
    parser.add_argument("--offline", action="store_true", help="Require all exact source archives in output-dir/sources")
    args = parser.parse_args()
    output, ndk = args.output_dir.resolve(), args.ndk.resolve()
    host = "darwin-x86_64" if platform.system() == "Darwin" else "linux-x86_64"
    toolbin = ndk / "toolchains/llvm/prebuilt" / host / "bin"
    compiler, ar = toolbin / "aarch64-linux-android26-clang", toolbin / "llvm-ar"
    for path in (compiler, ar, toolbin / "llvm-readelf", args.python):
        if not path.is_file():
            parser.error(f"required local tool missing: {path}")
    if "25.1.8937393" not in (ndk / "source.properties").read_text():
        parser.error("this reproducible runtime recipe requires NDK 25.1.8937393")
    output.mkdir(parents=True, exist_ok=True)
    with (output / ".build.lock").open("w") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            parser.error("another runtime build owns this output directory")
        build(args, output, ndk, toolbin, compiler, ar)


def build(args, output, ndk, toolbin, compiler, ar):
    started = time.monotonic()
    sources, workspace = output / "sources", output / "build"
    for path in (sources, workspace, output / "patches", output / "licenses", output / "relink"):
        path.mkdir(exist_ok=True)
    archives = {name: acquire(pin, sources, args.offline) for name, pin in SOURCES.items()}
    gpl3 = acquire(GPL3_TEXT, sources, args.offline)
    for name, archive in archives.items():
        dirname = {"proot": "proot-5.1.107.102", "talloc": "talloc-2.5.0"}[name]
        if not (workspace / dirname).is_dir():
            extract(archive, workspace)
    proot, talloc = [workspace / name for name in ("proot-5.1.107.102", "talloc-2.5.0")]
    event = proot / "src/tracee/event.c"
    text = event.read_text()
    if EXITKILL_BEFORE in text:
        if text.count(EXITKILL_BEFORE) != 1:
            raise RuntimeError("EXITKILL patch preimage ambiguous")
        event.write_text(text.replace(EXITKILL_BEFORE, EXITKILL_AFTER))
    elif EXITKILL_AFTER not in text:
        raise RuntimeError("EXITKILL patch preimage does not match pinned source")
    (output / "patches/0001-android-tracee-exitkill.patch").write_text(EXITKILL_PATCH)
    (output / "source-readback.json").write_text(json.dumps(verify_sources(archives, workspace), indent=2) + "\n")
    (talloc / "cross-answers.txt").write_text(CROSS_ANSWERS)
    environment = os.environ.copy()
    for name in ("CFLAGS", "CPPFLAGS", "LDFLAGS", "LD_PRELOAD", "CPATH", "LIBRARY_PATH", "PKG_CONFIG_PATH", "C_INCLUDE_PATH"):
        environment.pop(name, None)
    common = ["-O2", "-g0", "-fPIC", f"-ffile-prefix-map={workspace}=/glass-runtime"]
    environment.update(CC=str(compiler), AR=str(ar), RANLIB=str(toolbin / "llvm-ranlib"), LD=str(compiler),
                       PYTHON=str(args.python), PYTHONHASHSEED="1", SOURCE_DATE_EPOCH="1767225600",
                       CFLAGS=" ".join(common), LDFLAGS="-Wl,-z,max-page-size=16384", JOBS="1", LC_ALL="C")
    commands = []
    with (output / "build.log").open("w") as log:
        def run(argv, cwd=workspace, env=environment, timeout=240):
            argv = list(map(str, argv)); commands.append({"argv": argv, "cwd": str(cwd)})
            print("+ " + shlex.join(argv), flush=True)
            log.write("+ " + shlex.join(argv) + "\n"); log.flush()
            subprocess.run(argv, cwd=cwd, env=env, stdout=log, stderr=subprocess.STDOUT, check=True, timeout=timeout)
        prefix = workspace / "prefix"
        configuration = {"CC": str(compiler), "compiler_sha256": sha256(compiler.resolve()), "CFLAGS": common, "answers": CROSS_ANSWERS}
        config_marker = talloc / ".glass-config.json"
        if not config_marker.exists() or json.loads(config_marker.read_text()) != configuration:
            run(["sh", "configure", f"--prefix={prefix}", "--disable-rpath", "--disable-python", "--cross-compile", "--cross-answers=cross-answers.txt"], cwd=talloc)
            config_marker.write_text(json.dumps(configuration, sort_keys=True))
        run([args.python, "buildtools/bin/waf", "build", "--targets=talloc", "-j1"], cwd=talloc)
        talloc_objects = sorted((talloc / "bin/default").glob("talloc*.o"))
        # Android API26 lacks memset_explicit. Use upstream's configured
        # libreplace implementation, exactly as the normal talloc target does.
        replacements = sorted((talloc / "bin/default/lib/replace").glob("*.1.o"))
        if not talloc_objects:
            raise RuntimeError("upstream talloc build produced no expected objects")
        lib = workspace / "lib"; lib.mkdir(exist_ok=True)
        run([ar, "rcsD", lib / "libtalloc.a", *talloc_objects, *replacements])
        config_header = workspace / "glass-build-config.h"
        header_text = f'#define ARG_MAX 131072\n#define VERSION "{SOURCES["proot"]["version"]}"\n#define PROOT_UNBUNDLE_LOADER "/__glass_loader_env_required__"\n'
        if not config_header.exists() or config_header.read_text() != header_text:
            config_header.write_text(header_text)
        cppflags = shlex.join(["-include", str(config_header), f"-I{talloc}", "-D_FILE_OFFSET_BITS=64", "-D_GNU_SOURCE", "-I.", f"-I{proot / 'src'}"])
        linkflags = f"-L{lib} -ltalloc -Wl,-z,noexecstack,-z,max-page-size=16384"
        make = ["make", "-C", proot / "src", "-j1", "V=1", f"CC={compiler}", f"AR={ar}",
             f"OBJCOPY={toolbin / 'llvm-objcopy'}", f"OBJDUMP={toolbin / 'llvm-objdump'}", f"STRIP={toolbin / 'llvm-strip'}",
             "GIT=false", f"CFLAGS={shlex.join(common)}", f"CPPFLAGS={cppflags}", f"LDFLAGS={linkflags}",
             "PROOT_UNBUNDLE_LOADER=/__glass_loader_env_required__", "HAS_LOADER_32BIT=",
             "LOADER_LDFLAGS=-static -nostdlib -Wl,--build-id=none,-Ttext=0x5400000000,-z,noexecstack,-z,max-page-size=16384"]
        make_marker = proot / ".glass-make.json"
        recipe = list(map(str, make))
        if not make_marker.exists() or json.loads(make_marker.read_text()) != recipe:
            # Only generated files in this builder's pinned-source tree; no
            # shared project target/cache or user files are removed.
            run([*make, "clean"])
            make_marker.write_text(json.dumps(recipe))
        run([*make, "loader/loader"])
        symbols = subprocess.check_output([toolbin / "llvm-nm", proot / "src/loader/loader"], text=True)
        addresses = {name: int(address, 16) for address, _, name in (line.split() for line in symbols.splitlines() if len(line.split()) == 3)}
        offset = addresses["pokedata_workaround"] - addresses["_start"]
        if not 0 < offset < (proot / "src/loader/loader").stat().st_size:
            raise RuntimeError("invalid loader pokedata workaround offset")
        # Upstream uses GNU readelf + GNU awk strtonum; neither exists in a
        # stock macOS shell. Generate the same constant from NDK llvm-nm.
        loader_info = proot / "src/loader/loader-info.c"
        loader_info.write_text(f'#include <unistd.h>\nconst ssize_t offset_to_pokedata_workaround={offset};\n')
        run([*make, "proot"])
        binaries = output / "jniLibs/arm64-v8a"; binaries.mkdir(parents=True, exist_ok=True)
        original = {"libglass_proot.so": proot / "src/proot", "libglass_proot_loader.so": proot / "src/loader/loader"}
        files = {}
        for name, source in original.items():
            target = binaries / name
            shutil.copyfile(source, target); target.chmod(0o755)
            run([toolbin / "llvm-strip", "--strip-unneeded", target])
            check = inspect_elf(target, 3 if name == "libglass_proot.so" else 2)
            readelf = subprocess.check_output([toolbin / "llvm-readelf", "-lWd", target], text=True)
            needed = re.findall(r"Shared library: \[([^]]+)\]", readelf)
            if not set(needed) <= {"libc.so", "libm.so", "libdl.so", "liblog.so", "libandroid.so"}:
                raise RuntimeError(f"unexpected dynamic dependency: {needed}")
            if name.endswith("_loader.so") and needed:
                raise RuntimeError("loader acquired a dynamic dependency")
            (output / (name + ".readelf.txt")).write_text(readelf)
            files[name] = {"path": str(target.relative_to(output)), "sha256": sha256(target), "bytes": target.stat().st_size, "needed": needed, **check}
        shutil.copyfile(proot / "COPYING", output / "licenses/PRoot-COPYING")
        shutil.copyfile(talloc / "LICENSE", output / "licenses/talloc-LGPL-3.0.txt")
        shutil.copyfile(gpl3, output / "licenses/GPL-3.0.txt")
        shutil.copyfile(talloc / "talloc.c", output / "licenses/talloc-source-notice.c")
        shutil.copyfile(Path(__file__), sources / "build_arch_runtime.py")
        for item in sorted((proot / "src").rglob("*.o")):
            relative = item.relative_to(proot / "src")
            target = output / "relink" / relative
            target.parent.mkdir(parents=True, exist_ok=True); shutil.copyfile(item, target)
        shutil.copyfile(lib / "libtalloc.a", output / "relink/libtalloc.a")
    (output / "BUILDING.txt").write_text(__doc__ + "\nRecorded commands/environment: runtime-manifest.json. Downloaded archives are exact corresponding source.\n")
    notice = """Glass Terminal optional Arch userspace runtime\n\nTermux PRoot 5.1.107.102 (GPL-2.0-or-later), statically combined with talloc 2.5.0\n(LGPL-3.0-or-later). This external runtime is conveyed under GPL-3.0-or-later.\nCopyright and full terms are in licenses/. No libandroid-shmem is linked: PRoot\nuses its own per-session SysVIPC/memfd implementation when explicitly enabled.\nComplete pinned upstream source archives, this build script, exact build commands,\nthe EXITKILL patch, and relink inputs accompany the runtime in sources/, patches/,\nrelink/ and runtime-manifest.json. Retain and distribute these with the binaries.\n\nlibglass_proot.so is an executable, not JNI. PROOT_LOADER must name the adjacent\nlibglass_proot_loader.so in Android nativeLibraryDir. Use private PROOT_TMP_DIR\nand --kill-on-exit. No root privileges, VM, kernel boot or 32-bit guest support.\nCross-build checks do not establish Android guest execution acceptance.\n"""
    (output / "NOTICE.txt").write_text(notice)
    shutil.copyfile(output / "NOTICE.txt", output / "jniLibs/arm64-v8a/NOTICE.txt")
    manifest = {"schema": "glass.android.arch-runtime.v1", "architecture": "arm64-v8a", "min_api": 26,
                "ndk": {"version": "25.1.8937393", "compiler_sha256": sha256(compiler.resolve())},
                "sources": SOURCES, "patches": [{"file": "patches/0001-android-tracee-exitkill.patch", "sha256": sha256(output / "patches/0001-android-tracee-exitkill.patch"), "reason": "Kill traced guest processes if tracer exits, including SIGKILL. Both seccomp and default ptrace option paths include EXITKILL; no silent fallback without it."}],
                "files": files, "source_readback": {"path": "source-readback.json", "sha256": sha256(output / "source-readback.json")}, "license_text_source": GPL3_TEXT, "corresponding_source_directory": "sources", "notice": "NOTICE.txt", "licenses": "licenses", "commands": commands, "build_environment": {key: environment[key] for key in ("CC", "AR", "RANLIB", "LD", "PYTHON", "PYTHONHASHSEED", "SOURCE_DATE_EPOCH", "CFLAGS", "LDFLAGS", "JOBS", "LC_ALL")},
                "runtime_environment": {"PROOT_LOADER": "<nativeLibraryDir>/libglass_proot_loader.so", "PROOT_TMP_DIR": "<unique app-private per-session directory>"},
                "cleanup": {"argv": "--kill-on-exit", "graceful_tracer_signal": "SIGQUIT", "hard_death_guard": "PTRACE_O_EXITKILL; requires supporting kernel; unsupported options fail closed"},
                "execution_status": "Cross-built and ELF-inspected only. Guest exec, API34 compatibility and teardown require physical Android acceptance.",
                "elapsed_seconds": round(time.monotonic() - started, 3)}
    (output / "runtime-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(json.dumps({"manifest": str(output / "runtime-manifest.json"), "files": files}, indent=2))


if __name__ == "__main__":
    main()

# FVPROV1 22921 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiYWYyMjM1NDFhODgwODliYjc4MGQxMzQ0YmE2ZmJmOGZlNTc2YWRhYzE3OGQ2MjViOGJjODUwY2IwZWI2M2NjOCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMzM4MzQwZTIzOTk5YjYxMzgxY2NhMTNkMThhOWFmYzU3N2Q3MjFjMTFmMzc4OGRjZDdhNTc1M2UwYzVkMjExNTE4MzA1YWRhYjhmN2JhYTFlMzQ4MDU3NTI3MmExZWNmZjNlZjQwMTE4MDk0MTA4NjcxOTQxY2QzOTE0MjI3MGEifV19XQ== FVPROV1-END
