// SPDX-License-Identifier: GPL-3.0-only
use crate::*;
use anyhow::{ensure, Context, Result};
use notify::Watcher;
use std::collections::VecDeque;
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use sysinfo::{Disks, Networks, System};

#[derive(Clone)]
pub struct SystemClient {
    system: Arc<Mutex<(System, Instant)>>,
    recovery: RecoveryStore,
    file_events: Arc<Mutex<VecDeque<FileEvent>>>,
    _watcher: Arc<Mutex<notify::RecommendedWatcher>>,
}

impl SystemClient {
    pub fn new() -> Result<Self> {
        Self::with_managed_root(default_data_dir()?.join("managed"))
    }

    pub fn with_managed_root(root: PathBuf) -> Result<Self> {
        let mut system = System::new_all();
        system.refresh_all();
        let recovery = RecoveryStore::new(root)?;
        let root = recovery.root().to_path_buf();
        let watched = root.clone();
        let file_events = Arc::new(Mutex::new(VecDeque::new()));
        let events = file_events.clone();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if let Ok(event) = event {
                    let paths: Vec<_> = event
                        .paths
                        .iter()
                        .filter(|path| path.parent() == Some(watched.as_path()))
                        .take(32)
                        .map(|path| path.to_string_lossy().into_owned())
                        .collect();
                    if paths.is_empty() {
                        return;
                    }
                    let mut queue = events.lock().unwrap();
                    queue.push_back(FileEvent {
                        sampled_at_ms: now_ms(),
                        source: "native filesystem watcher; managed directory only".into(),
                        kind: format!("{:?}", event.kind),
                        paths,
                    });
                    if queue.len() > 512 {
                        queue.pop_front();
                    }
                }
            })?;
        watcher.watch(&root, notify::RecursiveMode::NonRecursive)?;
        Ok(Self {
            system: Arc::new(Mutex::new((system, Instant::now()))),
            recovery,
            file_events,
            _watcher: Arc::new(Mutex::new(watcher)),
        })
    }

    pub fn snapshot(&self) -> Result<SystemSnapshot> {
        let mut guard = self.system.lock().unwrap();
        let elapsed = guard.1.elapsed();
        if elapsed < Duration::from_millis(220) {
            thread::sleep(Duration::from_millis(220) - elapsed);
        }
        guard.0.refresh_all();
        guard.1 = Instant::now();
        let mut processes: Vec<_> = guard.0.processes().values().map(process_info).collect();
        processes.sort_by(|left, right| right.cpu_percent.total_cmp(&left.cpu_percent));
        processes.truncate(10_000);
        drop(guard);
        let mut limitations = vec!["Protected process details can be unavailable. Network values are cumulative interface byte counters.".into()];
        let services = match self.services() {
            Ok(services) => services,
            Err(error) => {
                limitations.push(error.to_string());
                Vec::new()
            }
        };
        let network = Networks::new_with_refreshed_list()
            .iter()
            .map(|(name, value)| NetworkInterface {
                name: name.clone(),
                received_bytes: value.total_received(),
                transmitted_bytes: value.total_transmitted(),
            })
            .collect();
        let devices: Vec<DeviceInfo> = Disks::new_with_refreshed_list()
            .iter()
            .map(|disk| DeviceInfo {
                name: disk.name().to_string_lossy().into_owned(),
                kind: format!("storage {:?}", disk.kind()),
                details: format!(
                    "{} · {} · {} bytes available / {} bytes total",
                    disk.mount_point().display(),
                    disk.file_system().to_string_lossy(),
                    disk.available_space(),
                    disk.total_space()
                ),
            })
            .collect();
        #[cfg(target_os = "macos")]
        let mut devices = devices;
        #[cfg(target_os = "macos")]
        match bounded_command(
            "/usr/sbin/ioreg",
            &[
                "-r".into(),
                "-c".into(),
                "IOUSBHostDevice".into(),
                "-l".into(),
                "-w".into(),
                "0".into(),
            ],
            Duration::from_secs(2),
            512 * 1024,
        ) {
            Ok(output) if output.success => {
                for line in output.stdout.lines().filter(|line| line.contains("+-o ")) {
                    if let Some(name) = line
                        .split("+-o ")
                        .nth(1)
                        .and_then(|name| name.split('<').next())
                    {
                        devices.push(DeviceInfo {
                            name: name.trim().into(),
                            kind: "USB".into(),
                            details: "IOKit IOUSBHostDevice registry, read-only device enumeration"
                                .into(),
                        });
                    }
                }
            }
            Ok(_) => limitations.push("IOKit USB enumeration returned an error.".into()),
            Err(error) => limitations.push(error.to_string()),
        }
        Ok(SystemSnapshot {
            sampled_at_ms: now_ms(),
            source: if cfg!(target_os = "macos") { "sysinfo 0.33.1 native process/disk/network APIs; current-user launchd; IOKit USB registry" } else { "sysinfo 0.33.1 native process/disk/network APIs" }.into(),
            processes,
            services,
            network,
            devices,
            limitations,
        })
    }

    pub fn services(&self) -> Result<Vec<ServiceInfo>> {
        user_services()
    }

    pub fn logs(&self, query: LogQuery) -> Result<Vec<LogEntry>> {
        ensure!(
            (1..=300).contains(&query.seconds) && (1..=500).contains(&query.limit),
            "log query exceeds the 300-second/500-entry limit"
        );
        #[cfg(target_os = "macos")]
        {
            let mut predicates = Vec::new();
            if let Some(process) = query.process {
                predicates.push(format!("process == {}", predicate_string(&process)?));
            }
            if let Some(text) = query.contains {
                predicates.push(format!(
                    "eventMessage CONTAINS[c] {}",
                    predicate_string(&text)?
                ));
            }
            ensure!(
                !predicates.is_empty(),
                "log query requires a process or message filter"
            );
            let output = bounded_command(
                "/usr/bin/log",
                &[
                    "show".into(),
                    "--style".into(),
                    "json".into(),
                    "--last".into(),
                    format!("{}s", query.seconds),
                    "--predicate".into(),
                    predicates.join(" AND "),
                    "--info".into(),
                ],
                Duration::from_secs(5),
                4 * 1024 * 1024,
            )?;
            ensure!(
                output.success,
                "unified log query failed: {}",
                output.stderr
            );
            let value: serde_json::Value = serde_json::from_str(&output.stdout)
                .context("unified log response was not JSON")?;
            let records = value
                .as_array()
                .context("unified log response was not an array")?;
            Ok(records
                .iter()
                .rev()
                .take(query.limit)
                .rev()
                .map(|value| LogEntry {
                    timestamp: value["timestamp"].as_str().unwrap_or_default().into(),
                    process: value["processImagePath"]
                        .as_str()
                        .unwrap_or_default()
                        .into(),
                    message: value["eventMessage"]
                        .as_str()
                        .unwrap_or_default()
                        .chars()
                        .take(8192)
                        .collect(),
                })
                .collect())
        }
        #[cfg(target_os = "linux")]
        {
            crate::linux_system::logs(current_uid(), query)
        }
        #[cfg(windows)]
        {
            crate::windows::logs(query)
        }
        #[cfg(target_os = "android")]
        {
            crate::android_system::logs(query)
        }
        #[cfg(not(any(
            target_os = "macos",
            target_os = "linux",
            target_os = "android",
            windows
        )))]
        {
            anyhow::bail!("bounded native log queries are unavailable on this platform")
        }
    }

    #[cfg(target_os = "macos")]
    pub fn connection_resources(&self, snapshot: &SystemSnapshot) -> Result<Vec<ProcessResources>> {
        let output = bounded_command(
            "/usr/sbin/lsof",
            &[
                "-nP".into(),
                "-i".into(),
                "-a".into(),
                "-u".into(),
                current_uid().to_string(),
                "-F0pftnPT".into(),
            ],
            Duration::from_secs(3),
            1024 * 1024,
        )?;
        ensure!(
            output.success || (output.stdout.is_empty() && output.stderr.trim().is_empty()),
            "connection query failed: {}",
            output.stderr
        );
        let at_ms = now_ms();
        let uid = current_uid().to_string();
        let identities: std::collections::HashMap<_, _> = snapshot
            .processes
            .iter()
            .filter(|process| process.identity.uid == uid)
            .map(|process| (process.identity.pid, &process.identity))
            .collect();
        let mut captured = Vec::new();
        let mut count = 0;
        for (pid, resources) in parse_connection_groups(&output.stdout)? {
            let Some(expected) = identities.get(&pid) else {
                // The process must belong to the paired, current-user snapshot.
                continue;
            };
            let identity = process_identity(pid)
                .context("connection process exited or became unavailable; refresh again")?;
            ensure!(
                identity.start_time == expected.start_time
                    && identity.uid == expected.uid
                    && identity.exe == expected.exe,
                "connection process identity changed; refresh again"
            );
            count += resources.len();
            ensure!(
                count <= 2000,
                "connection snapshot exceeds 2000 descriptors"
            );
            captured.push(ProcessResources {
                at_ms,
                identity,
                resources,
                truncated: false,
                source: "macOS lsof machine fields; current-user sockets paired with the process snapshot".into(),
            });
        }
        Ok(captured)
    }

    pub fn connections(&self) -> Result<String> {
        #[cfg(target_os = "macos")]
        {
            let output = bounded_command(
                "/usr/sbin/lsof",
                &[
                    "-nP".into(),
                    "-i".into(),
                    "-a".into(),
                    "-u".into(),
                    current_uid().to_string(),
                ],
                Duration::from_secs(3),
                1024 * 1024,
            )?;
            ensure!(
                output.success || (output.stdout.is_empty() && output.stderr.trim().is_empty()),
                "connection query failed: {}",
                output.stderr
            );
            Ok(output
                .stdout
                .lines()
                .take(1000)
                .collect::<Vec<_>>()
                .join("\n"))
        }
        #[cfg(target_os = "linux")]
        {
            crate::linux_system::connections(current_uid())
        }
        #[cfg(windows)]
        {
            crate::windows::connections()
        }
        #[cfg(target_os = "android")]
        {
            anyhow::bail!("Android 10+ restricts /proc/net for ordinary application UIDs; native connection enumeration is unavailable without separately authorized platform privileges")
        }
        #[cfg(not(any(
            target_os = "macos",
            target_os = "linux",
            target_os = "android",
            windows
        )))]
        {
            anyhow::bail!("connection details are unavailable on this platform")
        }
    }

    pub fn managed_root(&self) -> &Path {
        self.recovery.root()
    }
    pub fn file_events(&self) -> Result<Vec<FileEvent>> {
        Ok(self.file_events.lock().unwrap().iter().cloned().collect())
    }
    pub fn audit_history(&self, limit: usize) -> Result<Vec<AuditRecord>> {
        crate::audit::read_history(&default_data_dir()?.join("audit"), limit)
    }
    pub fn managed_files(&self) -> Result<Vec<ManagedFile>> {
        self.recovery.files()
    }
    pub fn read_managed(&self, path: &str) -> Result<String> {
        self.recovery.read(path)
    }
    pub fn edit_managed(&self, path: &str, contents: &str) -> Result<RecoveryRecord> {
        self.recovery.edit(path, contents.as_bytes())
    }
    pub fn recovery_records(&self) -> Result<Vec<RecoveryRecord>> {
        self.recovery.records()
    }
    pub fn rollback(&self, id: &str) -> Result<RecoveryRecord> {
        self.recovery.rollback(id)
    }
    pub fn run_plugin(
        &self,
        program: &Path,
        args: &[String],
        capabilities: PluginCapabilities,
    ) -> Result<PluginRun> {
        run_plugin(self.managed_root(), program, args, capabilities)
    }
}

