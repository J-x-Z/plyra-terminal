// SPDX-License-Identifier: GPL-3.0-only
use super::{
    observation_deadline, same_process, ObservationEvent, ProcessIdentity, ProcessResource,
    ProcessResources, GUI_OBSERVATION_DURATION,
};
use crate::{linux_system, now_ms, process_identity};
use anyhow::{ensure, Context, Result};
use notify::Watcher;
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    io::Read,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant},
};

const RESOURCE_LIMIT: usize = 1024;
const EVENT_LIMIT: usize = 1024;

fn socket_inode(target: &str) -> Option<u64> {
    target
        .strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

fn socket_resources(
    text: &str,
    protocol: &str,
    wanted: &HashSet<u64>,
) -> Result<HashMap<u64, ProcessResource>> {
    let mut sockets = HashMap::new();
    for line in text.lines().skip(1).filter(|line| !line.trim().is_empty()) {
        let fields: Vec<_> = line.split_whitespace().collect();
        ensure!(fields.len() >= 10, "invalid process socket table row");
        let inode = fields[9].parse::<u64>()?;
        if !wanted.contains(&inode) {
            continue;
        }
        let ipv6 = protocol.ends_with('6');
        sockets.insert(
            inode,
            ProcessResource {
                descriptor: String::new(),
                kind: if ipv6 { "IPv6" } else { "IPv4" }.into(),
                name: format!(
                    "{}->{}",
                    linux_system::endpoint(fields[1], ipv6)?,
                    linux_system::endpoint(fields[2], ipv6)?,
                ),
                protocol: Some(
                    if protocol.starts_with("tcp") {
                        "TCP"
                    } else {
                        "UDP"
                    }
                    .into(),
                ),
                state: Some(linux_system::socket_state(fields[3], protocol).into()),
            },
        );
    }
    Ok(sockets)
}

pub(super) fn process_resources(identity: &ProcessIdentity) -> Result<ProcessResources> {
    ensure!(
        same_process(identity, &process_identity(identity.pid)?),
        "process identity changed before collection"
    );
    let directory = std::fs::read_dir(format!("/proc/{}/fd", identity.pid))
        .context("process descriptors are unavailable for this user")?;
    let mut descriptors = BTreeMap::new();
    let mut truncated = false;
    for entry in directory {
        let entry = entry?;
        let Some(descriptor) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if descriptors.len() == RESOURCE_LIMIT {
            truncated = true;
            break;
        }
        let target = match std::fs::read_link(entry.path()) {
            Ok(target) => target,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("cannot read process descriptor identity"),
        };
        descriptors.insert(descriptor, target.to_string_lossy().into_owned());
    }
    let wanted: HashSet<_> = descriptors
        .values()
        .filter_map(|target| socket_inode(target))
        .collect();
    let mut sockets = HashMap::new();
    if !wanted.is_empty() {
        for protocol in ["tcp", "tcp6", "udp", "udp6"] {
            let path = format!("/proc/{}/net/{protocol}", identity.pid);
            let file = match std::fs::File::open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error).with_context(|| format!("cannot read {path}")),
            };
            let mut bytes = Vec::new();
            file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() <= 1024 * 1024,
                "process socket table exceeds its 1 MiB budget"
            );
            sockets.extend(socket_resources(
                std::str::from_utf8(&bytes)?,
                protocol,
                &wanted,
            )?);
        }
    }
    let resources = descriptors
        .into_iter()
        .map(|(descriptor, target)| {
            let mut resource = socket_inode(&target)
                .and_then(|inode| sockets.get(&inode).cloned())
                .unwrap_or_else(|| ProcessResource {
                    descriptor: String::new(),
                    kind: if target.starts_with("socket:[") {
                        "SOCKET"
                    } else if target.starts_with("pipe:[") {
                        "PIPE"
                    } else if target.starts_with("anon_inode:") {
                        "ANON"
                    } else {
                        "FILE"
                    }
                    .into(),
                    name: target.chars().take(8192).collect(),
                    protocol: None,
                    state: None,
                });
            resource.descriptor = descriptor.to_string();
            resource
        })
        .collect();
    ensure!(
        same_process(identity, &process_identity(identity.pid)?),
        "process identity changed during collection"
    );
    Ok(ProcessResources {
        at_ms: now_ms(),
        identity: identity.clone(),
        resources,
        truncated,
        source: "Linux /proc process descriptors and same-network-namespace socket inode tables; explicit identity-checked query".into(),
    })
}

