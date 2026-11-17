// SPDX-License-Identifier: GPL-3.0-only
use crate::*;
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Read, Write};

pub const MAX_MESSAGE: usize = 65_536;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bootstrap {
    pub nonce: String,
    pub parent_pid: u32,
    pub parent_uid: u32,
    pub socket_path: String,
    pub audit_path: String,
    #[serde(default)]
    pub administrator: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub nonce: String,
    pub sequence: u64,
    pub request: Request,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", content = "data", deny_unknown_fields)]
pub enum Request {
    Identity,
    Prepare(PrivilegedAction),
    PrepareProcessStop { expected: ProcessIdentity },
    Confirm { ticket: String },
    Revoke,
    CancelPending,
    Audit,
    PowerSample { interval_ms: u16 },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "result", content = "data", deny_unknown_fields)]
pub enum Response {
    Identity(BrokerSessionIdentity),
    Prepared(PreparedAction),
    Completed(OperationResult),
    Revoked,
    Cancelled,
    Audit(Vec<AuditRecord>),
    PowerSample(PowerSample),
    Error(String),
}

pub fn read_message<T: serde::de::DeserializeOwned>(
    reader: &mut impl BufRead,
) -> Result<Option<T>> {
    let mut bytes = Vec::new();
    let count = Read::by_ref(reader)
        .take((MAX_MESSAGE + 1) as u64)
        .read_until(b'\n', &mut bytes)?;
    if count == 0 {
        return Ok(None);
    }
    ensure!(
        bytes.len() <= MAX_MESSAGE && bytes.last() == Some(&b'\n'),
        "IPC message is truncated or oversized"
    );
    Ok(Some(
        serde_json::from_slice(&bytes).context("invalid IPC schema")?,
    ))
}

pub fn write_message(value: &impl Serialize, writer: &mut impl Write) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() < MAX_MESSAGE,
        "IPC response exceeds message limit"
    );
    bytes.push(b'\n');
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

#[cfg(unix)]
pub fn peer_identity(stream: &std::os::unix::net::UnixStream) -> Result<(u32, u32)> {
    use std::os::fd::AsRawFd;
    #[cfg(target_os = "macos")]
    {
        let mut uid = 0;
        let mut gid = 0;
        ensure!(
            unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } == 0,
            "cannot authenticate IPC peer UID"
        );
        let mut pid: libc::pid_t = 0;
        let mut length = std::mem::size_of_val(&pid) as libc::socklen_t;
        ensure!(
            unsafe {
                libc::getsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_LOCAL,
                    libc::LOCAL_PEERPID,
                    (&mut pid as *mut libc::pid_t).cast(),
                    &mut length,
                )
            } == 0
                && pid > 0,
            "cannot authenticate IPC peer PID"
        );
        Ok((uid, pid as u32))
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of_val(&credentials) as libc::socklen_t;
        ensure!(
            unsafe {
                libc::getsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&mut credentials as *mut libc::ucred).cast(),
                    &mut length,
                )
            } == 0
                && credentials.pid > 0,
            "cannot authenticate IPC peer credentials"
        );
        Ok((credentials.uid, credentials.pid as u32))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
    {
        let _ = stream;
        anyhow::bail!("kernel-authenticated broker IPC is unavailable on this platform")
    }
}

// FVPROV1 4019 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiZjdmNTM0N2M1Y2ViM2JlOGY1YmEyNzc4NTEwMTIxMWE0NmE1NzE4YjljNjVmNWVjMzlkZmUzOWUyODFhNmEwYyIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiZjUwNmVjMmI3OWZiZWNkM2I0NzEyYjlmZTNkOTM2MmU5ZjU5NjAwZjUyOTNmNTc1MzEzYmRlZGQ2YjA0Y2QwZWFhNGI1NjczZmU4N2Q0ZWY1MjdiMTJkYjU0NDk5YjM4ZWY5MGE5YzU0ZDcyM2U3ZGNmMTIxZTQ5ZmZmMzg3MDMifV19XQ== FVPROV1-END