#[cfg(any(test, target_os = "macos"))]
fn parse_connection_groups(output: &str) -> Result<Vec<(u32, Vec<ProcessResource>)>> {
    let mut groups = Vec::new();
    let mut current: Option<(u32, String)> = None;
    for field in output.split('\0') {
        let field = field.trim_start_matches('\n');
        if let Some(value) = field.strip_prefix('p') {
            if let Some(group) = current.take() {
                groups.push(group);
            }
            let pid = value.parse().context("invalid lsof process identity")?;
            current = Some((pid, String::new()));
        } else if let Some((_, fields)) = &mut current {
            fields.push_str(field);
            fields.push('\0');
        }
    }
    if let Some(group) = current {
        groups.push(group);
    }
    Ok(groups
        .into_iter()
        .map(|(pid, fields)| (pid, crate::observation::parse_resources(&fields)))
        .collect())
}

fn process_info(process: &sysinfo::Process) -> ProcessInfo {
    let cpu = process.cpu_usage();
    #[cfg(windows)]
    let identity = crate::windows::process_identity(process.pid().as_u32()).unwrap_or_else(|_| {
        ProcessIdentity {
            pid: process.pid().as_u32(),
            start_time: 0,
            exe: String::new(),
            uid: String::new(),
        }
    });
    #[cfg(not(windows))]
    let identity = ProcessIdentity {
        pid: process.pid().as_u32(),
        start_time: process_start_id(process.pid().as_u32(), process.start_time()),
        exe: process
            .exe()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default(),
        uid: process.user_id().map(user_id_string).unwrap_or_default(),
    };
    ProcessInfo {
        identity,
        name: process.name().to_string_lossy().into_owned(),
        cpu_percent: if cpu.is_finite() { cpu } else { 0.0 },
        rss_bytes: process.memory(),
        parent_pid: process.parent().map(|pid| pid.as_u32()),
    }
}

