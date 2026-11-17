#![cfg(unix)]
use std::{
    io::{BufReader, Read},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use system_api::{ipc::*, *};

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        self.0.kill().ok();
        let _ = self.0.wait();
    }
}

fn start_raw() -> (tempfile::TempDir, OwnedChild, Bootstrap) {
    let directory = tempfile::Builder::new()
        .prefix("glass-broker-test-")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let bootstrap = Bootstrap {
        nonce: random_token().unwrap(),
        parent_pid: std::process::id(),
        parent_uid: current_uid(),
        socket_path: directory
            .path()
            .join("ipc.sock")
            .to_string_lossy()
            .into_owned(),
        audit_path: directory
            .path()
            .join("audit.jsonl")
            .to_string_lossy()
            .into_owned(),
        administrator: false,
    };
    let mut child = Command::new(env!("CARGO_BIN_EXE_plyra-privilege-broker"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    write_message(&bootstrap, &mut child.stdin.take().unwrap()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !std::path::Path::new(&bootstrap.socket_path).exists() {
        assert!(Instant::now() < deadline);
        assert!(child.try_wait().unwrap().is_none());
        thread::sleep(Duration::from_millis(10));
    }
    (directory, OwnedChild(child), bootstrap)
}

#[test]
fn authentication_nonce_and_replay_fail_closed() {
    for invalid_nonce in [true, false] {
        let (_directory, mut child, bootstrap) = start_raw();
        let stream = UnixStream::connect(&bootstrap.socket_path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(stream);
        let envelope = Envelope {
            nonce: if invalid_nonce {
                "0".repeat(64)
            } else {
                bootstrap.nonce
            },
            sequence: 1,
            request: Request::Audit,
        };
        write_message(&envelope, reader.get_mut()).unwrap();
        if !invalid_nonce {
            assert!(matches!(
                read_message::<Response>(&mut reader).unwrap(),
                Some(Response::Audit(_))
            ));
            write_message(&envelope, reader.get_mut()).unwrap();
        }
        assert!(read_message::<Response>(&mut reader)
            .map(|response| response.is_none())
            .unwrap_or(true));
        let deadline = Instant::now() + Duration::from_secs(2);
        while child.0.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
    }
}

#[test]
fn completion_authenticated_identity_confirmation_and_revocation() {
    std::env::set_var(
        "PLYRA_BROKER_BIN",
        env!("CARGO_BIN_EXE_plyra-privilege-broker"),
    );
    let mut fixture = OwnedChild(Command::new("/bin/sleep").arg("30").spawn().unwrap());
    let identity = process_identity(fixture.0.id()).unwrap();
    let broker = BrokerClient::spawn().unwrap();
    assert!(!broker.is_admin());
    assert_eq!(broker.peer_identity().0, current_uid());
    let authenticated = broker.session_identity().unwrap();
    assert_eq!(authenticated.broker_uid, current_uid());
    assert_eq!(authenticated.caller_uid, current_uid());
    assert_eq!(authenticated.caller_pid, std::process::id());
    assert_eq!(authenticated.broker_pid, broker.peer_identity().1);
    assert!(!authenticated.administrator);
    let first = broker
        .prepare(PrivilegedAction::StopProcess(identity.clone()))
        .unwrap();
    broker.revoke().unwrap();
    assert!(broker.confirm(&first.ticket).is_err());
    assert!(fixture.0.try_wait().unwrap().is_none());
    let second = broker
        .prepare(PrivilegedAction::StopProcess(identity))
        .unwrap();
    assert!(broker.confirm(&second.ticket).unwrap().success);
    assert!(broker.confirm(&second.ticket).is_err());
    assert!(broker
        .audit()
        .unwrap()
        .iter()
        .any(|record| record.result == "completed"));
    let deadline = Instant::now() + Duration::from_secs(2);
    while fixture.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn kernel_peer_pid_rejects_a_different_process() {
    let (_directory, mut broker, bootstrap) = start_raw();
    let result = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "foreign_peer_probe", "--exact"])
        .env("GLASS_TEST_PEER_SOCKET", bootstrap.socket_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(result.success());
    let deadline = Instant::now() + Duration::from_secs(2);
    while broker.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "owned child-process probe invoked by kernel_peer_pid_rejects_a_different_process"]
fn foreign_peer_probe() {
    let path = std::env::var("GLASS_TEST_PEER_SOCKET").unwrap();
    let mut stream = UnixStream::connect(path).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut bytes = [0u8; 1];
    assert!(stream
        .read(&mut bytes)
        .map(|count| count == 0)
        .unwrap_or(true));
}

// FVPROV1 5797 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNWZlZTZiMzlkM2Y5M2ZkYzlkYmQ1ZTBlYWY4YTNjY2FiMzI1ZDRlOTZmZDA0MzM2NTRhZDE5Njc2YTE2Y2Q3MyIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMTMxYjU0OGNjNzNmM2YzOWIzOGRiYmEzOTQyMzQwYjVhZTVmZTQwYmI0NjI3YzdjMDY4YTE0NTRjYzYyMGM0MDcwZTc5ZWFjYmViY2JkMjUyYTg4M2I3ZDYyOWQ1NWI0ODc2NGQ4YjY3ODhkNWU5ZGVhY2ZmZTgzMTY2MDE3MDgifV19XQ== FVPROV1-END
