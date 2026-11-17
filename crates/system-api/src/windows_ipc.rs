// SPDX-License-Identifier: GPL-3.0-only
use super::windows_identity::{self, Handle, Principal};
use anyhow::{ensure, Result};
use std::{
    ffi::OsStr,
    io::{self, Read, Write},
    ptr,
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::{
        GetLastError, LocalFree, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED,
        GENERIC_READ, GENERIC_WRITE, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Security::{
        Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
        RevertToSelf, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    },
    Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
        OPEN_EXISTING, PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
    },
    System::{
        Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId,
            GetNamedPipeServerProcessId, ImpersonateNamedPipeClient, PIPE_READMODE_BYTE,
            PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
        },
        Threading::{
            CreateEventW, GetCurrentThread, OpenThreadToken, WaitForSingleObject, INFINITE,
        },
        IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
    },
};

pub struct PipeStream {
    handle: Handle,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
}

struct SecurityDescriptor(*mut std::ffi::c_void);

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

impl PipeStream {
    pub fn create(name: &str, caller_sid: &str) -> Result<Self> {
        ensure!(valid_name(name), "invalid private broker pipe name");
        ensure!(
            caller_sid.starts_with("S-1-")
                && caller_sid[4..]
                    .bytes()
                    .all(|b| b.is_ascii_digit() || b == b'-'),
            "invalid caller SID"
        );
        let name = windows_identity::wide(OsStr::new(name))?;
        let descriptor = windows_identity::wide(OsStr::new(&format!(
            "D:P(A;;GA;;;{caller_sid})(A;;GA;;;BA)(A;;GA;;;SY)"
        )))?;
        unsafe {
            let mut security = ptr::null_mut();
            ensure!(
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    descriptor.as_ptr(),
                    SDDL_REVISION_1,
                    &mut security,
                    ptr::null_mut()
                ) != 0,
                "cannot create pipe security descriptor ({})",
                io::Error::last_os_error()
            );
            let security = SecurityDescriptor(security);
            let attributes = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: security.0,
                bInheritHandle: 0,
            };
            let handle = Handle::from_raw(CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                65_536,
                65_536,
                5000,
                &attributes,
            ))?;
            Ok(Self {
                handle,
                read_timeout: Some(Duration::from_secs(5)),
                write_timeout: Some(Duration::from_secs(5)),
            })
        }
    }

    pub fn connect(name: &str) -> Result<Self> {
        ensure!(valid_name(name), "invalid private broker pipe name");
        let name = windows_identity::wide(OsStr::new(name))?;
        unsafe {
            let handle = Handle::from_raw(CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                ptr::null_mut(),
            ))?;
            Ok(Self {
                handle,
                read_timeout: Some(Duration::from_secs(5)),
                write_timeout: Some(Duration::from_secs(5)),
            })
        }
    }

    pub fn accept(&self, timeout: Duration) -> Result<()> {
        let event = unsafe { Handle::from_raw(CreateEventW(ptr::null(), 1, 0, ptr::null()))? };
        let mut operation: OVERLAPPED = unsafe { std::mem::zeroed() };
        operation.hEvent = event.raw();
        unsafe {
            if ConnectNamedPipe(self.raw(), &mut operation) != 0 {
                return Ok(());
            }
            match GetLastError() {
                ERROR_PIPE_CONNECTED => Ok(()),
                ERROR_IO_PENDING => {
                    self.finish(&mut operation, Some(timeout))?;
                    Ok(())
                }
                _ => Err(io::Error::last_os_error().into()),
            }
        }
    }

    pub fn raw(&self) -> HANDLE {
        self.handle.raw()
    }

    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) {
        self.read_timeout = timeout;
    }

    pub fn client_pid(&self) -> Result<u32> {
        let mut pid = 0;
        ensure!(
            unsafe { GetNamedPipeClientProcessId(self.raw(), &mut pid) } != 0 && pid > 4,
            "cannot authenticate named-pipe client PID ({})",
            io::Error::last_os_error()
        );
        Ok(pid)
    }

    pub fn server_pid(&self) -> Result<u32> {
        let mut pid = 0;
        ensure!(
            unsafe { GetNamedPipeServerProcessId(self.raw(), &mut pid) } != 0 && pid > 4,
            "cannot authenticate named-pipe server PID ({})",
            io::Error::last_os_error()
        );
        Ok(pid)
    }

    /// Call after reading the client's hello. The client opens with identification SQOS.
    pub fn client_principal(&self) -> Result<Principal> {
        unsafe {
            ensure!(
                ImpersonateNamedPipeClient(self.raw()) != 0,
                "cannot identify named-pipe client ({})",
                io::Error::last_os_error()
            );
            let result = (|| {
                let mut token = ptr::null_mut();
                ensure!(
                    OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) != 0,
                    "cannot query pipe client token ({})",
                    io::Error::last_os_error()
                );
                let token = Handle::from_raw(token)?;
                windows_identity::token_principal(token.raw())
            })();
            // Continuing on the UI thread with another token is never acceptable.
            if RevertToSelf() == 0 {
                std::process::abort();
            }
            result
        }
    }

    pub fn cancel(&self) {
        unsafe {
            CancelIoEx(self.raw(), ptr::null());
        }
    }

    fn finish(&self, operation: &mut OVERLAPPED, timeout: Option<Duration>) -> io::Result<usize> {
        let milliseconds = timeout.map_or(INFINITE, |duration| {
            duration.as_millis().min(u128::from(INFINITE - 1)) as u32
        });
        unsafe {
            let status = WaitForSingleObject(operation.hEvent, milliseconds);
            if status != WAIT_OBJECT_0 {
                let wait_error = io::Error::last_os_error();
                CancelIoEx(self.raw(), operation);
                let mut ignored = 0;
                // OVERLAPPED and its buffer stay alive until cancellation completes.
                GetOverlappedResult(self.raw(), operation, &mut ignored, 1);
                return Err(if status == WAIT_TIMEOUT {
                    io::Error::new(io::ErrorKind::TimedOut, "broker pipe I/O deadline expired")
                } else {
                    wait_error
                });
            }
            let mut count = 0;
            if GetOverlappedResult(self.raw(), operation, &mut count, 0) != 0 {
                Ok(count as usize)
            } else {
                Err(io::Error::last_os_error())
            }
        }
    }
}

