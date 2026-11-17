// SPDX-License-Identifier: GPL-3.0-only
use super::windows_identity;
use crate::{ServiceIdentity, ServiceInfo};
use anyhow::{ensure, Result};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsStr,
    ptr, thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{GetLastError, ERROR_MORE_DATA},
    Security::LookupAccountNameW,
    System::Services::*,
};

struct ServiceHandle(SC_HANDLE);

impl ServiceHandle {
    unsafe fn new(handle: SC_HANDLE) -> Result<Self> {
        ensure!(
            !handle.is_null(),
            "SCM request failed ({})",
            std::io::Error::last_os_error()
        );
        Ok(Self(handle))
    }
}

impl Drop for ServiceHandle {
    fn drop(&mut self) {
        unsafe {
            CloseServiceHandle(self.0);
        }
    }
}

unsafe fn text(pointer: *const u16) -> Result<String> {
    if pointer.is_null() {
        return Ok(String::new());
    }
    let mut length = 0;
    while length < 32_768 && *pointer.add(length) != 0 {
        length += 1;
    }
    ensure!(length < 32_768, "SCM returned an oversized string");
    Ok(String::from_utf16(std::slice::from_raw_parts(
        pointer, length,
    ))?)
}

pub(crate) fn account_sid(account: &str) -> Result<String> {
    let account = if account.eq_ignore_ascii_case("LocalSystem") {
        "NT AUTHORITY\\SYSTEM"
    } else {
        account
    };
    let account = windows_identity::wide(OsStr::new(account))?;
    unsafe {
        let mut sid_size = 0;
        let mut domain_size = 0;
        let mut account_type = 0;
        LookupAccountNameW(
            ptr::null(),
            account.as_ptr(),
            ptr::null_mut(),
            &mut sid_size,
            ptr::null_mut(),
            &mut domain_size,
            &mut account_type,
        );
        ensure!(
            (8..=65_536).contains(&sid_size) && domain_size <= 32_768,
            "SCM service account SID is unavailable"
        );
        let mut sid = vec![0_usize; (sid_size as usize).div_ceil(std::mem::size_of::<usize>())];
        let mut domain = vec![0_u16; domain_size as usize];
        ensure!(
            LookupAccountNameW(
                ptr::null(),
                account.as_ptr(),
                sid.as_mut_ptr().cast(),
                &mut sid_size,
                domain.as_mut_ptr(),
                &mut domain_size,
                &mut account_type
            ) != 0,
            "cannot resolve SCM account SID ({})",
            std::io::Error::last_os_error()
        );
        windows_identity::sid_string(sid.as_mut_ptr().cast())
    }
}

fn status(service: &ServiceHandle) -> Result<SERVICE_STATUS_PROCESS> {
    unsafe {
        let mut value: SERVICE_STATUS_PROCESS = std::mem::zeroed();
        let mut needed = 0;
        ensure!(
            QueryServiceStatusEx(
                service.0,
                SC_STATUS_PROCESS_INFO,
                (&mut value as *mut SERVICE_STATUS_PROCESS).cast(),
                std::mem::size_of::<SERVICE_STATUS_PROCESS>() as u32,
                &mut needed
            ) != 0,
            "cannot query SCM state ({})",
            std::io::Error::last_os_error()
        );
        Ok(value)
    }
}

