// SPDX-License-Identifier: GPL-3.0-only
use super::{
    windows_identity::{self, Handle, PinnedImage},
    windows_ipc::PipeStream,
    windows_trust::{self, InstalledPackage},
};
use crate::{ipc::*, *};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    fs::{File, OpenOptions},
    io::BufReader,
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle, process::CommandExt},
    path::Path,
    process::{Child, Command},
    ptr,
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::{HANDLE, WAIT_TIMEOUT},
    Storage::FileSystem::FILE_SHARE_READ,
    System::{
        Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED},
        Threading::{GetProcessId, TerminateProcess, WaitForSingleObject, CREATE_NO_WINDOW},
    },
    UI::{
        Shell::{ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW},
        WindowsAndMessaging::SW_HIDE,
    },
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowsBootstrap {
    pub nonce: String,
    pub caller_sid: String,
    pub caller_pid: u32,
    pub caller_session: u32,
    pub administrator: bool,
    pub audit_handle: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowsHello {
    pub pid: u32,
}

enum LaunchedProcess {
    Ordinary(Child),
    Elevated(Handle),
}

impl LaunchedProcess {
    fn raw(&self) -> HANDLE {
        match self {
            Self::Ordinary(child) => child.as_raw_handle(),
            Self::Elevated(handle) => handle.raw(),
        }
    }

    fn pid(&self) -> Result<u32> {
        let pid = unsafe { GetProcessId(self.raw()) };
        ensure!(pid > 4, "cannot query the launched broker PID");
        Ok(pid)
    }
}

impl Drop for LaunchedProcess {
    fn drop(&mut self) {
        if unsafe { WaitForSingleObject(self.raw(), 1000) } == WAIT_TIMEOUT {
            let terminated = unsafe { TerminateProcess(self.raw(), 1) };
            if terminated == 0 {
                eprintln!(
                    "Windows broker cleanup could not terminate the owned process ({})",
                    std::io::Error::last_os_error()
                );
            } else if unsafe { WaitForSingleObject(self.raw(), 1000) } == WAIT_TIMEOUT {
                eprintln!("Windows broker cleanup exceeded its termination deadline");
            }
        }
    }
}

pub struct WindowsTransport {
    connection: Option<BufReader<PipeStream>>,
    lifetime: Option<PipeStream>,
    _process: LaunchedProcess,
    nonce: String,
    sequence: u64,
    administrator: bool,
    active: bool,
    broker_pid: u32,
    broker_sid: String,
    _audit: File,
    _broker_image: PinnedImage,
    _ui_image: PinnedImage,
    _installed: Option<InstalledPackage>,
}

impl WindowsTransport {
    pub fn spawn(administrator: bool) -> Result<Self> {
        let caller = windows_identity::current_principal()?;
        ensure!(
            !caller.elevated,
            "the terminal UI must run without administrator elevation"
        );
        let executable = std::env::current_exe()?;
        let ui_image = PinnedImage::open(&executable)?;
        let installed = if administrator {
            Some(windows_trust::installed_package()?)
        } else {
            None
        };
        let broker_path = if let Some(package) = &installed {
            ensure!(
                ui_image.sha256 == package.ui.sha256,
                "running UI differs from the administrator-installed terminal package"
            );
            package.broker.path.clone()
        } else {
            executable.with_file_name("plyra-privilege-broker.exe")
        };
        let broker_image = PinnedImage::open(&broker_path)?;
        let nonce = random_token()?;
        let pipe_name = format!(
            r"\\.\pipe\GlassTerminal.{}.{}",
            std::process::id(),
            random_token()?
        );
        let stream = PipeStream::create(&pipe_name, &caller.sid)?;
        let lifetime = PipeStream::create(&format!("{pipe_name}.1"), &caller.sid)?;
        let arguments = vec![
            "--windows-pipe".into(),
            pipe_name,
            "--ui-pid".into(),
            std::process::id().to_string(),
            "--ui-image-sha256".into(),
            ui_image.sha256.clone(),
            if administrator {
                "--administrator".into()
            } else {
                "--ordinary".into()
            },
        ];
        let process = launch(&broker_image.path, &arguments, administrator)?;
        stream.accept(Duration::from_secs(10))?;
        lifetime.accept(Duration::from_secs(10))?;
        let mut connection = BufReader::new(stream);
        let hello: WindowsHello = read_message(&mut connection)?.context("missing broker hello")?;
        let broker_pid = connection.get_ref().client_pid()?;
        ensure!(
            broker_pid == process.pid()? && hello.pid == broker_pid,
            "broker pipe PID differs from the launched process"
        );
        ensure!(
            lifetime.client_pid()? == broker_pid,
            "broker lifetime pipe belongs to another process"
        );
        let broker = connection.get_ref().client_principal()?;
        ensure!(
            broker.elevated == administrator && broker.session_id == caller.session_id,
            "broker token elevation or desktop session differs from the requested authorization"
        );
        if !administrator {
            ensure!(
                broker.sid == caller.sid,
                "ordinary broker belongs to another SID"
            );
        }
        let actual_image = PinnedImage::open(&windows_identity::process_image(broker_pid)?)?;
        ensure!(
            actual_image.sha256 == broker_image.sha256,
            "broker running executable identity changed"
        );
        let audit_directory = default_data_dir()?.join("audit");
        std::fs::create_dir_all(&audit_directory)?;
        let audit = OpenOptions::new()
            .append(true)
            .create_new(true)
            .share_mode(FILE_SHARE_READ)
            .open(audit_directory.join(format!("{}-{}.jsonl", now_ms(), &random_token()?[..12])))?;
        write_message(
            &WindowsBootstrap {
                nonce: nonce.clone(),
                caller_sid: caller.sid.clone(),
                caller_pid: std::process::id(),
                caller_session: caller.session_id,
                administrator,
                audit_handle: audit.as_raw_handle() as usize as u64,
            },
            connection.get_mut(),
        )?;
        let mut transport = Self {
            connection: Some(connection),
            lifetime: Some(lifetime),
            _process: process,
            nonce,
            sequence: 0,
            administrator,
            active: true,
            broker_pid,
            broker_sid: broker.sid.clone(),
            _audit: audit,
            _broker_image: broker_image,
            _ui_image: ui_image,
            _installed: installed,
        };
        let Response::Identity(identity) = transport.request(Request::Identity)? else {
            anyhow::bail!("invalid Windows broker identity response");
        };
        ensure!(
            identity.broker_pid == broker_pid
                && identity.broker_sid.as_deref() == Some(broker.sid.as_str())
                && identity.caller_pid == std::process::id()
                && identity.caller_sid.as_deref() == Some(caller.sid.as_str())
                && identity.administrator == administrator,
            "Windows broker authenticated session identity mismatch"
        );
        Ok(transport)
    }

    pub fn request(&mut self, request: Request) -> Result<Response> {
        ensure!(self.active, "broker session authorization was revoked");
        self.sequence = self
            .sequence
            .checked_add(1)
            .context("broker sequence exhausted")?;
        let revoke = matches!(&request, Request::Revoke) && self.administrator;
        let connection = self
            .connection
            .as_mut()
            .context("Windows broker connection is closed")?;
        let result: Result<Response> = (|| {
            write_message(
                &Envelope {
                    nonce: self.nonce.clone(),
                    sequence: self.sequence,
                    request,
                },
                connection.get_mut(),
            )?;
            let response = read_message(connection)?
                .context("Windows broker closed its authenticated session")?;
            Ok(response)
        })();
        if result.is_err() || revoke {
            self.active = false;
            self.lifetime.take();
            self.connection.take();
        }
        let response = result?;
        if let Response::Error(message) = &response {
            anyhow::bail!("{message}");
        }
        Ok(response)
    }

    pub fn is_connected(&self) -> bool {
        self.active
    }

    pub fn is_admin(&self) -> bool {
        self.active && self.administrator
    }

    /// The numeric value is a legacy display placeholder. Authentication uses full SID fields.
    pub fn peer_identity(&self) -> (u32, u32) {
        (u32::MAX, self.broker_pid)
    }

    pub fn peer_principal_label(&self) -> String {
        format!("SID {}", self.broker_sid)
    }
}

impl Drop for WindowsTransport {
    fn drop(&mut self) {
        self.lifetime.take();
        self.connection.take();
    }
}

fn launch(binary: &Path, arguments: &[String], administrator: bool) -> Result<LaunchedProcess> {
    if !administrator {
        return Ok(LaunchedProcess::Ordinary(
            Command::new(binary)
                .args(arguments)
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
                .context("starting the Windows broker")?,
        ));
    }
    // All parameters are fixed flags, a restricted pipe name, PID digits, or a hexadecimal hash.
    ensure!(
        arguments
            .iter()
            .all(|arg| !arg.is_empty() && !arg.chars().any(|ch| ch.is_whitespace() || ch == '"')),
        "invalid UAC broker parameters"
    );
    let directory = windows_identity::wide(
        binary
            .parent()
            .context("broker directory is unavailable")?
            .as_os_str(),
    )?;
    let binary = windows_identity::wide(binary.as_os_str())?;
    let parameters = windows_identity::wide(OsStr::new(&arguments.join(" ")))?;
    let verb = windows_identity::wide(OsStr::new("runas"))?;
    unsafe {
        let initialized = CoInitializeEx(ptr::null(), COINIT_APARTMENTTHREADED as u32);
        ensure!(
            initialized >= 0,
            "cannot initialize the Windows administrator launcher ({initialized:#x})"
        );
        let mut execute: SHELLEXECUTEINFOW = std::mem::zeroed();
        execute.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        execute.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
        execute.lpVerb = verb.as_ptr();
        execute.lpFile = binary.as_ptr();
        execute.lpParameters = parameters.as_ptr();
        execute.lpDirectory = directory.as_ptr();
        execute.nShow = SW_HIDE;
        let started = ShellExecuteExW(&mut execute);
        let error = std::io::Error::last_os_error();
        CoUninitialize();
        ensure!(
            started != 0,
            "Windows administrator authorization failed ({error})"
        );
        Ok(LaunchedProcess::Elevated(Handle::from_raw(
            execute.hProcess,
        )?))
    }
}

// FVPROV1 11637 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiMzAyMzkxOTQ2MTYxZmNjMWNhMjc3Y2YwNDM1NjdlZmNmMjM0ODJkMmZjZjYyNzMwNGUzMTVjYWViNGRlNWIzZiIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMjEzN2Q1YTkxMTBmNDA1YWE1MmY5ZTA0YmQ3MDUzZjU1ZGIxMjNkMWEwZWNiMmNlYWJjOTU1ZDgwMzg3MmNhOGUzOTIwYTg2ODllOWE4NmM5OTM4YWRjYTgxMWQwNzI3ZjczNDBhN2E0MmZjYjBmODk5YTAwM2YyZjZhZWUwMGEifV19XQ== FVPROV1-END
