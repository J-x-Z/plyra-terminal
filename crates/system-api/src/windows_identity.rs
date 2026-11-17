// SPDX-License-Identifier: GPL-3.0-only
use crate::ProcessIdentity;
use anyhow::{ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::{
    ffi::{OsStr, OsString},
    fs::{File, OpenOptions},
    io::Read,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::{Path, PathBuf},
    ptr,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, LocalFree, FILETIME, HANDLE, INVALID_HANDLE_VALUE},
    Security::{
        Authorization::ConvertSidToStringSidW, GetTokenInformation, IsValidSid, TokenElevation,
        TokenSessionId, TokenUser, TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER,
    },
    Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
    },
    System::Threading::{
        GetCurrentProcess, GetProcessTimes, IsProcessCritical, OpenProcess, OpenProcessToken,
        QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        PROCESS_TERMINATE,
    },
};

pub struct Handle(HANDLE);

impl Handle {
    /// Takes ownership of a successfully returned Win32 kernel handle.
    pub unsafe fn from_raw(handle: HANDLE) -> Result<Self> {
        ensure!(
            !handle.is_null() && handle != INVALID_HANDLE_VALUE,
            "{}",
            std::io::Error::last_os_error()
        );
        Ok(Self(handle))
    }

    pub fn raw(&self) -> HANDLE {
        self.0
    }
}

// Kernel handles can move between threads. Shared I/O is serialized by the transport.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[derive(Clone, Debug)]
pub struct Principal {
    pub sid: String,
    pub elevated: bool,
    pub session_id: u32,
}

pub fn wide(value: &OsStr) -> Result<Vec<u16>> {
    let mut value: Vec<_> = value.encode_wide().collect();
    ensure!(
        !value.contains(&0),
        "Windows argument contains a null character"
    );
    value.push(0);
    Ok(value)
}

pub unsafe fn sid_string(sid: windows_sys::Win32::Security::PSID) -> Result<String> {
    ensure!(IsValidSid(sid) != 0, "Windows returned an invalid SID");
    let mut text = ptr::null_mut();
    ensure!(
        ConvertSidToStringSidW(sid, &mut text) != 0,
        "cannot format token SID ({})",
        std::io::Error::last_os_error()
    );
    let mut length = 0;
    while *text.add(length) != 0 {
        length += 1;
    }
    let result = String::from_utf16(std::slice::from_raw_parts(text, length));
    LocalFree(text.cast());
    Ok(result.context("token SID is not valid UTF-16")?)
}

pub fn token_principal(token: HANDLE) -> Result<Principal> {
    unsafe {
        let mut bytes = 0;
        GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut bytes);
        ensure!(
            (std::mem::size_of::<TOKEN_USER>() as u32..=65_536).contains(&bytes),
            "invalid token user size"
        );
        // TOKEN_USER contains a pointer and must be pointer-aligned.
        let mut user = vec![0_usize; (bytes as usize).div_ceil(std::mem::size_of::<usize>())];
        ensure!(
            GetTokenInformation(
                token,
                TokenUser,
                user.as_mut_ptr().cast(),
                bytes,
                &mut bytes
            ) != 0,
            "cannot query token user ({})",
            std::io::Error::last_os_error()
        );
        let sid = sid_string((*(user.as_ptr().cast::<TOKEN_USER>())).User.Sid)?;
        let mut elevation: TOKEN_ELEVATION = std::mem::zeroed();
        ensure!(
            GetTokenInformation(
                token,
                TokenElevation,
                (&mut elevation as *mut TOKEN_ELEVATION).cast(),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut bytes
            ) != 0,
            "cannot query token elevation ({})",
            std::io::Error::last_os_error()
        );
        let mut session_id = 0_u32;
        ensure!(
            GetTokenInformation(
                token,
                TokenSessionId,
                (&mut session_id as *mut u32).cast(),
                std::mem::size_of::<u32>() as u32,
                &mut bytes
            ) != 0,
            "cannot query token session ({})",
            std::io::Error::last_os_error()
        );
        Ok(Principal {
            sid,
            elevated: elevation.TokenIsElevated != 0,
            session_id,
        })
    }
}

pub fn process_principal(process: HANDLE) -> Result<Principal> {
    unsafe {
        let mut token = ptr::null_mut();
        ensure!(
            OpenProcessToken(process, TOKEN_QUERY, &mut token) != 0,
            "cannot open process token ({})",
            std::io::Error::last_os_error()
        );
        let token = Handle::from_raw(token)?;
        token_principal(token.raw())
    }
}