struct Events {
    queue: VecDeque<ObservationEvent>,
    dropped: u64,
}

struct Shared {
    events: Mutex<Events>,
    active: AtomicBool,
    cancelled: AtomicBool,
    notified: AtomicBool,
    wake: Box<dyn Fn() + Send + Sync>,
}

impl Shared {
    fn record(&self, source: &str, message: String) {
        let mut events = self.events.lock().unwrap();
        if events.queue.len() == EVENT_LIMIT {
            events.queue.pop_front();
            events.dropped += 1;
        }
        events.queue.push_back(ObservationEvent {
            at_ms: now_ms(),
            source: source.into(),
            message: message.chars().take(2048).collect(),
        });
        drop(events);
        if !self.notified.swap(true, Ordering::AcqRel) {
            (self.wake)();
        }
    }
}

pub struct ProcessObservation {
    shared: Arc<Shared>,
    stop_event: Arc<OwnedFd>,
    completion: Mutex<mpsc::Receiver<()>>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl ProcessObservation {
    pub fn start(
        identity: ProcessIdentity,
        directory: &Path,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        Self::start_with_duration(identity, directory, Some(GUI_OBSERVATION_DURATION), wake)
    }

    pub fn start_with_duration(
        identity: ProcessIdentity,
        directory: &Path,
        duration: Option<Duration>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        ensure!(
            identity.pid > 1 && identity.pid <= i32::MAX as u32,
            "invalid process observation PID"
        );
        let directory = directory
            .canonicalize()
            .context("observation directory is unavailable")?;
        ensure!(directory.is_dir(), "observation path must be a directory");
        ensure!(
            same_process(&identity, &process_identity(identity.pid)?),
            "process identity changed before observation"
        );
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, identity.pid as libc::pid_t, 0) };
        ensure!(
            fd >= 0,
            "pidfd process observation is unavailable: {}",
            std::io::Error::last_os_error()
        );
        let process = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        ensure!(
            same_process(&identity, &process_identity(identity.pid)?),
            "process identity changed during registration"
        );
        let stop_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        ensure!(
            stop_fd >= 0,
            "process observation cancellation setup failed: {}",
            std::io::Error::last_os_error()
        );
        let stop_event = Arc::new(unsafe { OwnedFd::from_raw_fd(stop_fd) });
        let shared = Arc::new(Shared {
            events: Mutex::new(Events {
                queue: VecDeque::new(),
                dropped: 0,
            }),
            active: AtomicBool::new(true),
            cancelled: AtomicBool::new(false),
            notified: AtomicBool::new(false),
            wake: Box::new(wake),
        });
        let file_shared = shared.clone();
        let mut watcher =
            notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                if file_shared.cancelled.load(Ordering::Acquire) {
                    return;
                }
                match result {
                    Ok(event) => file_shared.record(
                        "native filesystem observation",
                        format!(
                            "{:?} · {}",
                            event.kind,
                            event
                                .paths
                                .iter()
                                .take(32)
                                .map(|path| path.to_string_lossy().into_owned())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    ),
                    Err(error) => file_shared.record(
                        "native filesystem observation",
                        format!("Collection error · {error}"),
                    ),
                }
            })?;
        watcher.watch(&directory, notify::RecursiveMode::NonRecursive)?;
        let scope = duration.map_or_else(
            || "continuous".into(),
            |duration| format!("{}-second", duration.as_secs()),
        );
        shared.record("native process observation", format!(
            "Started {scope} observation · PID {} · start {} · directory {} · pidfd reports process exit; filesystem events have no process attribution",
            identity.pid, identity.start_time, directory.display()
        ));
        let worker_shared = shared.clone();
        let worker_stop = stop_event.clone();
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("glass-linux-observer".into())
            .spawn(move || {
                let deadline = observation_deadline(Instant::now(), duration);
                let mut descriptors = [
                    libc::pollfd {
                        fd: process.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: worker_stop.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                ];
                while !worker_shared.cancelled.load(Ordering::Acquire) {
                    let remaining =
                        deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
                    if remaining.is_some_and(|remaining| remaining.is_zero()) {
                        break;
                    }
                    let timeout = remaining.map_or(-1, |remaining| {
                        remaining.as_millis().max(1).min(i32::MAX as u128) as i32
                    });
                    let count = unsafe {
                        libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, timeout)
                    };
                    if count < 0 {
                        let error = std::io::Error::last_os_error();
                        if error.kind() == std::io::ErrorKind::Interrupted {
                            continue;
                        }
                        worker_shared.record(
                            "native process observation",
                            format!("Collection error · {error}"),
                        );
                        break;
                    }
                    if count == 0 {
                        break;
                    }
                    if descriptors.iter().any(|descriptor| {
                        descriptor.revents & (libc::POLLERR | libc::POLLNVAL) != 0
                    }) {
                        worker_shared.record(
                            "native process observation",
                            "Kernel notification descriptor failed".into(),
                        );
                        break;
                    }
                    if descriptors[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                        worker_shared.record(
                            "native process observation",
                            format!(
                                "Kernel exit notification · PID {} · registered start {}",
                                identity.pid, identity.start_time
                            ),
                        );
                        break;
                    }
                }
                worker_shared.cancelled.store(true, Ordering::Release);
                drop(watcher);
                drop(process);
                worker_shared.active.store(false, Ordering::Release);
                worker_shared.record(
                    "native process observation",
                    "Observation stopped; pidfd and directory watcher released".into(),
                );
                let _ = finished_tx.send(());
            })?;
        Ok(Self {
            shared,
            stop_event,
            completion: Mutex::new(finished_rx),
            worker: Mutex::new(Some(worker)),
        })
    }

    pub fn active(&self) -> bool {
        self.shared.active.load(Ordering::Acquire)
    }

    pub fn take_events(&self) -> (Vec<ObservationEvent>, u64) {
        let mut events = self.shared.events.lock().unwrap();
        self.shared.notified.store(false, Ordering::Release);
        (events.queue.drain(..).collect(), events.dropped)
    }

    pub fn stop(&self) {
        self.shared.cancelled.store(true, Ordering::Release);
        let value = 1_u64.to_ne_bytes();
        unsafe {
            libc::write(
                self.stop_event.as_raw_fd(),
                value.as_ptr().cast(),
                value.len(),
            );
        }
    }

    pub fn stop_and_wait(&self, timeout: Duration) -> bool {
        self.stop();
        let mut worker = self.worker.lock().unwrap();
        let Some(handle) = worker.as_ref() else {
            return true;
        };
        if !handle.is_finished()
            && self
                .completion
                .lock()
                .unwrap()
                .recv_timeout(timeout)
                .is_err()
        {
            return false;
        }
        worker.take().unwrap().join().is_ok()
    }
}

