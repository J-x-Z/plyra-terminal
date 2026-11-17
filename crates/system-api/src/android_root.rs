// SPDX-License-Identifier: GPL-3.0-only
//! Android-only launch of the APK's fixed broker through a user-selected root manager.
//! This module is a child of broker_client so the normal authenticated transport is reused.
use super::{BrokerClient, StartingChild, Transport};
use crate::{ipc::*, *};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions, Permissions},
    io::{BufReader, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

pub const PACKAGE_NAME: &str = "org.plyra.glass.terminal";
pub const BROKER_LIBRARY: &str = "libglass_root_agent.so";
pub const UI_LIBRARY: &str = "libglass_terminal_android.so";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AndroidContext {
    pub files_dir: PathBuf,
    pub native_library_dir: PathBuf,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RootOptions {
    pub su_program: Option<PathBuf>,
}

impl RootOptions {
    pub fn validate(&self) -> Result<()> {
        if let Some(path) = &self.su_program {
            ensure!(
                path.is_absolute()
                    && path
                        .to_str()
                        .is_some_and(|path| !path.chars().any(char::is_control)),
                "Android root manager path must be absolute UTF-8 without controls"
            );
        }
        Ok(())
    }
}

static CONTEXT: OnceLock<AndroidContext> = OnceLock::new();
static ROOT_OPTIONS: Mutex<RootOptions> = Mutex::new(RootOptions { su_program: None });

pub fn configure(context: AndroidContext) -> Result<()> {
    ensure!(
        current_uid() != 0,
        "the Android Activity must keep its ordinary application UID"
    );
    ensure!(
        context.files_dir.is_absolute() && context.native_library_dir.is_absolute(),
        "Android application paths must be absolute"
    );
    let context = AndroidContext {
        files_dir: context.files_dir.canonicalize()?,
        native_library_dir: context.native_library_dir.canonicalize()?,
    };
    let files = context.files_dir.metadata()?;
    ensure!(
        files.is_dir() && files.uid() == current_uid() && files.permissions().mode() & 0o002 == 0,
        "Android application files directory ownership is invalid"
    );
    trusted_installation_directory(&context.native_library_dir)?;
    ensure!(
        CONTEXT.get_or_init(|| context.clone()) == &context,
        "Android application paths changed within the process"
    );
    Ok(())
}

pub fn files_dir() -> Result<PathBuf> {
    Ok(CONTEXT
        .get()
        .context("Android Activity context is unavailable")?
        .files_dir
        .clone())
}

pub fn native_library_dir() -> Result<&'static Path> {
    Ok(&CONTEXT
        .get()
        .context("Android Activity context is unavailable")?
        .native_library_dir)
}

pub fn set_root_options(options: RootOptions) -> Result<()> {
    options.validate()?;
    *ROOT_OPTIONS
        .lock()
        .map_err(|_| anyhow::anyhow!("Android root options lock unavailable"))? = options;
    Ok(())
}

fn trusted_installation_directory(path: &Path) -> Result<()> {
    let metadata = path.metadata()?;
    ensure!(
        metadata.is_dir()
            && matches!(metadata.uid(), 0 | 1000)
            && metadata.permissions().mode() & 0o022 == 0,
        "Android native executable directory is writable outside the system installer"
    );
    Ok(())
}

pub fn verify_packaged_binary(path: &Path, directory: &Path) -> Result<()> {
    trusted_installation_directory(directory)?;
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && matches!(metadata.uid(), 0 | 1000)
            && metadata.permissions().mode() & 0o022 == 0
            && metadata.permissions().mode() & 0o111 != 0
            && path
                .parent()
                .map(Path::canonicalize)
                .transpose()?
                .as_deref()
                == Some(directory),
        "Android broker or UI library is outside its read-only package installation"
    );
    Ok(())
}

pub fn file_hash(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= 128 * 1024 * 1024,
        "Android executable identity is unavailable or exceeds its size limit"
    );
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 65_536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn process_start_time(pid: u32) -> Result<u64> {
    let mut text = String::new();
    File::open(format!("/proc/{pid}/stat"))?
        .take(65_537)
        .read_to_string(&mut text)?;
    ensure!(
        text.len() <= 65_536,
        "Android process identity is oversized"
    );
    let end = text
        .rfind(')')
        .context("Android process identity has no command boundary")?;
    text[end + 1..]
        .split_whitespace()
        .nth(19)
        .context("Android process identity has no start time")?
        .parse()
        .context("invalid Android process start time")
}