pub fn current_principal() -> Result<Principal> {
    process_principal(unsafe { GetCurrentProcess() })
}

fn image_path(handle: HANDLE) -> Result<PathBuf> {
    let mut path = vec![0_u16; 32_768];
    let mut length = path.len() as u32;
    ensure!(
        unsafe { QueryFullProcessImageNameW(handle, 0, path.as_mut_ptr(), &mut length) } != 0,
        "cannot query process image ({})",
        std::io::Error::last_os_error()
    );
    Ok(PathBuf::from(OsString::from_wide(&path[..length as usize])))
}

pub fn process_image(pid: u32) -> Result<PathBuf> {
    let handle =
        unsafe { Handle::from_raw(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid))? };
    image_path(handle.raw())
}

fn creation_time(handle: HANDLE) -> Result<u64> {
    unsafe {
        let mut creation: FILETIME = std::mem::zeroed();
        let mut exit: FILETIME = std::mem::zeroed();
        let mut kernel: FILETIME = std::mem::zeroed();
        let mut user: FILETIME = std::mem::zeroed();
        ensure!(
            GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) != 0,
            "cannot query process creation time ({})",
            std::io::Error::last_os_error()
        );
        Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
    }
}

pub fn process_start_time(pid: u32) -> Result<u64> {
    let handle =
        unsafe { Handle::from_raw(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid))? };
    creation_time(handle.raw())
}

pub struct Process {
    handle: Handle,
    pub identity: ProcessIdentity,
    pub principal: Principal,
}

impl Process {
    pub fn open(pid: u32, terminate: bool) -> Result<Self> {
        ensure!(
            pid > 4,
            "Windows kernel and system process targets are protected"
        );
        unsafe {
            let access = PROCESS_QUERY_LIMITED_INFORMATION
                | PROCESS_SYNCHRONIZE
                | if terminate { PROCESS_TERMINATE } else { 0 };
            let handle = Handle::from_raw(OpenProcess(access, 0, pid))?;
            let principal = process_principal(handle.raw())?;
            let exe = image_path(handle.raw())?.to_string_lossy().into_owned();
            let start_time = creation_time(handle.raw())?;
            let identity = ProcessIdentity {
                pid,
                start_time,
                exe,
                uid: principal.sid.clone(),
            };
            Ok(Self {
                handle,
                identity,
                principal,
            })
        }
    }

    pub fn raw(&self) -> HANDLE {
        self.handle.raw()
    }

    pub fn ensure_not_critical(&self) -> Result<()> {
        let mut critical = 0;
        ensure!(
            unsafe { IsProcessCritical(self.raw(), &mut critical) } != 0,
            "cannot determine whether the process is critical ({})",
            std::io::Error::last_os_error()
        );
        ensure!(
            critical == 0,
            "Windows critical process targets are protected"
        );
        Ok(())
    }
}

pub struct PinnedImage {
    pub path: PathBuf,
    pub sha256: String,
    _file: File,
}

impl PinnedImage {
    pub fn raw(&self) -> HANDLE {
        self._file.as_raw_handle()
    }

    pub fn open(path: &Path) -> Result<Self> {
        ensure!(
            path.is_absolute(),
            "Windows executable path must be absolute"
        );
        let mut file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            "executable must be a regular non-reparse file"
        );
        let mut hasher = Sha256::new();
        let mut chunk = [0_u8; 65_536];
        loop {
            let count = file.read(&mut chunk)?;
            if count == 0 {
                break;
            }
            hasher.update(&chunk[..count]);
        }
        Ok(Self {
            path: path.to_path_buf(),
            sha256: format!("{:x}", hasher.finalize()),
            _file: file,
        })
    }
}

pub fn process_identity(pid: u32) -> Result<ProcessIdentity> {
    Ok(Process::open(pid, false)?.identity)
}

// FVPROV1 9340 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiMjYwMTU2ODg5OWRhM2I2ZWUyMmY0YWE0ZGYxMTBhMjI4ODg1YWQwMmU2NDRkMzc3Mzk5ZGI4Y2UyNzFkNWIwNSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMjkyOGQ4NjNiYWU1ZDQyNzA1MGJhNjUxNDQ0Y2I4MjE0NWYxMGVmYTE3Yzc2ZTcyODA2ZmE3Y2ZhNmJiMDZhYzFmMzE4YTlkMmZlOGM1MDU3M2Y2YzMwNGRkYzAyYzQ1NzI0MDk3YjcxZDdkYTA4YzYxZjJlNDU5YTI4MTRiMDIifV19XQ== FVPROV1-END