#[cfg(not(windows))]
fn user_id_string(uid: &sysinfo::Uid) -> String {
    #[cfg(unix)]
    {
        (**uid).to_string()
    }
    #[cfg(not(unix))]
    {
        format!("{uid:?}")
    }
}

pub fn process_identity(pid: u32) -> Result<ProcessIdentity> {
    #[cfg(target_os = "android")]
    {
        crate::android_root::process_identity(pid)
    }
    #[cfg(windows)]
    {
        crate::windows::process_identity(pid)
    }
    #[cfg(not(any(target_os = "android", windows)))]
    {
        let target = sysinfo::Pid::from_u32(pid);
        let mut system = System::new();
        system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[target]),
            true,
            sysinfo::ProcessRefreshKind::nothing()
                .with_exe(sysinfo::UpdateKind::Always)
                .with_user(sysinfo::UpdateKind::Always),
        );
        let process = system
            .process(target)
            .context("target process no longer exists")?;
        let identity = process_info(process).identity;
        #[cfg(target_os = "linux")]
        ensure!(
            identity.start_time > 0,
            "process birth identity is unavailable"
        );
        Ok(identity)
    }
}

pub fn current_uid() -> u32 {
    #[cfg(unix)]
    {
        unsafe { libc::geteuid() }
    }
    #[cfg(not(unix))]
    {
        u32::MAX
    }
}