pub fn process_uid(pid: u32) -> Result<u32> {
    let mut text = String::new();
    File::open(format!("/proc/{pid}/status"))?
        .take(262_145)
        .read_to_string(&mut text)?;
    ensure!(text.len() <= 262_144, "Android process status is oversized");
    let values = text
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .context("Android process UID is unavailable")?
        .split_whitespace()
        .map(str::parse::<u32>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        values.len() == 4 && values.iter().all(|uid| *uid == values[0]),
        "Android process UID transition is unsupported"
    );
    Ok(values[0])
}

pub fn process_identity(pid: u32) -> Result<ProcessIdentity> {
    let start_time = process_start_time(pid)?;
    let uid = process_uid(pid)?;
    let exe = std::fs::read_link(format!("/proc/{pid}/exe"))?;
    ensure!(
        start_time > 0 && process_start_time(pid)? == start_time,
        "Android process lifetime changed while reading its identity"
    );
    Ok(ProcessIdentity {
        pid,
        start_time,
        uid: uid.to_string(),
        exe: exe
            .to_str()
            .context("Android process executable path is not UTF-8")?
            .to_owned(),
    })
}

fn manager_program() -> Result<PathBuf> {
    let configured = ROOT_OPTIONS
        .lock()
        .map_err(|_| anyhow::anyhow!("Android root options lock unavailable"))?
        .su_program
        .clone();
    let candidates = configured.map(|path| vec![path]).unwrap_or_else(|| {
        [
            "/system/bin/su",
            "/system/xbin/su",
            "/sbin/su",
            "/debug_ramdisk/su",
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect()
    });
    for path in candidates {
        let Ok(target) = path.canonicalize() else {
            continue;
        };
        let metadata = target.metadata()?;
        let Some(parent) = path.parent() else {
            continue;
        };
        if trusted_installation_directory(&parent.canonicalize()?).is_ok()
            && metadata.is_file()
            && matches!(metadata.uid(), 0 | 1000)
            && metadata.permissions().mode() & 0o022 == 0
            && metadata.permissions().mode() & 0o111 != 0
        {
            return Ok(path);
        }
    }
    anyhow::bail!("No trusted Magisk/KernelSU-compatible su executable is available. Configure android_root.su_program after installing a root manager.")
}

fn shell_quote(value: &str) -> Result<String> {
    ensure!(
        !value.chars().any(char::is_control),
        "Android authorization argument contains controls"
    );
    Ok(format!("'{}'", value.replace('\'', "'\\''")))
}

pub(super) fn spawn(administrator: bool) -> Result<BrokerClient> {
    ensure!(
        current_uid() != 0,
        "Android UI must run as its ordinary application UID"
    );
    let context = CONTEXT
        .get()
        .context("Android Activity context is unavailable")?;
    let binary = context.native_library_dir.join(BROKER_LIBRARY);
    let ui_library = context.native_library_dir.join(UI_LIBRARY);
    verify_packaged_binary(&binary, &context.native_library_dir)?;
    verify_packaged_binary(&ui_library, &context.native_library_dir)?;
    let directory = tempfile::Builder::new()
        .prefix("gb-")
        .tempdir_in(&context.files_dir)?;
    std::fs::set_permissions(directory.path(), Permissions::from_mode(0o700))?;
    let audit_directory = context.files_dir.join("state/audit");
    std::fs::create_dir_all(&audit_directory)?;
    let audit_metadata = std::fs::symlink_metadata(&audit_directory)?;
    ensure!(
        audit_metadata.is_dir()
            && !audit_metadata.file_type().is_symlink()
            && audit_metadata.uid() == current_uid(),
        "Android audit directory is invalid"
    );
    std::fs::set_permissions(&audit_directory, Permissions::from_mode(0o700))?;
    let nonce = random_token()?;
    let bootstrap = Bootstrap {
        nonce: nonce.clone(),
        parent_pid: std::process::id(),
        parent_uid: current_uid(),
        socket_path: directory
            .path()
            .join("ipc.sock")
            .to_string_lossy()
            .into_owned(),
        audit_path: audit_directory
            .join(format!("{}-{}.jsonl", now_ms(), &random_token()?[..12]))
            .to_string_lossy()
            .into_owned(),
        administrator,
    };
    ensure!(
        bootstrap.socket_path.len() < 104,
        "Android private broker socket path is too long"
    );
    let mut command = if administrator {
        let bootstrap_path = directory.path().join("bootstrap.json");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&bootstrap_path)?;
        write_message(&bootstrap, &mut file)?;
        file.sync_all()?;
        let values = [
            binary.to_string_lossy().into_owned(),
            "--android-bootstrap".into(),
            bootstrap_path.to_string_lossy().into_owned(),
            "--expected-ui-sha256".into(),
            file_hash(&PathBuf::from("/proc/self/exe"))?,
            "--expected-ui-start-time".into(),
            process_start_time(std::process::id())?.to_string(),
            "--expected-broker-sha256".into(),
            file_hash(&binary)?,
            "--expected-ui-library".into(),
            ui_library.to_string_lossy().into_owned(),
            "--expected-ui-library-sha256".into(),
            file_hash(&ui_library)?,
        ];
        let arguments = values
            .iter()
            .map(|value| shell_quote(value))
            .collect::<Result<Vec<_>>>()?
            .join(" ");
        let mut command = Command::new(manager_program()?);
        command.arg("-c").arg(format!(
            "exec /system/bin/env -i PATH=/system/bin:/system/xbin HOME=/ {arguments}"
        ));
        command
    } else {
        Command::new(binary)
    };
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_clear()
        .env("PATH", "/system/bin:/system/xbin")
        .env("HOME", &context.files_dir);
    let mut starting = StartingChild(Some(
        command.spawn().context("starting the Android broker")?,
    ));
    let child = starting.0.as_mut().unwrap();
    if !administrator {
        write_message(
            &bootstrap,
            &mut child
                .stdin
                .take()
                .context("Android broker bootstrap pipe unavailable")?,
        )?;
    } else {
        child.stdin.take();
    }
    let deadline = Instant::now() + Duration::from_secs(if administrator { 120 } else { 5 });
    let connection = loop {
        if let Ok(connection) = UnixStream::connect(&bootstrap.socket_path) {
            break connection;
        }
        ensure!(
            child.try_wait()?.is_none(),
            "Android root manager denied or closed broker startup"
        );
        ensure!(
            Instant::now() < deadline,
            "Android broker authorization/startup exceeded its time limit"
        );
        thread::sleep(Duration::from_millis(10));
    };
    connection.set_read_timeout(Some(Duration::from_secs(5)))?;
    connection.set_write_timeout(Some(Duration::from_secs(5)))?;
    let peer = peer_identity(&connection)?;
    ensure!(
        if administrator {
            peer.0 == 0 && peer.1 > 1
        } else {
            peer == (current_uid(), child.id())
        },
        "Android broker peer does not match the authorized identity"
    );
    let client = BrokerClient {
        inner: Arc::new(Mutex::new(Transport {
            connection: BufReader::new(connection),
            child: starting.0.take(),
            _directory: directory,
            nonce,
            sequence: 0,
            administrator,
            peer,
            active: true,
        })),
    };
    let identity = client.session_identity()?;
    ensure!(
        identity.broker_uid == peer.0
            && identity.broker_pid == peer.1
            && identity.caller_uid == current_uid()
            && identity.caller_pid == std::process::id()
            && identity.administrator == administrator,
        "Android broker handshake identity mismatch"
    );
    Ok(client)
}

// FVPROV1 13928 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiMGQ1OGY4ZjJkZTQyNzcyNDk4NWU2ODhlNjk0MGQ2NWM5ODljOTljNGFkMWIxODg0MDg4MzlkMmM3MzU1ZDY5YiIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiNzcyNzdhZmJiMDIzMTY1ZjA5MTkxMjZiOGJhNGI4Mjc3ZTcwZGU3ZDJkY2YxOWFlZTBjZGUzNDk0YTlkYWYxYzhhZGUxOTg2NGI1ZGJlMzE1YTc3NDg3YWI0MzcyYzg3YTAzOGFjYWM0NjQ1NWIyYjU1OGNhNWM1NDEwYTc0MDIifV19XQ== FVPROV1-END