fn identity(service: &ServiceHandle, label: &str) -> Result<ServiceIdentity> {
    unsafe {
        let mut needed = 0;
        QueryServiceConfigW(service.0, ptr::null_mut(), 0, &mut needed);
        ensure!(
            (std::mem::size_of::<QUERY_SERVICE_CONFIGW>() as u32..=65_536).contains(&needed),
            "SCM configuration is unavailable or oversized"
        );
        let mut bytes = vec![0_usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
        ensure!(
            QueryServiceConfigW(service.0, bytes.as_mut_ptr().cast(), needed, &mut needed) != 0,
            "cannot read SCM configuration ({})",
            std::io::Error::last_os_error()
        );
        let config = &*bytes.as_ptr().cast::<QUERY_SERVICE_CONFIGW>();
        ensure!(
            config.dwServiceType & SERVICE_WIN32 != 0,
            "driver service targets are excluded"
        );
        let executable = text(config.lpBinaryPathName)?;
        let account = text(config.lpServiceStartName)?;
        let mut dependencies = Vec::new();
        let mut pointer = config.lpDependencies;
        let mut consumed = 0;
        while !pointer.is_null() && *pointer != 0 {
            let dependency = text(pointer)?;
            consumed += dependency.encode_utf16().count() + 1;
            ensure!(consumed <= 32_768, "SCM dependencies exceed their budget");
            pointer = pointer.add(dependency.encode_utf16().count() + 1);
            dependencies.push(dependency);
        }
        let configuration = serde_json::to_vec(&(
            label,
            config.dwServiceType,
            config.dwStartType,
            config.dwErrorControl,
            executable,
            &account,
            dependencies,
        ))?;
        let state = status(service)?;
        let pid = (state.dwProcessId > 4).then_some(state.dwProcessId);
        Ok(ServiceIdentity {
            label: label.into(),
            domain: "scm/local".into(),
            uid: u32::MAX,
            pid,
            process_start_time: pid.and_then(|pid| windows_identity::process_start_time(pid).ok()),
            owner_sid: Some(account_sid(&account)?),
            configuration_hash: Some(format!("{:x}", Sha256::digest(configuration))),
        })
    }
}

fn open(label: &str, access: u32) -> Result<ServiceHandle> {
    ensure!(
        !label.is_empty() && label.len() <= 256 && !label.chars().any(char::is_control),
        "invalid SCM service name"
    );
    let label = windows_identity::wide(OsStr::new(label))?;
    unsafe {
        let manager =
            ServiceHandle::new(OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_CONNECT))?;
        ServiceHandle::new(OpenServiceW(manager.0, label.as_ptr(), access))
    }
}

pub fn service_identity(label: &str) -> Result<ServiceIdentity> {
    identity(
        &open(label, SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS)?,
        label,
    )
}

pub fn services() -> Result<Vec<ServiceInfo>> {
    unsafe {
        let manager = ServiceHandle::new(OpenSCManagerW(
            ptr::null(),
            ptr::null(),
            SC_MANAGER_ENUMERATE_SERVICE,
        ))?;
        let mut capacity: usize = 256 * 1024;
        for _ in 0..4 {
            let mut bytes = vec![0_u64; capacity.div_ceil(std::mem::size_of::<u64>())];
            let mut needed = 0;
            let mut count = 0;
            let mut resume = 0;
            let result = EnumServicesStatusExW(
                manager.0,
                SC_ENUM_PROCESS_INFO,
                SERVICE_WIN32,
                SERVICE_STATE_ALL,
                bytes.as_mut_ptr().cast(),
                capacity as u32,
                &mut needed,
                &mut count,
                &mut resume,
                ptr::null(),
            );
            if result == 0 {
                ensure!(
                    GetLastError() == ERROR_MORE_DATA,
                    "cannot enumerate SCM services ({})",
                    std::io::Error::last_os_error()
                );
                capacity = capacity.saturating_add(needed as usize).max(capacity * 2);
                ensure!(
                    capacity <= 2 * 1024 * 1024,
                    "SCM enumeration exceeds its byte budget"
                );
                continue;
            }
            ensure!(
                (count as usize)
                    <= bytes.len() * 8 / std::mem::size_of::<ENUM_SERVICE_STATUS_PROCESSW>(),
                "SCM returned an invalid record count"
            );
            let rows = std::slice::from_raw_parts(
                bytes.as_ptr().cast::<ENUM_SERVICE_STATUS_PROCESSW>(),
                count as usize,
            );
            let mut result = Vec::new();
            for row in rows.iter().take(4000) {
                let label = text(row.lpServiceName)?;
                let identity = service_identity(&label).unwrap_or_else(|_| {
                    let pid = (row.ServiceStatusProcess.dwProcessId > 4)
                        .then_some(row.ServiceStatusProcess.dwProcessId);
                    ServiceIdentity {
                        label,
                        domain: "scm/local".into(),
                        uid: u32::MAX,
                        pid,
                        process_start_time: pid
                            .and_then(|pid| windows_identity::process_start_time(pid).ok()),
                        owner_sid: None,
                        configuration_hash: None,
                    }
                });
                result.push(ServiceInfo {
                    pid: identity.pid,
                    identity,
                    last_exit: Some(row.ServiceStatusProcess.dwWin32ExitCode as i32),
                });
            }
            return Ok(result);
        }
        anyhow::bail!("SCM service list changed repeatedly during enumeration")
    }
}

