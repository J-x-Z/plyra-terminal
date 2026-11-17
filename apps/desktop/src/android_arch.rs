// SPDX-License-Identifier: GPL-3.0-only
//! Launch the APK-packaged Arch Linux runtime through the existing PTY.
use anyhow::{Context, Result};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU8, Ordering},
        Mutex,
    },
};

const NO_BUNDLE: u8 = 0;
const BUNDLE_AVAILABLE: u8 = 1;
const PREPARING: u8 = 2;
const BUNDLE_FAILED: u8 = 3;
const AETHER_SHELL_LIBRARY: &str = "libaether_shell.so";
const SYSTEM_BRIDGE_LIBRARY: &str = "libglass_system_bridge.so";

static BOOTSTRAP: AtomicU8 = AtomicU8::new(NO_BUNDLE);
static ANDROID_API: Mutex<String> = Mutex::new(String::new());

pub(crate) fn configure(bundled: bool, api_name: String) {
    if let Ok(mut name) = ANDROID_API.lock() {
        *name = api_name;
    }
    if bundled {
        let _ = BOOTSTRAP.compare_exchange(
            NO_BUNDLE,
            BUNDLE_AVAILABLE,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

pub(crate) fn bundled() -> bool {
    BOOTSTRAP.load(Ordering::Acquire) != NO_BUNDLE && available()
}

pub(crate) fn default_enabled() -> bool {
    bundled() && BOOTSTRAP.load(Ordering::Acquire) != BUNDLE_FAILED
}

pub(crate) fn prepare() -> Result<()> {
    if BOOTSTRAP
        .compare_exchange(
            BUNDLE_AVAILABLE,
            PREPARING,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
    {
        if let Err(error) = crate::android_input::prepare_bundled_arch() {
            finish(false);
            return Err(error);
        }
    }
    Ok(())
}

pub(crate) fn finish(success: bool) {
    BOOTSTRAP.store(
        if success {
            BUNDLE_AVAILABLE
        } else {
            BUNDLE_FAILED
        },
        Ordering::Release,
    );
}

pub(crate) fn available() -> bool {
    cfg!(target_arch = "aarch64")
        && system_api::android_root::native_library_dir().is_ok_and(|directory| {
            directory.join("libglass_proot.so").is_file()
                && directory.join("libglass_proot_loader.so").is_file()
        })
}

fn aether_in(directory: &Path) -> Result<PathBuf> {
    let shell = directory.join(AETHER_SHELL_LIBRARY);
    system_api::android_root::verify_packaged_binary(&shell, directory)
        .context("Aether Shell is unavailable in this APK")?;
    Ok(shell)
}

pub(crate) fn aether_shell() -> Result<PathBuf> {
    aether_in(system_api::android_root::native_library_dir()?)
}

pub(crate) fn system_bridge() -> Result<PathBuf> {
    let directory = system_api::android_root::native_library_dir()?;
    let bridge = directory.join(SYSTEM_BRIDGE_LIBRARY);
    system_api::android_root::verify_packaged_binary(&bridge, directory)
        .context("System API bridge is unavailable in this APK")?;
    Ok(bridge)
}

pub(crate) fn command() -> Result<Option<Vec<String>>> {
    anyhow::ensure!(available(), "Arch Linux is unavailable in this APK");
    let directory = system_api::android_root::native_library_dir()?;
    let base = system_api::android_root::files_dir()?.join("arch-linux");
    let rootfs = base.join("rootfs");
    if !rootfs.try_exists()? {
        return Ok(None);
    }
    let temporary = base.join("tmp");
    std::fs::create_dir_all(&temporary)?;
    let loader = directory.join("libglass_proot_loader.so");
    let proot = directory.join("libglass_proot.so");
    let api = directory.join("libglass_android_api.so");
    let mut command = vec![
        "/system/bin/env".into(),
        "-i".into(),
        "PATH=/usr/local/bin:/usr/bin:/bin".into(),
        "HOME=/root".into(),
        "SHELL=/bin/bash".into(),
        "TERM=xterm-256color".into(),
        "COLORTERM=truecolor".into(),
        "LANG=C.UTF-8".into(),
        "TMPDIR=/tmp".into(),
        "PROOT_NO_SECCOMP=1".into(),
        format!("PROOT_LOADER={}", loader.display()),
        format!("PROOT_TMP_DIR={}", temporary.display()),
    ];
    let api_name = ANDROID_API.lock().ok().map(|name| name.clone());
    let api_enabled = api_name
        .as_deref()
        .filter(|name| !name.is_empty() && api.is_file());
    if let Some(name) = api_enabled {
        command.push(format!("GLASS_ANDROID_API={name}"));
    }
    command.extend([
        // Toybox env reads any argument with '=' as an assignment until it
        // finds a command name. Android APK paths can contain '='.
        "/system/bin/sh".into(),
        "-c".into(),
        "exec \"$@\"".into(),
        "glass-arch".into(),
        proot.to_str().context("Invalid Arch runtime path")?.into(),
        "--kill-on-exit".into(),
        "--link2symlink".into(),
        "-0".into(),
        "-r".into(),
        rootfs.to_str().context("Invalid Arch rootfs path")?.into(),
        "-w".into(),
        "/root".into(),
    ]);
    for path in [
        "/dev/null",
        "/dev/zero",
        "/dev/random",
        "/dev/urandom",
        "/dev/tty",
        "/dev/pts",
        "/proc",
    ] {
        command.extend(["-b".into(), path.into()]);
    }
    if api_enabled.is_some() {
        command.extend([
            "-b".into(),
            format!("{}:/usr/local/bin/glass-android", api.display()),
        ]);
    }
    if let Ok(shell) = aether_in(directory) {
        command.extend([
            "-b".into(),
            format!("{}:/usr/local/bin/aether-shell", shell.display()),
        ]);
    }
    command.extend(["/bin/bash".into(), "--login".into(), "-i".into()]);
    Ok(Some(command))
}

// FVPROV1 5417 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNTJjYjc3YzBlN2MxNTNiNDdiMWI4MWUxZDFkYjkwODdhMjUyZGJhMjQ4M2FjOTBmN2I5YTRiNjQ1ZDc3MmMzOCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiZTJhMDhmM2U1ODUyMGYxNTExNTIyYTdlMDZhNWRhNmE1ZDcyNDIxYzg3MzA3NzBhYWI0NmNjNDZlOTBkZTY5YWQ1NjlmNDBmYjlmNWU3Y2RkMjVjNWIwNDk0MjAzMDVlZWU2MTRmZGE5MjExNDAwMzE0ZjhhMzhhYmJhNzIwMGMifV19XQ== FVPROV1-END