#[cfg(not(windows))]
fn process_start_id(pid: u32, fallback: u64) -> u64 {
    #[cfg(target_os = "linux")]
    {
        let _ = fallback;
        crate::linux_admin::process_start_time(pid).unwrap_or(0)
    }
    #[cfg(target_os = "android")]
    {
        let _ = fallback;
        crate::android_root::process_start_time(pid).unwrap_or(0)
    }
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of_val(&info) as i32;
        if unsafe {
            libc::proc_pidinfo(
                pid as i32,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        } == size
        {
            return info
                .pbi_start_tvsec
                .saturating_mul(1_000_000)
                .saturating_add(info.pbi_start_tvusec);
        }
    }
    #[cfg(not(any(target_os = "android", target_os = "linux")))]
    {
        let _ = pid;
        fallback.saturating_mul(1_000_000)
    }
}

pub fn default_data_dir() -> Result<PathBuf> {
    #[cfg(target_os = "android")]
    {
        Ok(crate::android_root::files_dir()?.join("state"))
    }
    #[cfg(windows)]
    {
        let directory = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .context("LOCALAPPDATA is unavailable")?;
        ensure!(
            directory.is_absolute(),
            "LOCALAPPDATA must be an absolute path"
        );
        Ok(directory.join("GlassTerminal"))
    }
    #[cfg(not(any(target_os = "android", windows)))]
    {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is unavailable")?;
        #[cfg(target_os = "macos")]
        {
            Ok(home.join("Library/Application Support/GlassTerminal"))
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".local/state"))
                .join("glass-terminal"))
        }
    }
}

