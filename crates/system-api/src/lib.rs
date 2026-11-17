// SPDX-License-Identifier: GPL-3.0-only
#[cfg(any(target_os = "android", test))]
mod android_system;
mod audit;
mod backend;
mod broker_client;
mod identity;
pub mod ipc;
#[cfg(target_os = "linux")]
pub mod linux_admin;
#[cfg(target_os = "linux")]
pub mod linux_system;
#[cfg(target_os = "macos")]
pub mod macos_admin;
mod observation;
pub mod operations;
mod plugin;
mod power;
mod recovery;
mod types;
#[cfg(windows)]
pub mod windows;

pub use backend::{
    bounded_command, current_uid, default_data_dir, process_identity, user_services,
    user_services_for_uid, SystemClient,
};
#[cfg(target_os = "android")]
pub use broker_client::android_root;
pub use broker_client::BrokerClient;
pub use identity::{code_hash_for_path, running_code_hash};
pub use observation::{
    process_resources, ObservationEvent, ProcessObservation, ProcessResource, ProcessResources,
};
pub use plugin::run_plugin;
pub use power::sample_power;
pub use recovery::RecoveryStore;
pub use types::*;

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

pub fn random_token() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("operating-system random source failed: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn isolated_store() -> (tempfile::TempDir, RecoveryStore) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("managed");
        let store = RecoveryStore::new(root).unwrap();
        (directory, store)
    }

    #[test]
    fn recovery_round_trip_conflict_and_reopen() {
        let (_directory, store) = isolated_store();
        let created = store.edit("note.txt", b"before").unwrap();
        let changed = store.edit("note.txt", b"after").unwrap();
        let reopened = RecoveryStore::new(store.root().to_path_buf()).unwrap();
        assert_eq!(reopened.read("note.txt").unwrap(), "after");
        assert_eq!(reopened.records().unwrap().len(), 2);
        reopened.rollback(&changed.id).unwrap();
        assert_eq!(reopened.read("note.txt").unwrap(), "before");
        let edited = reopened.edit("note.txt", b"revision").unwrap();
        std::fs::write(store.root().join("note.txt"), b"external change").unwrap();
        assert!(reopened.rollback(&edited.id).is_err());
        assert!(reopened.edit("../escape", b"x").is_err());
        assert!(reopened.rollback(&created.id).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn recovery_rejects_symbolic_and_hard_links() {
        let (directory, store) = isolated_store();
        let outside = directory.path().join("outside.txt");
        std::fs::write(&outside, b"protected").unwrap();
        std::os::unix::fs::symlink(&outside, store.root().join("link.txt")).unwrap();
        assert!(store.edit("link.txt", b"changed").is_err());
        std::fs::hard_link(&outside, store.root().join("hard.txt")).unwrap();
        assert!(store.edit("hard.txt", b"changed").is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"protected");
    }

    #[test]
    fn ipc_rejects_unknown_fields_and_oversized_messages() {
        use std::io::Cursor;
        let bytes = b"{\"nonce\":\"x\",\"sequence\":1,\"request\":{\"operation\":\"Audit\"},\"extra\":true}\n";
        assert!(ipc::read_message::<ipc::Envelope>(&mut Cursor::new(bytes)).is_err());
        assert!(ipc::read_message::<ipc::Envelope>(&mut Cursor::new(vec![
            b'x';
            ipc::MAX_MESSAGE + 1
        ]))
        .is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_snapshot_and_running_code_identity() {
        let identity = process_identity(std::process::id()).unwrap();
        assert_eq!(identity.uid, current_uid().to_string());
        assert!(identity.start_time > 0 && !identity.exe.is_empty());
        assert_eq!(
            running_code_hash(std::process::id()).unwrap(),
            code_hash_for_path(&std::env::current_exe().unwrap()).unwrap()
        );
        let client = SystemClient::new().unwrap();
        let snapshot = client.snapshot().unwrap();
        assert!(snapshot
            .processes
            .iter()
            .any(|process| process.identity.pid == std::process::id()));
        assert!(!snapshot.source.is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn plugin_executes_in_seatbelt_and_cannot_read_outside() {
        let (_directory, store) = isolated_store();
        let result = run_plugin(
            store.root(),
            std::path::Path::new("/usr/bin/printf"),
            &["plugin-%s".into(), "ok".into()],
            PluginCapabilities::default(),
        )
        .unwrap();
        assert_eq!(result.exit_code, Some(0), "{}", result.output);
        assert_eq!(result.output, "plugin-ok");
        assert!(!result.broker_access);
        let denied = run_plugin(
            store.root(),
            std::path::Path::new("/bin/cat"),
            &["/etc/passwd".into()],
            PluginCapabilities::default(),
        )
        .unwrap();
        assert_ne!(denied.exit_code, Some(0));
        std::env::set_var("GLASS_PLUGIN_TEST_CANARY", "dummy-canary-do-not-inherit");
        let environment = run_plugin(
            store.root(),
            std::path::Path::new("/usr/bin/printenv"),
            &["GLASS_PLUGIN_TEST_CANARY".into()],
            PluginCapabilities::default(),
        )
        .unwrap();
        std::env::remove_var("GLASS_PLUGIN_TEST_CANARY");
        assert_eq!(environment.output, "");
        assert_ne!(environment.exit_code, Some(0));
        let forked = run_plugin(
            store.root(),
            std::path::Path::new("/bin/sh"),
            &["-c".into(), "(printf child > child.txt) & wait".into()],
            PluginCapabilities {
                write_workspace: true,
                network: false,
            },
        )
        .unwrap();
        assert!(
            !store.root().join("child.txt").exists(),
            "sandbox child process unexpectedly wrote a file: {}",
            forked.output
        );
        let socket_path = store.root().parent().unwrap().join("private-broker.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let connection = run_plugin(
            store.root(),
            std::path::Path::new("/usr/bin/nc"),
            &[
                "-z".into(),
                "-w".into(),
                "1".into(),
                "-U".into(),
                socket_path.to_string_lossy().into_owned(),
            ],
            PluginCapabilities::default(),
        )
        .unwrap();
        assert_ne!(connection.exit_code, Some(0));
        assert!(
            listener.accept().is_err(),
            "plugin reached a private broker-style socket"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn managed_directory_produces_bounded_native_file_events() {
        let directory = tempfile::tempdir().unwrap();
        let client = SystemClient::with_managed_root(
            directory.path().canonicalize().unwrap().join("managed"),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        client.edit_managed("observed.txt", "native-event").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let events = client.file_events().unwrap();
            if events.iter().any(|event| {
                event
                    .paths
                    .iter()
                    .any(|path| path.ends_with("observed.txt"))
            }) {
                assert!(events.len() <= 512);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "native managed-directory event was not observed"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn completion_plugin_cannot_read_or_modify_recovery_journal() {
        let (_directory, store) = isolated_store();
        store.edit("note.txt", b"before").unwrap();
        let record = store.edit("note.txt", b"after").unwrap();
        let journal = store.root().join(".glass-journal");
        let before = journal.join(format!("{}.before", record.id));
        let read = run_plugin(
            store.root(),
            std::path::Path::new("/bin/cat"),
            &[before.to_string_lossy().into_owned()],
            PluginCapabilities::default(),
        )
        .unwrap();
        assert_ne!(read.exit_code, Some(0));
        let metadata = journal.join(format!("{}.json", record.id));
        let original = std::fs::read(&metadata).unwrap();
        let write = run_plugin(
            store.root(),
            std::path::Path::new("/bin/sh"),
            &[
                "-c".into(),
                "printf corrupted > \"$1\"".into(),
                "fixture".into(),
                metadata.to_string_lossy().into_owned(),
            ],
            PluginCapabilities {
                write_workspace: true,
                network: false,
            },
        )
        .unwrap();
        assert_ne!(write.exit_code, Some(0));
        assert_eq!(std::fs::read(metadata).unwrap(), original);
    }

    #[test]
    fn completion_log_query_uses_the_application_process_name() {
        assert_eq!(
            LogQuery::default().process.as_deref(),
            Some("glass-terminal")
        );
    }
}

// FVPROV1 9822 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiMWUyN2UzMDVmY2Y0MTQyMWRiNThiOTU1NjljNzhkMjI0OTY3ZDliOWU2YmRmMjA2MGEyZmNkYzY0YzdmYzRhZSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMjgxOTUyOGYzZmQyMmNjNjcyMTg4MjBmMmQ0YmQ5ZmQ4MzUzZjVjOTBmYjY3MjczYmVjY2JiNTZhMzBiNGI5NDk1Y2RhZWY3MWI1ZWRjMDc4ZWYwY2M4ZDk1NjFlMTU5N2ZlMzc5OGZjNTIyZDVhNjZiMzE3YmQ2ZGZkZWMwMDQifV19XQ== FVPROV1-END
