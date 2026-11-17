// SPDX-License-Identifier: GPL-3.0-only
//! Native launchd installation and pinned identities for remembered authorization.
use crate::{bounded_command, code_hash_for_path, current_uid, now_ms, running_code_hash};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

const GRANT_VERSION: u32 = 1;

#[derive(Clone, Debug)]
pub struct PersistentStatus {
    pub installed: bool,
    pub compatible: bool,
    pub description: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistentGrant {
    pub version: u32,
    pub uid: u32,
    pub ui_cdhash: String,
    pub broker_cdhash: String,
    pub installed_at_ms: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallRequest {
    uid: u32,
    pid: u32,
    ui_cdhash: String,
    broker_cdhash: String,
}

pub fn label(uid: u32) -> String {
    format!("org.plyra.glass-terminal.admin.{uid}")
}

pub fn helper_path(uid: u32) -> PathBuf {
    Path::new("/Library/PrivilegedHelperTools").join(label(uid))
}

pub fn grant_path(uid: u32) -> PathBuf {
    Path::new("/Library/PrivilegedHelperTools").join(format!("{}.json", label(uid)))
}

pub fn plist_path(uid: u32) -> PathBuf {
    Path::new("/Library/LaunchDaemons").join(format!("{}.plist", label(uid)))
}

pub fn socket_path(uid: u32) -> PathBuf {
    Path::new("/Library/PrivilegedHelperTools").join(format!("{}.sock", label(uid)))
}

fn valid_hash(hash: &str) -> bool {
    (40..=64).contains(&hash.len()) && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn protected_directory(path: &Path) -> Result<()> {
    let metadata = path.symlink_metadata()?;
    ensure!(
        metadata.is_dir() && metadata.uid() == 0 && metadata.mode() & 0o022 == 0,
        "persistent administrator directory is not protected: {}",
        path.display()
    );
    Ok(())
}

fn protected_file(path: &Path) -> Result<File> {
    protected_directory(path.parent().context("missing installation directory")?)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == 0
            && metadata.nlink() == 1
            && metadata.mode() & 0o022 == 0,
        "persistent administrator file is not protected: {}",
        path.display()
    );
    Ok(file)
}

pub fn read_grant(uid: u32) -> Result<PersistentGrant> {
    ensure!(uid != 0, "persistent administrator requires a normal user");
    let mut file = protected_file(&grant_path(uid))?;
    ensure!(
        file.metadata()?.len() <= 4096,
        "administrator grant is oversized"
    );
    let mut bytes = Vec::new();
    Read::by_ref(&mut file).take(4097).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 4096, "administrator grant is oversized");
    let grant: PersistentGrant = serde_json::from_slice(&bytes)?;
    ensure!(
        grant.version == GRANT_VERSION
            && grant.uid == uid
            && valid_hash(&grant.ui_cdhash)
            && valid_hash(&grant.broker_cdhash),
        "persistent administrator grant has an invalid identity"
    );
    Ok(grant)
}

pub fn persistent_status() -> Result<PersistentStatus> {
    let uid = current_uid();
    ensure!(uid != 0, "the terminal UI must run as a normal user");
    if !grant_path(uid).try_exists()?
        && !helper_path(uid).try_exists()?
        && !plist_path(uid).try_exists()?
    {
        return Ok(PersistentStatus {
            installed: false,
            compatible: false,
            description: "Remembered administrator authorization is not installed".into(),
        });
    }
    let grant = read_grant(uid)?;
    protected_file(&plist_path(uid))?;
    protected_file(&helper_path(uid))?;
    let expected =
        code_hash_for_path(&std::env::current_exe()?.with_file_name("plyra-privilege-broker"))?;
    let compatible = grant.ui_cdhash == running_code_hash(std::process::id())?
        && grant.broker_cdhash == expected
        && code_hash_for_path(&helper_path(uid))? == expected;
    Ok(PersistentStatus {
        installed: true,
        compatible,
        description: if compatible {
            "Remembered administrator authorization is installed for this application build".into()
        } else {
            "Installed administrator helper belongs to another application build; explicit update is required".into()
        },
    })
}

fn shell_quote(path: &Path) -> Result<String> {
    let value = path.to_str().context("administrator path is not UTF-8")?;
    ensure!(
        !value.chars().any(char::is_control),
        "administrator path contains controls"
    );
    Ok(format!("'{}'", value.replace('\'', "'\\''")))
}

/// Requests native authentication only when called by an explicit install/update action.
pub fn install_persistent_helper() -> Result<()> {
    let uid = current_uid();
    ensure!(uid != 0, "the terminal UI must run as a normal user");
    let broker = std::env::current_exe()?.with_file_name("plyra-privilege-broker");
    let request = InstallRequest {
        uid,
        pid: std::process::id(),
        ui_cdhash: running_code_hash(std::process::id())?,
        broker_cdhash: code_hash_for_path(&broker)?,
    };
    let directory = tempfile::Builder::new()
        .prefix("glass-admin-install-")
        .tempdir_in("/tmp")?;
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
    let path = directory.path().join("install.json");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    serde_json::to_writer(&mut file, &request)?;
    file.sync_all()?;
    let script = installer_script(&broker, &path, &request)?;
    let statement = format!(
        "do shell script {} with administrator privileges",
        serde_json::to_string(&script)?
    );
    let result = bounded_command(
        "/usr/bin/osascript",
        &["-e".into(), statement],
        Duration::from_secs(120),
        65_536,
    )?;
    ensure!(
        result.success,
        "administrator helper installation failed: {}",
        result.stderr
    );
    Ok(())
}

fn installer_script(broker: &Path, path: &Path, request: &InstallRequest) -> Result<String> {
    ensure!(
        request.uid != 0 && valid_hash(&request.ui_cdhash) && valid_hash(&request.broker_cdhash),
        "invalid administrator installation identity"
    );
    Ok(format!(
        "set -eu\numask 077\nstage=$(/usr/bin/mktemp -d /private/tmp/glass-admin-install.XXXXXXXX)\ntrap '/bin/rm -f \"$stage/broker\"; /bin/rmdir \"$stage\"' EXIT\n/usr/bin/install -m 555 {} \"$stage/broker\"\n/usr/bin/codesign --verify --strict \"$stage/broker\"\n/usr/bin/codesign -d --verbose=4 \"$stage/broker\" 2>&1 | /usr/bin/grep -F -x 'CDHash={}' >/dev/null\n/usr/bin/env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin HOME=/var/root \"$stage/broker\" --install-persistent {} --expected-ui-cdhash '{}' --expected-broker-cdhash '{}' --authorized-uid '{}'",
        shell_quote(broker)?, request.broker_cdhash, shell_quote(path)?, request.ui_cdhash,
        request.broker_cdhash, request.uid
    ))
}

fn prepare_installation_directory(path: &Path) -> Result<()> {
    protected_directory(path.parent().context("missing system parent directory")?)?;
    if !path.try_exists()? {
        std::fs::create_dir(path)?;
        clear_acl(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    protected_directory(path)
}

fn clear_acl(path: &Path) -> Result<()> {
    let result = bounded_command(
        "/bin/chmod",
        &["-N".into(), path.to_string_lossy().into_owned()],
        Duration::from_secs(3),
        4096,
    )?;
    ensure!(
        result.success,
        "cannot clear administrator installation ACL"
    );
    Ok(())
}

fn replace_file(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    protected_directory(path.parent().context("missing installation parent")?)?;
    if path.try_exists()? {
        protected_file(path)?;
    }
    let mut temporary = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    clear_acl(temporary.path())?;
    temporary
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(mode))?;
    temporary.persist(path).map_err(|error| error.error)?;
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}

fn daemon_plist(uid: u32) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{}</string>\n<key>ProgramArguments</key><array><string>{}</string><string>--persistent-service</string><string>{uid}</string></array>\n<key>RunAtLoad</key><true/>\n<key>ProcessType</key><string>Background</string>\n<key>ExitTimeOut</key><integer>5</integer>\n</dict></plist>\n",
        label(uid), helper_path(uid).display()
    )
}