pub fn user_services() -> Result<Vec<ServiceInfo>> {
    user_services_for_uid(current_uid())
}

pub fn user_services_for_uid(uid: u32) -> Result<Vec<ServiceInfo>> {
    #[cfg(target_os = "macos")]
    {
        let arguments = user_service_arguments(current_uid(), uid)?;
        let output = bounded_command(
            "/bin/launchctl",
            &arguments,
            Duration::from_secs(3),
            2 * 1024 * 1024,
        )?;
        ensure!(
            output.success,
            "launchctl list for user {uid} failed: {}",
            output.stderr
        );
        let system = System::new_all();
        Ok(parse_user_services(&output.stdout, uid, &system))
    }
    #[cfg(target_os = "linux")]
    {
        crate::linux_system::user_services(uid)
    }
    #[cfg(windows)]
    {
        ensure!(
            uid == u32::MAX,
            "Windows services use the authenticated SID context"
        );
        crate::windows::services()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = uid;
        anyhow::bail!("service enumeration is unavailable on this platform")
    }
}

#[cfg(target_os = "macos")]
fn user_service_arguments(effective_uid: u32, target_uid: u32) -> Result<Vec<String>> {
    ensure!(
        effective_uid == 0 || effective_uid == target_uid,
        "service query is outside the current user's domain"
    );
    Ok(if effective_uid == target_uid {
        vec!["list".into()]
    } else {
        vec![
            "asuser".into(),
            target_uid.to_string(),
            "/bin/launchctl".into(),
            "list".into(),
        ]
    })
}

#[cfg(target_os = "macos")]
fn parse_user_services(output: &str, uid: u32, system: &System) -> Vec<ServiceInfo> {
    output
        .lines()
        .skip(1)
        .take(4000)
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?.parse::<u32>().ok();
            let last_exit = parts.next()?.parse::<i32>().ok();
            let label = parts.next()?.to_owned();
            let process_start_time = pid
                .and_then(|pid| system.process(sysinfo::Pid::from_u32(pid)))
                .map(|process| process_start_id(process.pid().as_u32(), process.start_time()));
            Some(ServiceInfo {
                identity: ServiceIdentity {
                    label,
                    domain: format!("gui/{uid}"),
                    uid,
                    pid,
                    process_start_time,
                    owner_sid: None,
                    configuration_hash: None,
                },
                pid,
                last_exit,
            })
        })
        .collect()
}

#[cfg(all(test, target_os = "macos"))]
mod service_query_tests {
    use super::*;

    #[test]
    fn service_query_preserves_self_and_selects_the_admin_callers_bootstrap() {
        assert_eq!(user_service_arguments(501, 501).unwrap(), vec!["list"]);
        assert_eq!(
            user_service_arguments(0, 501).unwrap(),
            vec!["asuser", "501", "/bin/launchctl", "list"]
        );
        assert!(user_service_arguments(501, 502).is_err());
        assert!(user_service_arguments(501, 0).is_err());
    }

    #[test]
    fn service_list_identity_uses_the_requested_user_domain() {
        let output = "PID\tStatus\tLabel\n123\t0\torg.plyra.fixture\n-\t17\torg.plyra.waiting\n";
        let services = parse_user_services(output, 501, &System::new());
        assert_eq!(services.len(), 2);
        assert_eq!(services[0].identity.label, "org.plyra.fixture");
        assert_eq!(services[0].identity.pid, Some(123));
        assert_eq!(services[1].identity.pid, None);
        assert_eq!(services[1].last_exit, Some(17));
        for service in services {
            assert_eq!(service.identity.uid, 501);
            assert_eq!(service.identity.domain, "gui/501");
            assert_eq!(service.identity.process_start_time, None);
        }
    }
}

