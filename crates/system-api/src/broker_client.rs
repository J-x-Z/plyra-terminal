// SPDX-License-Identifier: GPL-3.0-only
#[cfg(target_os = "android")]
#[path = "android_root.rs"]
pub mod android_root;

use crate::{ipc::*, *};
#[cfg(unix)]
use anyhow::Context;
use anyhow::{ensure, Result};
use std::sync::{Arc, Mutex};
#[cfg(unix)]
use std::{
    io::BufReader,
    thread,
    time::{Duration, Instant},
};
#[cfg(all(unix, not(target_os = "android")))]
use std::{
    path::PathBuf,
    process::{Command, Stdio},
};

#[derive(Clone)]
pub struct BrokerClient {
    inner: Arc<Mutex<Transport>>,
}

#[cfg(unix)]
struct Transport {
    connection: BufReader<std::os::unix::net::UnixStream>,
    child: Option<std::process::Child>,
    _directory: tempfile::TempDir,
    nonce: String,
    sequence: u64,
    administrator: bool,
    peer: (u32, u32),
    active: bool,
    #[cfg(target_os = "macos")]
    persistent: bool,
}
#[cfg(windows)]
type Transport = crate::windows::WindowsTransport;
#[cfg(not(any(unix, windows)))]
struct Transport;

#[cfg(unix)]
struct StartingChild(Option<std::process::Child>);
#[cfg(unix)]
impl Drop for StartingChild {
    fn drop(&mut self) {
        if let Some(child) = self.0.take() {
            close_owned_child(child);
        }
    }
}

#[cfg(unix)]
fn close_owned_child(mut child: std::process::Child) {
    child.stdin.take();
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    child.kill().ok();
    thread::spawn(move || {
        let _ = child.wait();
    });
}

#[cfg(target_os = "linux")]
fn launcher_details(file: &mut std::fs::File) -> String {
    use std::io::Read;
    let mut bytes = Vec::new();
    let _ = std::io::Seek::rewind(file);
    let _ = Read::by_ref(file).take(8192).read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes)
        .trim()
        .chars()
        .take(2048)
        .collect()
}

#[cfg(target_os = "linux")]
struct LinuxTtyAgent {
    child: Option<std::process::Child>,
}

#[cfg(target_os = "linux")]
impl LinuxTtyAgent {
    fn start_if_foreground(diagnostics: &mut std::fs::File) -> Result<Option<Self>> {
        use std::os::{
            fd::{AsRawFd, FromRawFd, OwnedFd},
            unix::{fs::MetadataExt, process::CommandExt},
        };
        let terminal = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
        {
            Ok(terminal) => terminal,
            Err(_) => return Ok(None),
        };
        if unsafe { libc::tcgetpgrp(terminal.as_raw_fd()) } != unsafe { libc::getpgrp() } {
            return Ok(None);
        }
        let binary = "/usr/bin/pkttyagent";
        let metadata = std::fs::symlink_metadata(binary)
            .context("terminal authentication requires /usr/bin/pkttyagent")?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == 0
                && metadata.mode() & 0o022 == 0
                && metadata.mode() & 0o6000 == 0
                && metadata.mode() & 0o111 != 0,
            "terminal authentication agent must be a protected root-owned executable"
        );
        let mut descriptors = [-1; 2];
        ensure!(
            unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) }
                == 0,
            "cannot create authentication readiness pipe: {}",
            std::io::Error::last_os_error()
        );
        let reader = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
        let writer = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
        let notify_fd = writer.as_raw_fd();
        let pid = std::process::id();
        let start = crate::linux_admin::process_start_time(pid)?;
        let mut command = Command::new(binary);
        command
            .args([
                "--fallback",
                "--process",
                &format!("{pid},{start}"),
                "--notify-fd",
                &notify_fd.to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(diagnostics.try_clone()?))
            .env_clear()
            .env("PATH", "/usr/bin:/bin");
        for name in ["HOME", "LANG", "LC_ALL", "TERM", "COLORTERM"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(notify_fd, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut agent = Self {
            child: Some(
                command
                    .spawn()
                    .context("starting the terminal authentication agent")?,
            ),
        };
        drop(writer);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut descriptor = libc::pollfd {
            fd: reader.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            if let Some(status) = agent.child.as_mut().unwrap().try_wait()? {
                anyhow::bail!(
                    "terminal authentication agent exited before registration ({status})\n{}",
                    launcher_details(diagnostics)
                );
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(
                !remaining.is_zero(),
                "terminal authentication agent registration exceeded five seconds"
            );
            let count =
                unsafe { libc::poll(&mut descriptor, 1, remaining.as_millis().max(1) as i32) };
            if count < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error)
                    .context("waiting for terminal authentication agent registration");
            }
            ensure!(
                descriptor.revents & (libc::POLLERR | libc::POLLNVAL) == 0,
                "terminal authentication readiness descriptor failed"
            );
            if descriptor.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                let mut byte = 0_u8;
                let read =
                    unsafe { libc::read(reader.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) };
                if read == 0 {
                    ensure!(
                        agent.child.as_mut().unwrap().try_wait()?.is_none(),
                        "terminal authentication agent exited during registration\n{}",
                        launcher_details(diagnostics)
                    );
                    return Ok(Some(agent));
                }
                if read < 0 {
                    let error = std::io::Error::last_os_error();
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                    ) {
                        continue;
                    }
                    return Err(error).context("reading terminal authentication readiness");
                }
                anyhow::bail!("terminal authentication agent sent unexpected readiness data");
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for LinuxTtyAgent {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        // pkttyagent handles SIGTERM by restoring terminal echo before exit.
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
        }
        close_owned_child(child);
    }
}