/// Invoked only by the root-owned, hash-verified staged installer executable.
pub fn install_from_request(path: &Path, ui_hash: &str, broker_hash: &str, uid: u32) -> Result<()> {
    ensure!(
        current_uid() == 0 && uid != 0,
        "administrator installation requires root"
    );
    ensure!(
        valid_hash(ui_hash) && valid_hash(broker_hash),
        "invalid installation code identity"
    );
    let own = std::env::current_exe()?;
    protected_file(&own)?;
    ensure!(
        running_code_hash(std::process::id())? == broker_hash
            && code_hash_for_path(&own)? == broker_hash,
        "administrator installer executable identity changed"
    );
    let parent = path
        .parent()
        .context("missing installer request directory")?
        .symlink_metadata()?;
    ensure!(
        parent.is_dir() && parent.uid() == uid && parent.mode() & 0o077 == 0,
        "installer request directory is not private"
    );
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.nlink() == 1
            && metadata.uid() == uid
            && metadata.mode() & 0o077 == 0
            && metadata.len() <= 4096,
        "installer request is not private"
    );
    let mut bytes = Vec::new();
    Read::by_ref(&mut file).take(4097).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 4096, "installer request is oversized");
    let request: InstallRequest = serde_json::from_slice(&bytes)?;
    ensure!(
        request.uid == uid
            && request.pid > 1
            && request.ui_cdhash == ui_hash
            && request.broker_cdhash == broker_hash,
        "installation request identity mismatch"
    );
    ensure!(
        crate::process_identity(request.pid)?.uid == uid.to_string()
            && running_code_hash(request.pid)? == ui_hash,
        "installation caller identity changed"
    );
    prepare_installation_directory(Path::new("/Library/PrivilegedHelperTools"))?;
    prepare_installation_directory(Path::new("/Library/LaunchDaemons"))?;
    protected_directory(
        socket_path(uid)
            .parent()
            .context("administrator socket parent missing")?,
    )?;
    for path in [helper_path(uid), grant_path(uid), plist_path(uid)] {
        if path.try_exists()? {
            protected_file(&path)?;
        }
    }
    stop_existing_job(uid)?;
    remove_socket(uid)?;
    let grant = PersistentGrant {
        version: GRANT_VERSION,
        uid,
        ui_cdhash: ui_hash.into(),
        broker_cdhash: broker_hash.into(),
        installed_at_ms: now_ms(),
    };
    let installation = (|| -> Result<()> {
        replace_file(&helper_path(uid), &std::fs::read(&own)?, 0o555)?;
        ensure!(
            code_hash_for_path(&helper_path(uid))? == broker_hash,
            "installed helper signature changed"
        );
        replace_file(&grant_path(uid), &serde_json::to_vec(&grant)?, 0o444)?;
        replace_file(&plist_path(uid), daemon_plist(uid).as_bytes(), 0o644)?;
        let result = bounded_command(
            "/bin/launchctl",
            &[
                "bootstrap".into(),
                "system".into(),
                plist_path(uid).to_string_lossy().into_owned(),
            ],
            Duration::from_secs(5),
            4096,
        )?;
        ensure!(
            result.success,
            "launchd administrator installation failed: {}",
            result.stderr
        );
        Ok(())
    })();
    if installation.is_err() {
        let _ = remove_installation(uid);
        let _ = bootout(uid);
    }
    installation
}