#[cfg(target_os = "macos")]
fn predicate_string(value: &str) -> Result<String> {
    ensure!(
        value.len() <= 256 && !value.chars().any(char::is_control),
        "invalid log filter"
    );
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

pub struct CommandOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

pub fn bounded_command(
    program: &str,
    args: &[String],
    timeout: Duration,
    maximum_bytes: usize,
) -> Result<CommandOutput> {
    bounded_command_with_directory(program, args, timeout, maximum_bytes, None)
}

pub(crate) fn bounded_command_with_directory(
    program: &str,
    args: &[String],
    timeout: Duration,
    maximum_bytes: usize,
    directory: Option<&Path>,
) -> Result<CommandOutput> {
    ensure!(
        Path::new(program).is_absolute()
            && args.len() <= 128
            && args.iter().all(|arg| arg.len() <= 4096),
        "invalid native command arguments"
    );
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("DYLD_INSERT_LIBRARIES")
        .env_remove("LD_PRELOAD");
    if let Some(directory) = directory {
        command
            .current_dir(directory)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LANG", "C.UTF-8")
            .env("HOME", directory)
            .env("TMPDIR", directory);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("starting {program}"))?;
    let (sender, receiver) = mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    for (index, pipe) in [
        (0, Box::new(stdout) as Box<dyn Read + Send>),
        (1, Box::new(stderr) as Box<dyn Read + Send>),
    ] {
        let sender = sender.clone();
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = pipe
                .take((maximum_bytes + 1) as u64)
                .read_to_end(&mut bytes);
            let _ = sender.send((index, result, bytes));
        });
    }
    drop(sender);
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().ok();
            thread::spawn(move || {
                let _ = child.wait();
            });
            anyhow::bail!("native query exceeded its time limit");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let mut streams = [String::new(), String::new()];
    for _ in 0..2 {
        let (index, result, bytes) = receiver
            .recv_timeout(Duration::from_millis(500))
            .context("native query output did not close")?;
        result?;
        ensure!(
            bytes.len() <= maximum_bytes,
            "native query output exceeded its byte limit"
        );
        streams[index] = String::from_utf8_lossy(&bytes).into_owned();
    }
    let [stdout, stderr] = streams;
    Ok(CommandOutput {
        success: status.success(),
        code: status.code(),
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod connection_tests {
    use super::parse_connection_groups;

    #[test]
    fn machine_socket_fields_preserve_process_endpoint_and_state_groups() {
        let output = "p42\0\nf7u\0tIPv4\0PTCP\0n127.0.0.1:1234->127.0.0.1:4321\0TST=ESTABLISHED\0\np43\0\nf8u\0tIPv6\0PUDP\0n[::1]:1234\0\n";
        let groups = parse_connection_groups(output).unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, 42);
        assert_eq!(groups[0].1[0].name, "127.0.0.1:1234->127.0.0.1:4321");
        assert_eq!(groups[0].1[0].state.as_deref(), Some("ESTABLISHED"));
        assert_eq!(groups[1].0, 43);
        assert_eq!(groups[1].1[0].protocol.as_deref(), Some("UDP"));
        assert_eq!(groups[1].1[0].name, "[::1]:1234");
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_identity_tests {
    #[test]
    fn process_identity_matches_kernel_birth_ticks() {
        let pid = std::process::id();
        let identity = super::process_identity(pid).unwrap();
        assert_eq!(
            identity.start_time,
            crate::linux_admin::process_start_time(pid).unwrap()
        );
        assert_eq!(identity.uid, crate::current_uid().to_string());
        assert!(!identity.exe.is_empty());
    }
}

// FVPROV1 28336 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNWI3ODNjZDFmYWVhZmJkYjA1ZjNkMWVmMWMxYWY0ZTIzMTZkMDJlMThhNTUwMjZiNzg4NGJhZDE2MDZhZDI1NCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiZTZiMWZhMmNiMTAyYzYwNmNlMGYzNWVhYTgyZThhZTY2MjAzM2UxYWVjMzg0NTlmZDYwZGYwODRiOWJmOTAyOWJkODU2NmI5NzljNjc5ODA1NmQ4ZTcwNDM4ODI2NmI1OWE0MGM5MGZjMjRiMGVhYmZkOTdjZjIyOGFjMzgwMDMifV19XQ== FVPROV1-END
