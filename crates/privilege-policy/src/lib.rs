// SPDX-License-Identifier: GPL-3.0-only
#[cfg(not(target_os = "linux"))]
use anyhow::Context;
use anyhow::{ensure, Result};
use system_api::*;

#[derive(Clone, Copy)]
pub struct Caller {
    pub uid: u32,
    pub pid: u32,
    pub broker_pid: u32,
    pub administrator: bool,
}

pub fn validate(action: &PrivilegedAction, caller: Caller) -> Result<String> {
    ensure!(
        caller.uid != 0 && caller.pid > 1,
        "ordinary control requires a normal authenticated UI user"
    );
    match action {
        PrivilegedAction::StopProcess(expected) => {
            ensure!(
                expected.pid > 1 && expected.pid != caller.pid && expected.pid != caller.broker_pid,
                "protected process target"
            );
            ensure!(
                (caller.administrator && expected.uid.parse::<u32>().is_ok()
                    || expected.uid == caller.uid.to_string())
                    && !expected.exe.is_empty(),
                "process ownership or executable identity is unavailable"
            );
            let current = process_identity(expected.pid)?;
            ensure!(
                current.uid == expected.uid
                    && current.start_time == expected.start_time
                    && current.exe == expected.exe,
                "process target identity changed"
            );
            Ok(format!(
                "{}Send SIGTERM to {} (PID {}, UID {}, start ID {})",
                if caller.administrator {
                    "Administrator session. "
                } else {
                    ""
                },
                expected.exe,
                expected.pid,
                expected.uid,
                expected.start_time
            ))
        }
        PrivilegedAction::RestartUserService(expected) => {
            #[cfg(target_os = "linux")]
            {
                ensure!(
                    expected.pid != Some(caller.pid) && expected.pid != Some(caller.broker_pid),
                    "protected service target"
                );
                linux_system::validate_service(expected, caller.uid)?;
                Ok(format!("Restart {}/{}", expected.domain, expected.label))
            }
            #[cfg(not(target_os = "linux"))]
            {
                ensure!(
                    expected.uid == caller.uid && expected.domain == format!("gui/{}", caller.uid),
                    "service target is outside the authenticated user domain"
                );
                ensure!(
                    !expected.label.is_empty()
                        && expected.label.len() <= 200
                        && expected
                            .label
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte)),
                    "invalid service label"
                );
                let current = user_services_for_uid(caller.uid)?
                    .into_iter()
                    .find(|service| service.identity.label == expected.label)
                    .context("user service no longer exists")?;
                ensure!(
                    current.identity.uid == expected.uid
                        && current.identity.domain == expected.domain
                        && current.identity.pid == expected.pid
                        && current.identity.process_start_time == expected.process_start_time,
                    "service process identity changed"
                );
                ensure!(
                    expected.pid != Some(caller.pid) && expected.pid != Some(caller.broker_pid),
                    "protected service target"
                );
                Ok(format!("Restart {}/{}", expected.domain, expected.label))
            }
        }
    }
}

pub fn execute(action: &PrivilegedAction, caller: Caller) -> Result<OperationResult> {
    let summary = validate(action, caller)?;
    match action {
        PrivilegedAction::StopProcess(target) => {
            #[cfg(unix)]
            ensure!(
                unsafe { libc::kill(target.pid as i32, libc::SIGTERM) } == 0,
                "SIGTERM failed: {}",
                std::io::Error::last_os_error()
            );
            #[cfg(not(unix))]
            {
                let _ = target;
                anyhow::bail!("native process control unavailable");
            }
        }
        PrivilegedAction::RestartUserService(target) => {
            #[cfg(target_os = "linux")]
            linux_system::restart_user_service(target, caller.uid)?;
            #[cfg(not(target_os = "linux"))]
            {
                let result = bounded_command(
                    "/bin/launchctl",
                    &[
                        "kickstart".into(),
                        "-k".into(),
                        format!("{}/{}", target.domain, target.label),
                    ],
                    // launchd can defer a restart during its normal startup
                    // throttle interval. Keep this below the 30-second IPC budget.
                    std::time::Duration::from_secs(15),
                    65_536,
                )
                .context("service restart completion is unconfirmed. Refresh its native state before retrying")?;
                ensure!(
                    result.success,
                    "user service restart failed: {}",
                    result.stderr
                );
            }
        }
    }
    Ok(OperationResult {
        success: true,
        message: summary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_kernel_and_foreign_service_targets_are_rejected() {
        let caller = Caller {
            uid: current_uid(),
            pid: std::process::id(),
            broker_pid: std::process::id(),
            administrator: false,
        };
        let myself = process_identity(std::process::id()).unwrap();
        assert!(validate(&PrivilegedAction::StopProcess(myself.clone()), caller).is_err());
        let mut kernel = myself;
        kernel.pid = 1;
        assert!(validate(&PrivilegedAction::StopProcess(kernel), caller).is_err());
        let service = ServiceIdentity {
            label: "com.example.fixture".into(),
            domain: "system".into(),
            uid: 0,
            pid: None,
            process_start_time: None,
            owner_sid: None,
            configuration_hash: None,
        };
        assert!(validate(
            &PrivilegedAction::RestartUserService(service.clone()),
            caller
        )
        .is_err());
        assert!(validate(
            &PrivilegedAction::RestartUserService(service),
            Caller {
                administrator: true,
                ..caller
            },
        )
        .is_err());
    }
}

// FVPROV1 6835 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiZTRkMThhZDFiMTE0M2VmOTk1ZDljNzk3MjIzOTdkY2ZkYzA4MGJkY2UwYzQyZDhiNTk0MWFlZmE2OGFjNDgwZCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiYzU4YjI3YzQxM2EyMzlmNzIzMGU0N2JhM2ViOGFkZmU3MDI0YjM4YzA1YWU3N2ZjYmY2ZTc1MWViM2FiZDdlZWQ4NTg3MGI2ODNiYmFkNGYxNDIwOTk0YjVkNTNjN2U4NmEzZjE0YTM0OGM2ODgwNzgwMjE2M2JiNGIxYTllMDgifV19XQ== FVPROV1-END