pub fn validate_service(uid: u32) -> Result<PersistentGrant> {
    ensure!(
        current_uid() == 0,
        "persistent administrator service requires root"
    );
    let grant = read_grant(uid)?;
    let own = std::env::current_exe()?;
    ensure!(
        own == helper_path(uid),
        "persistent administrator executable path changed"
    );
    protected_file(&own)?;
    protected_file(&plist_path(uid))?;
    ensure!(
        running_code_hash(std::process::id())? == grant.broker_cdhash
            && code_hash_for_path(&own)? == grant.broker_cdhash,
        "persistent administrator executable identity changed"
    );
    protected_directory(
        socket_path(uid)
            .parent()
            .context("administrator socket parent missing")?,
    )?;
    Ok(grant)
}

pub fn validate_socket(uid: u32) -> Result<()> {
    protected_directory(
        socket_path(uid)
            .parent()
            .context("administrator socket parent missing")?,
    )?;
    let metadata = socket_path(uid).symlink_metadata()?;
    ensure!(
        metadata.file_type().is_socket() && metadata.uid() == uid && metadata.mode() & 0o077 == 0,
        "persistent administrator socket identity changed"
    );
    Ok(())
}

fn remove_socket(uid: u32) -> Result<()> {
    match socket_path(uid).symlink_metadata() {
        Ok(metadata) => {
            ensure!(
                metadata.file_type().is_socket() && (metadata.uid() == uid || metadata.uid() == 0),
                "administrator socket path is not an owned socket"
            );
            std::fs::remove_file(socket_path(uid))?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub fn reset_service_socket(uid: u32) -> Result<()> {
    ensure!(
        current_uid() == 0 && uid != 0,
        "administrator socket reset requires root"
    );
    protected_directory(
        socket_path(uid)
            .parent()
            .context("administrator socket parent missing")?,
    )?;
    remove_socket(uid)
}

/// Delete durable authorization before stopping the current launchd job.
pub fn remove_installation(uid: u32) -> Result<()> {
    ensure!(
        current_uid() == 0 && uid != 0,
        "administrator removal requires root"
    );
    for path in [grant_path(uid), plist_path(uid), helper_path(uid)] {
        if path.try_exists()? {
            protected_file(&path)?;
            std::fs::remove_file(&path)?;
        }
    }
    remove_socket(uid)
}

/// Call only after the final revocation response has been flushed to the UI.
pub fn bootout(uid: u32) -> Result<()> {
    let result = bounded_command(
        "/bin/launchctl",
        &["bootout".into(), format!("system/{}", label(uid))],
        Duration::from_secs(5),
        4096,
    )?;
    ensure!(
        result.success,
        "administrator launchd unload failed: {}",
        result.stderr
    );
    Ok(())
}

fn stop_existing_job(uid: u32) -> Result<()> {
    let status = bounded_command(
        "/bin/launchctl",
        &["print".into(), format!("system/{}", label(uid))],
        Duration::from_secs(3),
        65_536,
    )?;
    if status.success {
        bootout(uid)?;
    } else {
        ensure!(
            status.code == Some(113),
            "cannot inspect an existing administrator job: {}",
            status.stderr
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistent_paths_and_plist_are_bound_to_one_uid() {
        assert_eq!(
            socket_path(501),
            Path::new("/Library/PrivilegedHelperTools/org.plyra.glass-terminal.admin.501.sock")
        );
        assert!(helper_path(501).to_string_lossy().ends_with(".501"));
        assert_ne!(grant_path(501), grant_path(502));
        assert!(socket_path(501).as_os_str().len() < 104);
        let plist = daemon_plist(501);
        assert!(plist.contains("--persistent-service"));
        assert!(!plist.contains("KeepAlive"));
        assert!(!plist.contains("MachServices"));
    }

    #[test]
    fn persistent_socket_parent_is_protected_on_this_host() {
        let socket = socket_path(current_uid());
        let parent = socket.parent().unwrap();
        assert_eq!(parent, helper_path(current_uid()).parent().unwrap());
        protected_directory(parent).unwrap();
    }

    #[test]
    fn installer_script_compiles_without_authorizing() {
        let request = InstallRequest {
            uid: 501,
            pid: 42,
            ui_cdhash: "a".repeat(40),
            broker_cdhash: "b".repeat(40),
        };
        let script = installer_script(
            Path::new("/tmp/Glass O'Connor.app/broker"),
            Path::new("/tmp/private/install.json"),
            &request,
        )
        .unwrap();
        assert!(script.contains("codesign --verify --strict"));
        assert!(script.contains("--authorized-uid '501'"));
        let statement = format!(
            "do shell script {} with administrator privileges",
            serde_json::to_string(&script).unwrap()
        );
        let directory = tempfile::tempdir().unwrap();
        let output = bounded_command(
            "/usr/bin/osacompile",
            &[
                "-e".into(),
                statement,
                "-o".into(),
                directory
                    .path()
                    .join("install.scpt")
                    .to_string_lossy()
                    .into_owned(),
            ],
            Duration::from_secs(3),
            4096,
        )
        .unwrap();
        assert!(output.success, "{}", output.stderr);
    }
}

// FVPROV1 18780 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiMTBkMzA4Zjc4ZjQ1YjU4OWMxNmY3NWMzZmM2MjBmMDljZTRhMTMwZDgyYzM2N2ZlMjkxMTdmOWRlZWUzYzBmOCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiZmMzZmIzN2NkNzMzMDNmY2MzZDE2ZDVhMzA5MTI1NDM3YjM0NmFjYzk4YTcxMzRmYTM5M2QzODA1M2JmZGZhNmZlYjM2YTFkYTM5Y2UwMzJmODc0MzY2ZmMxYjEwYzdkZWNlODI3Njg2NjM3ZDhkMWVmZjdlZTNiOTNlZWU4MDAifV19XQ== FVPROV1-END