impl BrokerClient {
    pub fn spawn() -> Result<Self> {
        Self::spawn_mode(false)
    }

    /// Call only from an explicit UI action to request native administrator authorization.
    pub fn spawn_admin() -> Result<Self> {
        ensure!(
            cfg!(any(
                target_os = "macos",
                target_os = "linux",
                target_os = "android",
                windows
            )),
            "native administrator authorization is unavailable on this platform"
        );
        Self::spawn_mode(true)
    }

    /// Connects an already installed helper without showing authentication UI.
    #[cfg(target_os = "macos")]
    pub fn connect_persistent_admin() -> Result<Self> {
        use std::os::unix::{
            fs::{MetadataExt, PermissionsExt},
            net::UnixStream,
        };
        let uid = current_uid();
        ensure!(uid != 0, "the terminal UI must run as a normal user");
        let status = crate::macos_admin::persistent_status()?;
        ensure!(
            status.installed && status.compatible,
            "{}",
            status.description
        );
        let grant = crate::macos_admin::read_grant(uid)?;
        crate::macos_admin::validate_socket(uid)?;
        let connection = UnixStream::connect(crate::macos_admin::socket_path(uid))
            .context("remembered administrator helper is not available")?;
        connection.set_read_timeout(Some(Duration::from_secs(5)))?;
        connection.set_write_timeout(Some(Duration::from_secs(5)))?;
        let (peer_uid, peer_pid) = peer_identity(&connection)?;
        ensure!(
            peer_uid == 0 && running_code_hash(peer_pid)? == grant.broker_cdhash,
            "remembered administrator helper code identity mismatch"
        );
        let directory = tempfile::Builder::new()
            .prefix("glass-persistent-client-")
            .tempdir_in("/tmp")?;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
        let audit_directory = default_data_dir()?.join("audit");
        std::fs::create_dir_all(&audit_directory)?;
        let metadata = audit_directory.symlink_metadata()?;
        ensure!(
            metadata.is_dir() && metadata.uid() == uid,
            "invalid broker audit directory"
        );
        std::fs::set_permissions(&audit_directory, std::fs::Permissions::from_mode(0o700))?;
        let nonce = random_token()?;
        let bootstrap = Bootstrap {
            nonce: nonce.clone(),
            parent_pid: std::process::id(),
            parent_uid: uid,
            socket_path: directory
                .path()
                .join("ipc.sock")
                .to_string_lossy()
                .into_owned(),
            audit_path: audit_directory
                .join(format!("{}-{}.jsonl", now_ms(), &random_token()?[..12]))
                .to_string_lossy()
                .into_owned(),
            administrator: true,
        };
        let mut connection = BufReader::new(connection);
        write_message(&bootstrap, connection.get_mut())?;
        let client = Self {
            inner: Arc::new(Mutex::new(Transport {
                connection,
                child: None,
                _directory: directory,
                nonce,
                sequence: 0,
                administrator: true,
                peer: (peer_uid, peer_pid),
                active: true,
                persistent: true,
            })),
        };
        let identity = client
            .session_identity()
            .context("remembered administrator helper is busy or its handshake failed")?;
        ensure!(
            identity.administrator
                && identity.broker_uid == peer_uid
                && identity.broker_pid == peer_pid
                && identity.caller_uid == uid
                && identity.caller_pid == std::process::id(),
            "remembered administrator session identity mismatch"
        );
        Ok(client)
    }