impl Read for PipeStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let event = unsafe { Handle::from_raw(CreateEventW(ptr::null(), 1, 0, ptr::null())) }
            .map_err(io::Error::other)?;
        let mut operation: OVERLAPPED = unsafe { std::mem::zeroed() };
        operation.hEvent = event.raw();
        let mut count = 0;
        unsafe {
            if ReadFile(
                self.raw(),
                buffer.as_mut_ptr(),
                buffer.len().min(u32::MAX as usize) as u32,
                &mut count,
                &mut operation,
            ) != 0
            {
                return Ok(count as usize);
            }
            match GetLastError() {
                ERROR_BROKEN_PIPE => Ok(0),
                ERROR_IO_PENDING => match self.finish(&mut operation, self.read_timeout) {
                    Err(error) if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) => Ok(0),
                    result => result,
                },
                _ => Err(io::Error::last_os_error()),
            }
        }
    }
}

impl Write for PipeStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let event = unsafe { Handle::from_raw(CreateEventW(ptr::null(), 1, 0, ptr::null())) }
            .map_err(io::Error::other)?;
        let mut operation: OVERLAPPED = unsafe { std::mem::zeroed() };
        operation.hEvent = event.raw();
        let mut count = 0;
        unsafe {
            if WriteFile(
                self.raw(),
                buffer.as_ptr(),
                buffer.len().min(u32::MAX as usize) as u32,
                &mut count,
                &mut operation,
            ) != 0
            {
                return Ok(count as usize);
            }
            match GetLastError() {
                ERROR_IO_PENDING => self.finish(&mut operation, self.write_timeout),
                _ => Err(io::Error::last_os_error()),
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn valid_name(name: &str) -> bool {
    name.strip_prefix(r"\\.\pipe\GlassTerminal.")
        .is_some_and(|suffix| {
            (32..=128).contains(&suffix.len())
                && suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() || byte == b'.')
        })
}

#[cfg(test)]
mod tests {
    #[test]
    fn pipe_names_are_local_and_limited_to_the_private_namespace() {
        assert!(super::valid_name(
            r"\\.\pipe\GlassTerminal.1234.0123456789abcdef0123456789abcdef"
        ));
        assert!(!super::valid_name(
            r"\\server\pipe\GlassTerminal.0123456789abcdef0123456789abcdef"
        ));
        assert!(!super::valid_name(
            r"\\.\pipe\other.0123456789abcdef0123456789abcdef"
        ));
        assert!(!super::valid_name(r"\\.\pipe\GlassTerminal.1"));
    }
}

// FVPROV1 11149 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiMTdhMDFlOGM2NmI1NjQ4ZTY1ODI2YTA2NDIyYzVjYWM1N2Y0ZGJkZTAzNzc1ZjFiMjExMDg3ZGQzNzRlOTk0NiIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiYjJmNzIzOGEwOTI1MDZhY2VhZGZlYzk4MjIxYjg1MWQ3YWNmYzEzOTYyYjUzNGFjNDMxZDlkMjhhYmQwMjM2NjYyNDUyMzdmMDI1OTJhMzY5ZDk4MmQ1YmNmMzc3YzRlN2VkYjVkN2VlNmY3ZjA4NGQyNWIzZWYyOGQzNTBkMGUifV19XQ== FVPROV1-END
