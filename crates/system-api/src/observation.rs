// SPDX-License-Identifier: GPL-3.0-only
//! Explicit, bounded observation. macOS uses EVFILT_PROC and Linux uses pidfd.
//! https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/kqueue.2.html
#[cfg(target_os = "macos")]
use crate::now_ms;
use crate::ProcessIdentity;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[cfg(any(test, target_os = "macos", target_os = "linux"))]
const GUI_OBSERVATION_DURATION: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(any(test, target_os = "macos", target_os = "linux"))]
fn observation_deadline(
    started: std::time::Instant,
    duration: Option<std::time::Duration>,
) -> Option<std::time::Instant> {
    duration.map(|duration| started + duration)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObservationEvent {
    pub at_ms: u64,
    pub source: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessResource {
    pub descriptor: String,
    pub kind: String,
    pub name: String,
    pub protocol: Option<String>,
    pub state: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessResources {
    pub at_ms: u64,
    pub identity: ProcessIdentity,
    pub resources: Vec<ProcessResource>,
    pub truncated: bool,
    pub source: String,
}

pub fn process_resources(identity: &ProcessIdentity) -> Result<ProcessResources> {
    #[cfg(target_os = "macos")]
    {
        use crate::{bounded_command, process_identity};
        use anyhow::ensure;
        ensure!(
            same_process(identity, &process_identity(identity.pid)?),
            "process identity changed before collection"
        );
        let output = bounded_command(
            "/usr/sbin/lsof",
            &[
                "-nP".into(),
                "-a".into(),
                "-p".into(),
                identity.pid.to_string(),
                "-F0pftnPT".into(),
            ],
            std::time::Duration::from_secs(3),
            1024 * 1024,
        )?;
        ensure!(
            output.success,
            "process descriptor query failed: {}",
            output.stderr
        );
        ensure!(
            same_process(identity, &process_identity(identity.pid)?),
            "process identity changed during collection"
        );
        let mut resources = parse_resources(&output.stdout);
        let truncated = resources.len() > 1024;
        resources.truncate(1024);
        Ok(ProcessResources {
            at_ms: now_ms(),
            identity: identity.clone(),
            resources,
            truncated,
            source: "macOS lsof machine fields; explicit process descriptor query".into(),
        })
    }
    #[cfg(target_os = "linux")]
    {
        linux::process_resources(identity)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = identity;
        anyhow::bail!("process descriptor observation is available on macOS")
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn same_process(left: &ProcessIdentity, right: &ProcessIdentity) -> bool {
    left.pid == right.pid
        && left.start_time == right.start_time
        && left.uid == right.uid
        && left.exe == right.exe
}

#[cfg(any(test, target_os = "macos"))]
pub(crate) fn parse_resources(output: &str) -> Vec<ProcessResource> {
    let mut resources = Vec::new();
    let mut current: Option<ProcessResource> = None;
    for field in output.split('\0') {
        // lsof separates process/file sets with a newline before the next tag.
        let field = field.trim_start_matches('\n');
        let Some(tag) = field.as_bytes().first() else {
            continue;
        };
        if !tag.is_ascii() {
            continue;
        }
        let value = &field[1..];
        if *tag == b'f' {
            if let Some(resource) = current.take() {
                resources.push(resource);
            }
            current = Some(ProcessResource {
                descriptor: value.into(),
                kind: String::new(),
                name: String::new(),
                protocol: None,
                state: None,
            });
        } else if let Some(resource) = &mut current {
            match tag {
                b't' => resource.kind = value.into(),
                b'n' => resource.name = value.chars().take(8192).collect(),
                b'P' => resource.protocol = Some(value.into()),
                b'T' => {
                    if let Some(state) = value.strip_prefix("ST=") {
                        resource.state = Some(state.into());
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(resource) = current {
        resources.push(resource);
    }
    resources
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use anyhow::{ensure, Context};
    use notify::Watcher;
    use std::{
        collections::VecDeque,
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
        sync::{
            atomic::{AtomicBool, Ordering},
            mpsc, Arc, Mutex,
        },
        time::{Duration, Instant},
    };

    const LIMIT: usize = 1024;
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
            let message: String = message.chars().take(2048).collect();
            let mut events = self.events.lock().unwrap();
            if events.queue.len() == LIMIT {
                events.queue.pop_front();
                events.dropped += 1;
            }
            events.queue.push_back(ObservationEvent {
                at_ms: now_ms(),
                source: source.into(),
                message,
            });
            drop(events);
            if !self.notified.swap(true, Ordering::AcqRel) {
                (self.wake)();
            }
        }
    }

    pub struct ProcessObservation {
        shared: Arc<Shared>,
        queue: Arc<OwnedFd>,
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
            let directory = directory
                .canonicalize()
                .context("observation directory is unavailable")?;
            ensure!(directory.is_dir(), "observation path must be a directory");
            ensure!(
                same_process(&identity, &crate::process_identity(identity.pid)?),
                "process identity changed before observation"
            );
            let fd = unsafe { libc::kqueue() };
            ensure!(
                fd >= 0,
                "kqueue failed: {}",
                std::io::Error::last_os_error()
            );
            let queue = Arc::new(unsafe { OwnedFd::from_raw_fd(fd) });
            ensure!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == 0,
                "kqueue close-on-exec setup failed: {}",
                std::io::Error::last_os_error()
            );
            let changes = [
                event(
                    identity.pid as usize,
                    libc::EVFILT_PROC,
                    libc::EV_ADD | libc::EV_CLEAR,
                    libc::NOTE_EXIT | libc::NOTE_FORK | libc::NOTE_EXEC,
                ),
                event(1, libc::EVFILT_USER, libc::EV_ADD | libc::EV_CLEAR, 0),
            ];
            ensure!(
                unsafe {
                    libc::kevent(
                        fd,
                        changes.as_ptr(),
                        changes.len() as i32,
                        std::ptr::null_mut(),
                        0,
                        std::ptr::null(),
                    )
                } == 0,
                "process notification registration failed: {}",
                std::io::Error::last_os_error()
            );
            // Recheck after registration so an intervening PID reuse is rejected.
            ensure!(
                same_process(&identity, &crate::process_identity(identity.pid)?),
                "process identity changed during registration"
            );
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
            let watched = directory.clone();
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
                                    .map(|p| p.to_string_lossy().into_owned())
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
            watcher.watch(&watched, notify::RecursiveMode::NonRecursive)?;
            let scope = duration.map_or_else(
                || "continuous".into(),
                |duration| format!("{}-second", duration.as_secs()),
            );
            shared.record("native process observation", format!("Started {scope} observation · PID {} · start {} · directory {} · process notes may coalesce; filesystem events have no process attribution", identity.pid, identity.start_time, directory.display()));
            let worker_shared = shared.clone();
            let worker_queue = queue.clone();
            let (finished_tx, finished_rx) = mpsc::sync_channel(1);
            let worker = std::thread::Builder::new()
                .name("glass-process-observer".into())
                .spawn(move || {
                    let deadline = observation_deadline(Instant::now(), duration);
                    let mut events: [libc::kevent; 8] = unsafe { std::mem::zeroed() };
                    while !worker_shared.cancelled.load(Ordering::Acquire) {
                        let remaining = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
                        if remaining.is_some_and(|remaining| remaining.is_zero()) {
                            break;
                        }
                        let timeout = remaining.map(|remaining| libc::timespec {
                            tv_sec: remaining.as_secs() as _,
                            tv_nsec: remaining.subsec_nanos() as _,
                        });
                        let count = unsafe {
                            libc::kevent(
                                worker_queue.as_raw_fd(),
                                std::ptr::null(),
                                0,
                                events.as_mut_ptr(),
                                events.len() as i32,
                                timeout
                                    .as_ref()
                                    .map_or(std::ptr::null(), |timeout| timeout as *const libc::timespec),
                            )
                        };
                        if count < 0 {
                            let error = std::io::Error::last_os_error();
                            if error.kind() == std::io::ErrorKind::Interrupted {
                                continue;
                            }
                            worker_shared.record("native process observation", format!("Collection error · {error}"));
                            break;
                        }
                        if count == 0 {
                            break;
                        }
                        for note in events.iter().take(count as usize).filter(|event| event.filter == libc::EVFILT_PROC) {
                            if note.flags & libc::EV_ERROR != 0 {
                                let error_code = note.data;
                                worker_shared.record("native process observation", format!("Kernel notification error · {error_code}"));
                                worker_shared.cancelled.store(true, Ordering::Release);
                                continue;
                            }
                            for (flag, name) in [
                                (libc::NOTE_FORK, "fork"),
                                (libc::NOTE_EXEC, "exec"),
                                (libc::NOTE_EXIT, "exit"),
                            ] {
                                if note.fflags & flag != 0 {
                                    worker_shared.record("native process observation", format!("Kernel {name} notification · PID {} · registered start {}", identity.pid, identity.start_time));
                                }
                            }
                            if note.fflags & libc::NOTE_EXIT != 0 {
                                worker_shared.cancelled.store(true, Ordering::Release);
                            }
                        }
                    }
                    worker_shared.cancelled.store(true, Ordering::Release);
                    drop(watcher);
                    for removal in [
                        event(identity.pid as usize, libc::EVFILT_PROC, libc::EV_DELETE, 0),
                        event(1, libc::EVFILT_USER, libc::EV_DELETE, 0),
                    ] {
                        unsafe {
                            libc::kevent(worker_queue.as_raw_fd(), &removal, 1, std::ptr::null_mut(), 0, std::ptr::null());
                        }
                    }
                    worker_shared.active.store(false, Ordering::Release);
                    worker_shared.record("native process observation", "Observation stopped; kernel registration and directory watcher released".into());
                    let _ = finished_tx.send(());
                })?;
            Ok(Self {
                shared,
                queue,
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
        pub fn stop(&self) {
            self.shared.cancelled.store(true, Ordering::Release);
            let trigger = event(1, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER);
            unsafe {
                libc::kevent(
                    self.queue.as_raw_fd(),
                    &trigger,
                    1,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                );
            }
        }
    }
    impl Drop for ProcessObservation {
        fn drop(&mut self) {
            if !self.stop_and_wait(Duration::from_millis(500)) {
                eprintln!("process observation cleanup exceeded 500 ms");
            }
        }
    }
    fn event(ident: usize, filter: i16, flags: u16, fflags: u32) -> libc::kevent {
        libc::kevent {
            ident,
            filter,
            flags,
            fflags,
            data: 0,
            udata: std::ptr::null_mut(),
        }
    }
}
#[cfg(target_os = "macos")]
pub use native::ProcessObservation;

#[cfg(target_os = "linux")]
#[path = "linux_observation.rs"]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::ProcessObservation;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub struct ProcessObservation;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
impl ProcessObservation {
    pub fn start(
        _identity: ProcessIdentity,
        _directory: &Path,
        _wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        anyhow::bail!("native process event observation is available on macOS")
    }
    pub fn start_with_duration(
        identity: ProcessIdentity,
        directory: &Path,
        _duration: Option<std::time::Duration>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        Self::start(identity, directory, wake)
    }
    pub fn active(&self) -> bool {
        false
    }
    pub fn take_events(&self) -> (Vec<ObservationEvent>, u64) {
        (Vec::new(), 0)
    }
    pub fn stop(&self) {}
    pub fn stop_and_wait(&self, _timeout: std::time::Duration) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_observation_duration_preserves_gui_and_continuous_subscription() {
        use std::time::{Duration, Instant};
        let started = Instant::now();
        assert_eq!(
            observation_deadline(started, Some(GUI_OBSERVATION_DURATION)),
            Some(started + Duration::from_secs(30))
        );
        assert_eq!(
            observation_deadline(started, Some(Duration::from_secs(120))),
            Some(started + Duration::from_secs(120))
        );
        assert_eq!(observation_deadline(started, None), None);

        #[cfg(target_os = "macos")]
        {
            let directory = tempfile::tempdir().unwrap();
            let identity = crate::process_identity(std::process::id()).unwrap();
            for (duration, label) in [
                (Some(GUI_OBSERVATION_DURATION), "30-second"),
                (Some(Duration::from_secs(120)), "120-second"),
                (None, "continuous"),
            ] {
                let observation = if duration == Some(GUI_OBSERVATION_DURATION) {
                    ProcessObservation::start(identity.clone(), directory.path(), || {})
                } else {
                    ProcessObservation::start_with_duration(
                        identity.clone(),
                        directory.path(),
                        duration,
                        || {},
                    )
                }
                .unwrap();
                assert!(observation.active());
                assert!(observation.take_events().0.iter().any(|event| event
                    .message
                    .contains(&format!("Started {label} observation"))));
                assert!(observation.stop_and_wait(Duration::from_secs(1)));
                assert!(!observation.active());
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_process_exit_and_directory_events_are_observed() {
        use std::{
            io::{BufRead, BufReader, Write},
            process::{Command, Stdio},
            time::{Duration, Instant},
        };
        let directory = tempfile::tempdir().unwrap();
        let mut child = Command::new("/bin/sh")
            .args([
                "-c",
                "printf 'ready\\n'; read line; true & wait; exec /bin/sleep 0.1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "ready\n");
        let identity = crate::process_identity(child.id()).unwrap();
        let observation = ProcessObservation::start(identity, directory.path(), || {}).unwrap();
        // Native FSEvents delivery is asynchronous. Keep the process alive while
        // the file event is delivered, then exercise kernel exec and exit notes.
        std::fs::write(directory.path().join("observation-fixture.txt"), "event").unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut events = Vec::new();
        while !events.iter().any(|event: &ObservationEvent| {
            event.source.contains("filesystem") && event.message.contains("observation-fixture")
        }) && Instant::now() < deadline
        {
            events.extend(observation.take_events().0);
            std::thread::sleep(Duration::from_millis(25));
        }
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"continue\n")
            .unwrap();
        child.wait().unwrap();
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
                .any(|event| event.message.contains("Kernel exec notification")),
            "{events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| event.source.contains("filesystem")
                    && event.message.contains("observation-fixture")),
            "{events:?}"
        );
        assert!(!observation.active());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn explicit_stop_waits_for_the_observer_to_retire() {
        let directory = tempfile::tempdir().unwrap();
        let identity = crate::process_identity(std::process::id()).unwrap();
        let observation = ProcessObservation::start(identity, directory.path(), || {}).unwrap();
        assert!(observation.active());
        assert!(observation.stop_and_wait(std::time::Duration::from_secs(1)));
        assert!(!observation.active());
        assert!(observation.take_events().0.iter().any(|event| event
            .message
            .contains("registration and directory watcher released")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn process_resources_include_an_owned_listening_socket() {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let identity = crate::process_identity(std::process::id()).unwrap();
        let collected = process_resources(&identity).unwrap();
        assert!(
            collected
                .resources
                .iter()
                .any(|resource| resource.protocol.as_deref() == Some("TCP")
                    && resource.state.as_deref() == Some("LISTEN")
                    && resource.name.ends_with(&format!(":{}", address.port()))),
            "{collected:?}"
        );
    }

    #[test]
    fn descriptor_fields_preserve_filenames_and_network_identity() {
        let records = parse_resources("p42\0\nf3\0tREG\0n/tmp/file with\nnewline\0\nf8\0tIPv6\0PTCP\0n[::1]:123->[::1]:456\0TST=ESTABLISHED\0\n");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].name, "/tmp/file with\nnewline");
        assert_eq!(records[1].descriptor, "8");
        assert_eq!(records[1].protocol.as_deref(), Some("TCP"));
        assert_eq!(records[1].state.as_deref(), Some("ESTABLISHED"));
    }
}

// FVPROV1 25371 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiN2YyZDMyYmE2OGFlMmFkZWE4Y2M5ZTgxZmJiMDFhY2JmNzc5Zjc3Y2ZjOTQ0MjlkYjVkNTYyYTg3N2RkYWYyOCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMmI5YmYwZWMxMzE1Y2QxODg1MzljNzk0ZDkwZmZmZDUwMWNlMDIwZmE3MTg1OGUxZWYzYzk4OGRiYmE5NzA3OTZiMmQzNGRlMDhlMTQ5Y2JlNDAxZGJkMDdlY2IxZjMxNThkNTY3YmFjOTk2YzI0NjAyODUwNDYwZmQ5ZGIyMDQifV19XQ== FVPROV1-END