pub fn validate_service(
    expected: &ServiceIdentity,
    caller_sid: &str,
    administrator: bool,
) -> Result<String> {
    let actual = service_identity(&expected.label)?;
    ensure!(
        expected.domain == "scm/local"
            && expected.owner_sid.is_some()
            && expected.configuration_hash.is_some(),
        "incomplete Windows service identity"
    );
    ensure!(
        administrator || expected.owner_sid.as_deref() == Some(caller_sid),
        "service belongs to another account SID"
    );
    ensure!(
        same_identity(&actual, expected),
        "SCM service identity changed"
    );
    Ok(format!(
        "Restart Windows service {} ({})",
        expected.label,
        expected.owner_sid.as_deref().unwrap()
    ))
}

fn same_identity(actual: &ServiceIdentity, expected: &ServiceIdentity) -> bool {
    actual.label == expected.label
        && actual.domain == expected.domain
        && actual.owner_sid == expected.owner_sid
        && actual.configuration_hash == expected.configuration_hash
        && actual.pid == expected.pid
        && actual.process_start_time == expected.process_start_time
}

pub fn restart_service(
    expected: &ServiceIdentity,
    caller_sid: &str,
    administrator: bool,
) -> Result<()> {
    validate_service(expected, caller_sid, administrator)?;
    let service = open(
        &expected.label,
        SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS | SERVICE_START | SERVICE_STOP,
    )?;
    ensure!(
        same_identity(&identity(&service, &expected.label)?, expected),
        "SCM target changed before execution"
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    let state = status(&service)?;
    if state.dwCurrentState == SERVICE_RUNNING {
        ensure!(
            state.dwControlsAccepted & SERVICE_ACCEPT_STOP != 0,
            "Windows service does not accept stop"
        );
        let mut stopped: SERVICE_STATUS = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut stopped) } != 0,
            "SCM stop failed ({})",
            std::io::Error::last_os_error()
        );
        wait_state(&service, SERVICE_STOPPED, deadline)?;
    } else {
        ensure!(
            state.dwCurrentState == SERVICE_STOPPED,
            "Windows service is already transitioning"
        );
    }
    ensure!(
        unsafe { StartServiceW(service.0, 0, ptr::null()) } != 0,
        "SCM start failed ({})",
        std::io::Error::last_os_error()
    );
    wait_state(&service, SERVICE_RUNNING, deadline)
}

fn wait_state(service: &ServiceHandle, target: u32, deadline: Instant) -> Result<()> {
    loop {
        if status(service)?.dwCurrentState == target {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "Windows service transition timed out"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

// FVPROV1 12052 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiMDI2OGE1MDYzMjBjOTM1ZDU1YjFlMmM1M2ZkNWE1MDAyYjhlZmVkNzU3OGFhYzQzMzYxZThlZjNlNGI4ZjI3YyIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiYzBlMjRiMTdhYWNlMmQ2MTExMDcyYTEzZDA4YjllNDlhYTJmMWUwNDIzOWVkOTRjNWQ5YTBmY2Q4ZmNiMDk2NjgwNDM0YzNmN2JkYjlkYWYzYWFjYzEwM2Q2NGQzMDY1ZDczZTk3ZTA0MjNkYjQwNmQ2MWI1YWM0NzVjNTZhMDIifV19XQ== FVPROV1-END
