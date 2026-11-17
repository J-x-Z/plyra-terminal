// SPDX-License-Identifier: GPL-3.0-only
use anyhow::{ensure, Context, Result};
use std::{
    io::BufReader,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::Path,
    time::Duration,
};
use system_api::{ipc::*, macos_admin, *};

pub(super) fn dispatch(arguments: &[String]) -> Option<Result<()>> {
    match arguments.first().map(String::as_str) {
        Some("--persistent-service") => Some((|| {
            ensure!(
                arguments.len() == 2,
                "invalid persistent administrator arguments"
            );
            run(arguments[1].parse()?)
        })()),
        Some("--install-persistent") => Some((|| {
            ensure!(
                arguments.len() == 8
                    && arguments[2] == "--expected-ui-cdhash"
                    && arguments[4] == "--expected-broker-cdhash"
                    && arguments[6] == "--authorized-uid",
                "invalid administrator installation arguments"
            );
            macos_admin::install_from_request(
                Path::new(&arguments[1]),
                &arguments[3],
                &arguments[5],
                arguments[7].parse()?,
            )
        })()),
        _ => None,
    }
}

fn run(uid: u32) -> Result<()> {
    let grant = macos_admin::validate_service(uid)?;
    macos_admin::reset_service_socket(uid)?;
    let socket = macos_admin::socket_path(uid);
    let listener = UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    let path = std::ffi::CString::new(
        socket
            .to_str()
            .context("invalid administrator socket path")?,
    )?;
    ensure!(
        unsafe { libc::chown(path.as_ptr(), uid, u32::MAX) } == 0,
        "cannot grant the authorized UI access to its socket"
    );
    // One active application session per user. EOF discards its tickets before
    // accepting another session. Blocking accept/read produces no idle polling.
    loop {
        let (stream, _) = listener.accept()?;
        let mut revoking = false;
        let session = (|| -> Result<()> {
            let (peer_uid, peer_pid) = peer_identity(&stream)?;
            ensure!(
                peer_uid == uid && running_code_hash(peer_pid)? == grant.ui_cdhash,
                "persistent administrator rejected a different UI identity"
            );
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            stream.set_write_timeout(Some(Duration::from_secs(5)))?;
            let mut reader = BufReader::new(stream);
            let bootstrap: Bootstrap =
                read_message(&mut reader)?.context("missing persistent administrator handshake")?;
            ensure!(
                bootstrap.administrator
                    && bootstrap.parent_uid == uid
                    && bootstrap.parent_pid == peer_pid
                    && bootstrap.nonce.len() == 64
                    && bootstrap.nonce.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "persistent administrator handshake identity mismatch"
            );
            reader.get_mut().set_read_timeout(None)?;
            super::serve_session(
                reader,
                bootstrap,
                Some(grant.ui_cdhash.clone()),
                None,
                || {
                    revoking = true;
                    macos_admin::remove_installation(uid)
                },
            )
        })();
        if revoking {
            // Durable files and the final response are handled before bootout,
            // which can terminate this process while launchctl is still running.
            drop(listener);
            let unload = macos_admin::bootout(uid);
            session?;
            return unload;
        }
        if let Err(error) = session {
            eprintln!("persistent administrator session ended: {error:#}");
        }
    }
}

// FVPROV1 3926 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNjVhY2EzZjU3OTBmNGMzMjZmM2U2OTIyMTljNDJlODM2MmU5ZGNmNTMxZTYzZDMyOTc4Y2I5OWYxMGU2NTMxNSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiZmIwYzk1Yzk4ZDMxYWY1MGExNWVhNGQyNGNjN2JlMDE4ODkzYTFhOTNmYzc3M2FmOGFjZDVkNmVmNDA4MDBkOWE3ZmU0OWVlZGMzYjViYThiNmQ1NzMxMGRmYTBiZWE0MzAxYjBmYWM4YzM0MGM1NzEwZGJhYmYyZTE2YTQ3MDEifV19XQ== FVPROV1-END