    /// Install/update is reserved for an explicit user action.
    #[cfg(target_os = "macos")]
    pub fn install_persistent_admin() -> Result<Self> {
        crate::macos_admin::install_persistent_helper()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if crate::macos_admin::validate_socket(current_uid()).is_ok() {
                return Self::connect_persistent_admin();
            }
            ensure!(
                Instant::now() < deadline,
                "installed administrator helper did not publish its socket"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(target_os = "macos")]
    pub fn is_persistent_admin(&self) -> bool {
        let transport = self.inner.lock().unwrap();
        transport.persistent && transport.active
    }

    fn spawn_mode(administrator: bool) -> Result<Self> {
        #[cfg(target_os = "android")]
        {
            android_root::spawn(administrator)
        }
        #[cfg(all(unix, not(target_os = "android")))]
        {
            use std::os::unix::{
                fs::{MetadataExt, PermissionsExt},
                net::UnixStream,
            };
            ensure!(
                current_uid() != 0,
                "the terminal UI and ordinary broker must run as a normal user"
            );
            let sibling = std::env::current_exe()?.with_file_name("plyra-privilege-broker");
            let binary = if administrator {
                #[cfg(target_os = "linux")]
                {
                    crate::linux_admin::verify_installation()?
                }
                #[cfg(not(target_os = "linux"))]
                {
                    sibling
                }
            } else {
                std::env::var_os("PLYRA_BROKER_BIN")
                    .map(PathBuf::from)
                    .unwrap_or(sibling)
            };
            ensure!(
                binary.is_absolute()
                    && binary.is_file()
                    && binary.metadata()?.permissions().mode() & 0o111 != 0,
                "broker executable is unavailable"
            );
            let directory = tempfile::Builder::new()
                .prefix("glass-broker-")
                .tempdir_in("/tmp")?;
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
            let audit_directory = default_data_dir()?.join("audit");
            std::fs::create_dir_all(&audit_directory)?;
            ensure!(
                !std::fs::symlink_metadata(&audit_directory)?
                    .file_type()
                    .is_symlink()
                    && audit_directory.metadata()?.uid() == current_uid(),
                "invalid broker audit directory"
            );
            std::fs::set_permissions(&audit_directory, std::fs::Permissions::from_mode(0o700))?;
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
            let expected_hash = if administrator {
                Some(code_hash_for_path(&binary)?)
            } else {
                None
            };
            let mut command = if administrator {
                use std::os::unix::fs::OpenOptionsExt;
                let bootstrap_path = directory.path().join("bootstrap.json");
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&bootstrap_path)?;
                write_message(&bootstrap, &mut file)?;
                file.sync_all()?;
                let ui_hash = running_code_hash(std::process::id())?;
                administrator_command(
                    &binary,
                    &bootstrap_path,
                    expected_hash.as_deref().unwrap(),
                    &ui_hash,
                )?
            } else {
                Command::new(binary)
            };
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .env_clear()
                .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
            if let Some(home) = std::env::var_os("HOME") {
                command.env("HOME", home);
            }
            #[cfg(target_os = "linux")]
            {
                if administrator {
                    command.stdin(Stdio::null());
                } else {
                    for name in ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
                        if let Some(value) = std::env::var_os(name) {
                            command.env(name, value);
                        }
                    }
                }
            }
            #[cfg(target_os = "linux")]
            let mut launcher_diagnostics = tempfile::tempfile_in(directory.path())?;
            #[cfg(target_os = "linux")]
            command.stderr(Stdio::from(launcher_diagnostics.try_clone()?));
            let mut starting = StartingChild(None);
            #[cfg(target_os = "linux")]
            let authentication_agent = if administrator {
                LinuxTtyAgent::start_if_foreground(&mut launcher_diagnostics)?
            } else {
                None
            };
            starting.0 = Some(command.spawn().context("starting the private broker")?);
            let child = starting.0.as_mut().unwrap();
            if !administrator {
                write_message(
                    &bootstrap,
                    &mut child
                        .stdin
                        .take()
                        .context("broker bootstrap pipe is unavailable")?,
                )?;
            }
            let deadline =
                Instant::now() + Duration::from_secs(if administrator { 120 } else { 5 });
            let connection = loop {
                if let Ok(connection) = UnixStream::connect(&bootstrap.socket_path) {
                    break connection;
                }
                #[cfg(target_os = "linux")]
                if let Some(status) = child.try_wait()? {
                    let details = launcher_details(&mut launcher_diagnostics);
                    anyhow::bail!(
                        "broker exited before accepting its UI client ({status})\n{details}"
                    );
                }
                #[cfg(not(target_os = "linux"))]
                ensure!(
                    child.try_wait()?.is_none(),
                    "broker exited before accepting its UI client"
                );
                if Instant::now() >= deadline {
                    anyhow::bail!("broker startup exceeded its time limit");
                }
                thread::sleep(Duration::from_millis(10));
            };
            #[cfg(target_os = "linux")]
            drop(authentication_agent);
            connection.set_read_timeout(Some(Duration::from_secs(5)))?;
            connection.set_write_timeout(Some(Duration::from_secs(5)))?;
            let (uid, pid) = peer_identity(&connection)?;
            if administrator {
                ensure!(
                    uid == 0,
                    "administrator broker did not obtain platform authorization"
                );
                #[cfg(target_os = "macos")]
                ensure!(
                    running_code_hash(pid)? == expected_hash.clone().unwrap(),
                    "administrator broker code identity mismatch"
                );
                #[cfg(target_os = "linux")]
                ensure!(
                    pid == child.id(),
                    "administrator broker is not the owned pkexec process"
                );
            } else {
                ensure!(
                    uid == current_uid() && pid == child.id(),
                    "broker server identity does not match the owned child"
                );
            }
            let client = Self {
                inner: Arc::new(Mutex::new(Transport {
                    connection: BufReader::new(connection),
                    child: starting.0.take(),
                    _directory: directory,
                    nonce,
                    sequence: 0,
                    administrator,
                    peer: (uid, pid),
                    active: true,
                    #[cfg(target_os = "macos")]
                    persistent: false,
                })),
            };
            let identity = client.session_identity()?;
            ensure!(
                identity.broker_uid == uid
                    && identity.broker_pid == pid
                    && identity.caller_uid == current_uid()
                    && identity.caller_pid == std::process::id()
                    && identity.administrator == administrator,
                "broker authenticated session identity mismatch"
            );
            Ok(client)
        }
        #[cfg(windows)]
        {
            Ok(Self {
                inner: Arc::new(Mutex::new(Transport::spawn(administrator)?)),
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            anyhow::bail!("kernel-authenticated broker transport is unavailable on this platform")
        }
    }

    fn request(&self, request: Request) -> Result<Response> {
        #[cfg(unix)]
        {
            let mut transport = self.inner.lock().unwrap();
            let timeout = if transport.sequence == 0 {
                Duration::from_secs(5)
            } else {
                Duration::from_secs(30)
            };
            transport.request(request, timeout)
        }
        #[cfg(windows)]
        {
            self.inner.lock().unwrap().request(request)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = request;
            anyhow::bail!("broker transport unavailable")
        }
    }

    pub fn prepare(&self, action: PrivilegedAction) -> Result<PreparedAction> {
        let request = match action {
            #[cfg(target_os = "linux")]
            PrivilegedAction::StopProcess(expected) if expected.exe.is_empty() => {
                Request::PrepareProcessStop { expected }
            }
            action => Request::Prepare(action),
        };
        match self.request(request)? {
            Response::Prepared(value) => Ok(value),
            _ => anyhow::bail!("invalid broker preparation response"),
        }
    }
    pub fn confirm(&self, ticket: &str) -> Result<OperationResult> {
        ensure!(ticket.len() == 64, "invalid confirmation ticket");
        match self.request(Request::Confirm {
            ticket: ticket.into(),
        })? {
            Response::Completed(value) => Ok(value),
            _ => anyhow::bail!("invalid broker execution response"),
        }
    }
    pub fn revoke(&self) -> Result<()> {
        ensure!(
            matches!(self.request(Request::Revoke)?, Response::Revoked),
            "invalid broker revocation response"
        );
        #[cfg(unix)]
        {
            let mut transport = self.inner.lock().unwrap();
            if transport.administrator {
                transport.active = false;
                let _ = transport
                    .connection
                    .get_mut()
                    .shutdown(std::net::Shutdown::Both);
            }
        }
        Ok(())
    }

    pub fn cancel_pending(&self) -> Result<()> {
        ensure!(
            matches!(self.request(Request::CancelPending)?, Response::Cancelled),
            "invalid pending authorization cancellation response"
        );
        Ok(())
    }
    pub fn session_identity(&self) -> Result<BrokerSessionIdentity> {
        match self.request(Request::Identity)? {
            Response::Identity(identity) => Ok(identity),
            _ => anyhow::bail!("invalid broker identity response"),
        }
    }
    pub fn is_connected(&self) -> bool {
        #[cfg(unix)]
        {
            self.inner.lock().unwrap().active
        }
        #[cfg(windows)]
        {
            self.inner.lock().unwrap().is_connected()
        }
        #[cfg(not(any(unix, windows)))]
        {
            false
        }
    }

    pub fn is_admin(&self) -> bool {
        #[cfg(unix)]
        {
            let transport = self.inner.lock().unwrap();
            transport.administrator && transport.active
        }
        #[cfg(windows)]
        {
            self.inner.lock().unwrap().is_admin()
        }
        #[cfg(not(any(unix, windows)))]
        {
            false
        }
    }
    pub fn peer_identity(&self) -> (u32, u32) {
        #[cfg(unix)]
        {
            self.inner.lock().unwrap().peer
        }
        #[cfg(windows)]
        {
            self.inner.lock().unwrap().peer_identity()
        }
        #[cfg(not(any(unix, windows)))]
        {
            (u32::MAX, 0)
        }
    }
    pub fn peer_principal_label(&self) -> String {
        #[cfg(windows)]
        {
            self.inner.lock().unwrap().peer_principal_label()
        }
        #[cfg(not(windows))]
        {
            format!("UID {}", self.peer_identity().0)
        }
    }
    pub fn audit(&self) -> Result<Vec<AuditRecord>> {
        match self.request(Request::Audit)? {
            Response::Audit(value) => Ok(value),
            _ => anyhow::bail!("invalid broker audit response"),
        }
    }

    pub fn power_sample(&self, interval_ms: u16) -> Result<PowerSample> {
        anyhow::ensure!(
            (500..=2000).contains(&interval_ms),
            "power interval must be 500–2000 ms"
        );
        match self.request(Request::PowerSample { interval_ms })? {
            Response::PowerSample(value) => Ok(value),
            _ => anyhow::bail!("invalid broker power response"),
        }
    }
}

#[cfg(all(unix, not(target_os = "android")))]
fn administrator_command(
    binary: &std::path::Path,
    bootstrap: &std::path::Path,
    broker_hash: &str,
    ui_hash: &str,
) -> Result<Command> {
    #[cfg(target_os = "macos")]
    {
        let script = administrator_script(binary, bootstrap, broker_hash, ui_hash)?;
        let mut command = Command::new("/usr/bin/osascript");
        command.args([
            "-e",
            &format!(
                "do shell script {} with administrator privileges",
                serde_json::to_string(&script)?
            ),
        ]);
        Ok(command)
    }
    #[cfg(target_os = "linux")]
    {
        let _ = broker_hash;
        ensure!(
            binary == std::path::Path::new(crate::linux_admin::BROKER_PATH),
            "administrator helper path is not the installed path"
        );
        ensure!(
            ui_hash.len() == 64 && ui_hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid UI executable identity"
        );
        let start = crate::linux_admin::process_start_time(std::process::id())?;
        let mut command = Command::new(crate::linux_admin::PKEXEC_PATH);
        command
            .arg("--disable-internal-agent")
            .arg(binary)
            .arg("--polkit-bootstrap")
            .arg(bootstrap)
            .arg("--expected-ui-sha256")
            .arg(ui_hash)
            .arg("--expected-ui-start-time")
            .arg(start.to_string());
        Ok(command)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (binary, bootstrap, broker_hash, ui_hash);
        anyhow::bail!("native administrator launcher is unavailable on this platform")
    }
}

#[cfg(target_os = "macos")]
fn administrator_script(
    binary: &std::path::Path,
    bootstrap: &std::path::Path,
    broker_hash: &str,
    ui_hash: &str,
) -> Result<String> {
    ensure!(
        [broker_hash, ui_hash]
            .iter()
            .all(|hash| (40..=64).contains(&hash.len())
                && hash.bytes().all(|byte| byte.is_ascii_hexdigit())),
        "invalid signed-code identity"
    );
    let quote = |path: &std::path::Path| -> Result<String> {
        let value = path.to_str().context("administrator path is not UTF-8")?;
        ensure!(
            !value.chars().any(char::is_control),
            "administrator path contains controls"
        );
        Ok(format!("'{}'", value.replace('\'', "'\\''")))
    };
    // Security.framework reads the peer's executable when checking its running
    // signature. The stage contains only public code, so allow traversal and
    // code reads while keeping every path root-owned and non-writable by users.
    // Bootstrap credentials and the socket live in the separate private UI dir.
    Ok(format!("set -eu\numask 077\nstage=$(/usr/bin/mktemp -d /private/tmp/glass-admin.XXXXXXXX)\ntrap '/bin/rm -f \"$stage/broker\"; /bin/rmdir \"$stage\"' EXIT\n/usr/bin/install -m 555 {} \"$stage/broker\"\n/usr/bin/codesign --verify --strict \"$stage/broker\"\n/usr/bin/codesign -d --verbose=4 \"$stage/broker\" 2>&1 | /usr/bin/grep -F -x 'CDHash={broker_hash}' >/dev/null\n/bin/chmod -N \"$stage\" \"$stage/broker\"\n/bin/chmod 711 \"$stage\"\n/usr/bin/env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin HOME=/var/root \"$stage/broker\" --admin-bootstrap {} --expected-ui-cdhash '{ui_hash}'", quote(binary)?, quote(bootstrap)?))
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;

    #[test]
    fn polkit_command_uses_fixed_helper_and_separate_arguments() {
        let hash = "a".repeat(64);
        let command = administrator_command(
            std::path::Path::new(crate::linux_admin::BROKER_PATH),
            std::path::Path::new("/tmp/owned path/bootstrap.json"),
            &hash,
            &hash,
        )
        .unwrap();
        assert_eq!(command.get_program(), crate::linux_admin::PKEXEC_PATH);
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect();
        assert_eq!(
            &args[..6],
            &[
                "--disable-internal-agent",
                crate::linux_admin::BROKER_PATH,
                "--polkit-bootstrap",
                "/tmp/owned path/bootstrap.json",
                "--expected-ui-sha256",
                &hash
            ]
        );
        assert_eq!(args[6], "--expected-ui-start-time");
        assert!(args[7].parse::<u64>().unwrap() > 0);
        assert!(administrator_command(
            std::path::Path::new("/tmp/user-writable/broker"),
            std::path::Path::new("/tmp/bootstrap.json"),
            &hash,
            &hash,
        )
        .is_err());
    }

    #[test]
    #[ignore = "requires a foreground controlling PTY and running polkit; agent registration only"]
    fn terminal_agent_registration_and_cleanup_without_authorization() {
        let mut diagnostics = tempfile::tempfile().unwrap();
        let mut agent = LinuxTtyAgent::start_if_foreground(&mut diagnostics)
            .unwrap()
            .expect("test requires a foreground controlling PTY");
        let pid = agent.child.as_ref().unwrap().id();
        assert!(agent.child.as_mut().unwrap().try_wait().unwrap().is_none());
        assert_eq!(
            crate::process_identity(pid).unwrap().uid,
            current_uid().to_string()
        );
        drop(agent);
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }
}

#[cfg(unix)]
impl Transport {
    fn request(&mut self, request: Request, timeout: Duration) -> Result<Response> {
        ensure!(
            self.active,
            "broker connection is closed or authorization was revoked"
        );
        let confirming = matches!(&request, Request::Confirm { .. });
        let response = (|| -> Result<Response> {
            self.connection.get_mut().set_read_timeout(Some(timeout))?;
            self.sequence = self
                .sequence
                .checked_add(1)
                .context("broker sequence exhausted")?;
            let envelope = Envelope {
                nonce: self.nonce.clone(),
                sequence: self.sequence,
                request,
            };
            write_message(&envelope, self.connection.get_mut())?;
            read_message(&mut self.connection)?.context("broker closed its authenticated session")
        })();
        let response = response.map_err(|error| {
            // A partial frame or late response can no longer be matched to its
            // request. Close this connection before any subsequent operation.
            self.active = false;
            let _ = self.connection.get_mut().shutdown(std::net::Shutdown::Both);
            if confirming {
                error.context("Broker connection closed while awaiting operation result. The outcome may already be recorded in Audit; reconnect to inspect it before another operation")
            } else {
                error.context("Broker connection closed after a transport or response error; reconnect to continue")
            }
        })?;
        if let Response::Error(message) = &response {
            anyhow::bail!("{message}");
        }
        Ok(response)
    }
}

#[cfg(unix)]
impl Drop for Transport {
    fn drop(&mut self) {
        let _ = self.connection.get_mut().shutdown(std::net::Shutdown::Both);
        if let Some(child) = self.child.take() {
            close_owned_child(child);
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    fn client_pair(sequence: u64) -> (BrokerClient, std::os::unix::net::UnixStream) {
        let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
        let broker = BrokerClient {
            inner: Arc::new(Mutex::new(Transport {
                connection: BufReader::new(client),
                child: None,
                _directory: tempfile::tempdir().unwrap(),
                nonce: "a".repeat(64),
                sequence,
                administrator: true,
                peer: (current_uid(), std::process::id()),
                active: true,
                persistent: false,
            })),
        };
        (broker, server)
    }

    fn identity_response() -> Response {
        Response::Identity(BrokerSessionIdentity {
            broker_uid: current_uid(),
            broker_pid: std::process::id(),
            caller_uid: current_uid(),
            caller_pid: std::process::id(),
            administrator: true,
            broker_sid: None,
            caller_sid: None,
        })
    }

    #[test]
    fn handshake_and_established_requests_use_separate_response_budgets() {
        let (broker, server) = client_pair(0);
        let worker = thread::spawn(move || {
            let mut reader = BufReader::new(server);
            let first: Envelope = read_message(&mut reader).unwrap().unwrap();
            assert!(matches!(first.request, Request::Identity));
            write_message(&identity_response(), reader.get_mut()).unwrap();
            let second: Envelope = read_message(&mut reader).unwrap().unwrap();
            assert!(matches!(second.request, Request::Audit));
            thread::sleep(Duration::from_millis(30));
            write_message(&Response::Audit(Vec::new()), reader.get_mut()).unwrap();
        });
        broker.session_identity().unwrap();
        assert_eq!(
            broker
                .inner
                .lock()
                .unwrap()
                .connection
                .get_ref()
                .read_timeout()
                .unwrap(),
            Some(Duration::from_secs(5))
        );
        broker.audit().unwrap();
        assert_eq!(
            broker
                .inner
                .lock()
                .unwrap()
                .connection
                .get_ref()
                .read_timeout()
                .unwrap(),
            Some(Duration::from_secs(30))
        );
        assert!(broker.is_connected());
        worker.join().unwrap();
    }

    #[test]
    fn timed_out_confirmation_closes_before_a_late_response_or_next_request() {
        let (broker, server) = client_pair(1);
        let worker = thread::spawn(move || {
            let mut reader = BufReader::new(server);
            let request: Envelope = read_message(&mut reader).unwrap().unwrap();
            assert!(matches!(request.request, Request::Confirm { .. }));
            thread::sleep(Duration::from_millis(100));
            let late = Response::Completed(OperationResult {
                success: true,
                message: "mock operation completed".into(),
            });
            // A local peer can accept late bytes after shutdown. The inactive
            // client rejects subsequent requests before reading those bytes.
            let _ = write_message(&late, reader.get_mut());
            assert!(read_message::<Envelope>(&mut reader).unwrap().is_none());
        });
        let error = broker
            .inner
            .lock()
            .unwrap()
            .request(
                Request::Confirm {
                    ticket: "b".repeat(64),
                },
                Duration::from_millis(20),
            )
            .err()
            .unwrap();
        assert!(error.to_string().contains("Audit"));
        assert!(!broker.is_connected());
        assert!(!broker.is_admin());
        assert!(broker.revoke().is_err());
        worker.join().unwrap();
    }

    #[test]
    fn malformed_response_closes_the_connection() {
        use std::io::Write;
        let (broker, server) = client_pair(1);
        let worker = thread::spawn(move || {
            let mut reader = BufReader::new(server);
            read_message::<Envelope>(&mut reader).unwrap().unwrap();
            reader.get_mut().write_all(b"{invalid}\n").unwrap();
        });
        assert!(broker.audit().is_err());
        assert!(!broker.is_connected());
        worker.join().unwrap();
    }

    #[test]
    fn complete_policy_error_keeps_the_authenticated_connection() {
        let (broker, server) = client_pair(1);
        let worker = thread::spawn(move || {
            let mut reader = BufReader::new(server);
            read_message::<Envelope>(&mut reader).unwrap().unwrap();
            write_message(
                &Response::Error("mock policy denial".into()),
                reader.get_mut(),
            )
            .unwrap();
            let next: Envelope = read_message(&mut reader).unwrap().unwrap();
            assert!(matches!(next.request, Request::Identity));
            write_message(&identity_response(), reader.get_mut()).unwrap();
        });
        assert!(broker.audit().is_err());
        assert!(broker.is_connected());
        broker.session_identity().unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn administrator_script_compiles_without_executing_authorization() {
        let hash = "a".repeat(40);
        let script = administrator_script(
            std::path::Path::new("/tmp/Glass O'Connor.app/broker"),
            std::path::Path::new("/tmp/private bootstrap.json"),
            &hash,
            &hash,
        )
        .unwrap();
        assert!(script.contains("env -i"));
        assert!(script.contains("codesign --verify --strict"));
        assert!(script.contains("install -m 555"));
        assert!(script.contains("chmod 711"));
        assert!(script.contains("chmod -N"));
        assert!(script.contains("'\\''"));
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
                    .join("administrator.scpt")
                    .to_string_lossy()
                    .into_owned(),
            ],
            Duration::from_secs(3),
            65_536,
        )
        .unwrap();
        assert!(output.success, "{}", output.stderr);
        assert!(administrator_script(
            std::path::Path::new("/tmp/broker"),
            std::path::Path::new("/tmp/bootstrap"),
            "invalid",
            &hash
        )
        .is_err());
    }
}

// FVPROV1 40821 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiMTk5NDBiNTNkMzQ0MDllNDUyN2FjM2I5MTdmNWZkYmVkMGY5ZDNiMDlkODRhNGE2Y2E5ODVmZjI0YWU2N2I4YiIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiZjAyZDM4ZDc3YjlhZDVkY2YzYzBmMzI1NTMzYzkzNWRjNzVkMTJlMjg3N2JkZWJlN2NkZGRhNmMxYjU2OTg5YWQxYzVlNTAyNDEwYzAwMjM1ZDdmZjY0ZjY4Y2MyZTVjN2VmZWZmYzE1ZjMxNmRmYTgwOWM3NzlhMGIwN2NlMDAifV19XQ== FVPROV1-END
