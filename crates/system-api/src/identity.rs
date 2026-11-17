// SPDX-License-Identifier: GPL-3.0-only
use anyhow::Result;
#[cfg(target_os = "macos")]
use anyhow::{ensure, Context};
use std::path::Path;
#[cfg(target_os = "macos")]
use std::time::Duration;

pub fn code_hash_for_path(path: &Path) -> Result<String> {
    #[cfg(target_os = "macos")]
    {
        let path = path.to_str().context("signed code path is not UTF-8")?;
        let verify = crate::bounded_command(
            "/usr/bin/codesign",
            &["--verify".into(), "--strict".into(), path.into()],
            Duration::from_secs(3),
            65_536,
        )?;
        ensure!(
            verify.success,
            "broker code signature verification failed: {}",
            verify.stderr
        );
        let info = crate::bounded_command(
            "/usr/bin/codesign",
            &["-d".into(), "--verbose=4".into(), path.into()],
            Duration::from_secs(3),
            65_536,
        )?;
        let text = format!("{}{}", info.stdout, info.stderr);
        text.lines()
            .find_map(|line| line.strip_prefix("CDHash="))
            .filter(|hash| {
                (40..=64).contains(&hash.len()) && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
            .map(str::to_owned)
            .context("code has no valid CodeDirectory hash")
    }
    #[cfg(target_os = "linux")]
    {
        crate::linux_admin::file_hash(path)
    }
    #[cfg(target_os = "android")]
    {
        crate::android_root::file_hash(path)
    }
    #[cfg(windows)]
    {
        Ok(crate::windows::windows_identity::PinnedImage::open(path)?.sha256)
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "linux",
        target_os = "android",
        windows
    )))]
    {
        let _ = path;
        anyhow::bail!("native code signing identity is implemented for macOS")
    }
}

pub fn running_code_hash(pid: u32) -> Result<String> {
    #[cfg(target_os = "macos")]
    unsafe {
        use core_foundation_sys::{
            base::{CFGetTypeID, CFRelease},
            data::{CFDataGetBytePtr, CFDataGetLength, CFDataGetTypeID},
            dictionary::{
                kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFDictionaryCreate,
                CFDictionaryGetValue,
            },
            number::{kCFNumberSInt32Type, CFNumberCreate},
        };
        use std::ffi::c_void;
        #[link(name = "Security", kind = "framework")]
        extern "C" {
            static kSecGuestAttributePid: *const c_void;
            static kSecCodeInfoUnique: *const c_void;
            fn SecCodeCopyGuestWithAttributes(
                host: *const c_void,
                attributes: *const c_void,
                flags: u32,
                code: *mut *const c_void,
            ) -> i32;
            fn SecCodeCopySelf(flags: u32, code: *mut *const c_void) -> i32;
            fn SecCodeCopySigningInformation(
                code: *const c_void,
                flags: u32,
                info: *mut *const c_void,
            ) -> i32;
            fn SecCodeCheckValidity(
                code: *const c_void,
                flags: u32,
                requirement: *const c_void,
            ) -> i32;
        }
        struct Owned(*const c_void);
        impl Drop for Owned {
            fn drop(&mut self) {
                if !self.0.is_null() {
                    unsafe { CFRelease(self.0) };
                }
            }
        }
        let pid = i32::try_from(pid).context("process signing PID is out of range")?;
        ensure!(pid > 0, "process signing PID must be positive");
        let number = Owned(
            CFNumberCreate(
                std::ptr::null(),
                kCFNumberSInt32Type,
                (&pid as *const i32).cast(),
            )
            .cast(),
        );
        ensure!(
            !number.0.is_null(),
            "cannot allocate process signing attributes"
        );
        let key = kSecGuestAttributePid;
        let value = number.0;
        let attributes = Owned(
            CFDictionaryCreate(
                std::ptr::null(),
                &key,
                &value,
                1,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
            .cast(),
        );
        let mut code = std::ptr::null();
        let status = if pid as u32 == std::process::id() {
            SecCodeCopySelf(0, &mut code)
        } else {
            SecCodeCopyGuestWithAttributes(std::ptr::null(), attributes.0, 0, &mut code)
        };
        ensure!(
            status == 0 && !code.is_null(),
            "cannot inspect authenticated process {pid} code identity (OSStatus {status})"
        );
        let code = Owned(code);
        let status = SecCodeCheckValidity(code.0, 0, std::ptr::null());
        ensure!(
            status == 0,
            "authenticated process {pid} code signature is invalid (OSStatus {status})"
        );
        let mut info = std::ptr::null();
        let status = SecCodeCopySigningInformation(code.0, 1 << 1, &mut info);
        ensure!(
            status == 0 && !info.is_null(),
            "cannot read process {pid} CodeDirectory identity (OSStatus {status})"
        );
        let info = Owned(info);
        let data = CFDictionaryGetValue(info.0.cast(), kSecCodeInfoUnique);
        ensure!(
            !data.is_null() && CFGetTypeID(data) == CFDataGetTypeID(),
            "process has no CodeDirectory hash"
        );
        let length = CFDataGetLength(data.cast());
        ensure!(
            (20..=32).contains(&length),
            "unexpected CodeDirectory hash length"
        );
        Ok(
            std::slice::from_raw_parts(CFDataGetBytePtr(data.cast()), length as usize)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        )
    }
    #[cfg(target_os = "linux")]
    {
        crate::linux_admin::file_hash(&std::path::PathBuf::from(format!("/proc/{pid}/exe")))
    }
    #[cfg(target_os = "android")]
    {
        crate::android_root::file_hash(&std::path::PathBuf::from(format!("/proc/{pid}/exe")))
    }
    #[cfg(windows)]
    {
        code_hash_for_path(&crate::windows::windows_identity::process_image(pid)?)
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "linux",
        target_os = "android",
        windows
    )))]
    {
        let _ = pid;
        anyhow::bail!("running-code identity is implemented for macOS")
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn running_self_identity_matches_verified_file_on_worker_thread() {
        let expected = code_hash_for_path(&std::env::current_exe().unwrap()).unwrap();
        let actual = std::thread::spawn(|| running_code_hash(std::process::id()))
            .join()
            .unwrap()
            .unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn signing_identity_rejects_invalid_process_ids() {
        assert!(running_code_hash(0).is_err());
        assert!(running_code_hash(u32::MAX).is_err());
    }
}

// FVPROV1 7148 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiMWU3MDQxMDEyYThjNDgyM2MzYWZiMmJjZDM3YTg3MjZjN2Y1ZGZhMDJhNmQxNjFlOGZmY2M3NTYyZTljYTQ3NiIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiNzQ2YTkwNDE2YTAwZmZiMWRkOWI3MDM3MzAxNmExNWNkODRjYTY4ZGQ0MTIwZmVmYTJjYjgwZGE3NDM4YmU3MzgzNWI5NzZhMzQxNmZkOTA4MzY1ODE0OTFiMzlhZjg5MWE2YWI4MWFiN2E1YTc2Yzc2ZWY2OGEzYjNhMzE4MDUifV19XQ== FVPROV1-END