impl Drop for ProcessObservation {
    fn drop(&mut self) {
        if !self.stop_and_wait(Duration::from_millis(500)) {
            eprintln!("process observation cleanup exceeded 500 ms");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_inode_matching_preserves_protocol_endpoints_and_state() {
        let wanted = HashSet::from([12345]);
        let table = "header\n0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000 1000 0 12345\n1: 0100007F:1F91 00000000:0000 0A 00000000:00000000 00:00000000 00000000 1000 0 12346\n";
        let sockets = socket_resources(table, "tcp", &wanted).unwrap();
        assert_eq!(sockets.len(), 1);
        let socket = &sockets[&12345];
        assert_eq!(socket.kind, "IPv4");
        assert_eq!(socket.name, "127.0.0.1:8080->0.0.0.0:0");
        assert_eq!(socket.protocol.as_deref(), Some("TCP"));
        assert_eq!(socket.state.as_deref(), Some("LISTEN"));
        assert_eq!(socket_inode("socket:[12345]"), Some(12345));
        assert!(socket_inode("socket:[invalid]").is_none());
    }

    #[test]
    fn owned_descriptors_include_file_and_listening_socket() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("owned-descriptor.txt");
        let _file = std::fs::File::create(&path).unwrap();
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let identity = process_identity(std::process::id()).unwrap();
        let captured = process_resources(&identity).unwrap();
        assert!(captured
            .resources
            .iter()
            .any(|resource| resource.name == path.to_string_lossy()));
        assert!(
            captured
                .resources
                .iter()
                .any(|resource| resource.protocol.as_deref() == Some("TCP")
                    && resource.state.as_deref() == Some("LISTEN")
                    && resource.name.starts_with(&format!("127.0.0.1:{port}->"))),
            "{captured:?}"
        );
        let mut stale = identity;
        stale.start_time = stale.start_time.saturating_add(1);
        assert!(process_resources(&stale).is_err());
    }

    #[test]
    fn explicit_stop_releases_native_observation() {
        let directory = tempfile::tempdir().unwrap();
        let identity = process_identity(std::process::id()).unwrap();
        let observation = ProcessObservation::start(identity, directory.path(), || {}).unwrap();
        assert!(observation.active());
        assert!(observation.stop_and_wait(Duration::from_secs(1)));
        assert!(!observation.active());
        assert!(observation.take_events().0.iter().any(|event| event
            .message
            .contains("pidfd and directory watcher released")));
    }

    #[test]
    fn pidfd_exit_and_directory_events_are_delivered() {
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let mut child = Child(
            std::process::Command::new("/bin/sleep")
                .arg("30")
                .spawn()
                .unwrap(),
        );
        let identity = process_identity(child.0.id()).unwrap();
        let observation = ProcessObservation::start(identity, directory.path(), || {}).unwrap();
        std::fs::write(directory.path().join("observation-fixture.txt"), b"event").unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut events = Vec::new();
        while !events.iter().any(|event: &ObservationEvent| {
            event.source.contains("filesystem") && event.message.contains("observation-fixture")
        }) && Instant::now() < deadline
        {
            events.extend(observation.take_events().0);
            std::thread::sleep(Duration::from_millis(10));
        }
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while observation.active() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(observation.stop_and_wait(Duration::from_secs(1)));
        events.extend(observation.take_events().0);
        assert!(
            events
                .iter()
                .any(|event| event.message.contains("Kernel exit notification")),
            "{events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| event.source.contains("filesystem")
                    && event.message.contains("observation-fixture")),
            "{events:?}"
        );
    }
}

// FVPROV1 20179 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNDlmNDBkNDhmNDc0MmRkMzFjNzY3MmUzY2EzNjAzYzlmMzI2NDFiMGYzMTAwYmZmMGVhOWQzMDA5NWEzODljYiIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMDBmMDA1YTU4NzFmNGViNmQxNTBlYjhhZDNhOTBiNTU1NjJmOTI1NWU0ODg5ZTliNWE5NWQ1YWRiNDljMTRiZTlhYzEzMGQxODYwYmY3NzRhNjAyMGIyNzIwY2Y1N2FkNzFiMWNmOTEyOTE2ODk5NmY5Y2U3OTZlMTdmNmMyMDgifV19XQ== FVPROV1-END
