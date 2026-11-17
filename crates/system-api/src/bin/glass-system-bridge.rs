// SPDX-License-Identifier: GPL-3.0-only
//! Ordinary-user adapter for the existing System API read catalog.
use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{BufRead, Read, Write};
use system_api::{operations, BrokerClient, SystemClient};
const MAX_REQUEST: usize = 64 * 1024;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    schema: String,
    request_id: u64,
    operation: String,
    #[serde(default)]
    args: Vec<String>,
}

#[cfg(unix)]
struct OutputCloseGuard {
    cancel: std::os::unix::net::UnixStream,
    worker: Option<std::thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl OutputCloseGuard {
    fn start() -> Result<Option<Self>> {
        use std::os::fd::AsRawFd;
        let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe { libc::fstat(1, &mut metadata) } == 0,
            "cannot inspect subscription output"
        );
        if metadata.st_mode & libc::S_IFMT != libc::S_IFIFO {
            return Ok(None);
        }
        let (cancel, wake) = std::os::unix::net::UnixStream::pair()?;
        let worker = std::thread::spawn(move || {
            let mut descriptors = [
                libc::pollfd {
                    fd: 1,
                    // Darwin reports pipe-reader HUP only with an interested
                    // event mask. A FIFO writer never becomes input-readable.
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: wake.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            loop {
                if unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, -1) } < 0 {
                    if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    break;
                }
                if descriptors[1].revents != 0 {
                    break;
                }
                if descriptors[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    // A source waiting for native events still stops when its
                    // downstream pipe closes. No periodic sampler is needed.
                    unsafe {
                        libc::kill(libc::getpid(), libc::SIGPIPE);
                    }
                    break;
                }
            }
        });
        Ok(Some(Self {
            cancel,
            worker: Some(worker),
        }))
    }
}

#[cfg(unix)]
impl Drop for OutputCloseGuard {
    fn drop(&mut self) {
        let _ = self.cancel.shutdown(std::net::Shutdown::Both);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn write_record(output: &mut impl Write, value: &Value) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() < MAX_REQUEST, "system record exceeds 64 KiB");
    output.write_all(&bytes)?;
    output.write_all(b"\n")?;
    Ok(())
}
fn catalog(output: &mut impl Write) -> Result<()> {
    for definition in operations::CATALOG {
        write_record(output, &serde_json::to_value(definition)?)?;
    }
    Ok(())
}
fn stdio() -> Result<()> {
    let input = std::io::stdin();
    let mut input = input.lock();
    let output = std::io::stdout();
    let mut output = output.lock();
    let mut client = None;
    let mut broker = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        let count = Read::by_ref(&mut input)
            .take((MAX_REQUEST + 1) as u64)
            .read_until(b'\n', &mut line)?;
        if count == 0 {
            break;
        }
        ensure!(
            line.len() <= MAX_REQUEST && line.ends_with(b"\n"),
            "system request exceeds limit or is truncated"
        );
        let request: Request = serde_json::from_slice(&line)?;
        ensure!(
            request.schema == "glass.system.request.v1" && request.request_id > 0,
            "invalid system request identity"
        );
        ensure!(
            request.args.len() <= 128
                && request
                    .args
                    .iter()
                    .all(|arg| arg.len() <= 8192 && !arg.contains('\0')),
            "invalid system request arguments"
        );
        let records = (|| -> Result<Vec<Value>> {
            if request.operation == "sys.catalog" {
                ensure!(request.args.is_empty(), "sys.catalog accepts no arguments");
                return operations::CATALOG
                    .iter()
                    .map(|definition| serde_json::to_value(definition).map_err(Into::into))
                    .collect();
            }
            if request.operation == "sys.operations" {
                return operations::object_operation_records(&request.args);
            }
            if request.operation == "sys.metrics" {
                ensure!(request.args.is_empty(), "sys.metrics accepts no arguments");
                return Ok(vec![operations::metrics_record()?]);
            }
            if operations::is_broker_operation(&request.operation) {
                if broker.is_none() {
                    broker = Some(BrokerClient::spawn()?);
                }
                return operations::broker_records(
                    broker.as_ref().unwrap(),
                    &request.operation,
                    &request.args,
                )
                .map(|value| vec![value]);
            }
            if client.is_none() {
                client = Some(SystemClient::new()?);
            }
            operations::records(client.as_ref().unwrap(), &request.operation, &request.args)
        })();
        match records {
            Ok(records) => {
                let count = records.len();
                for value in records {
                    write_record(
                        &mut output,
                        &json!({"schema":"glass.system.response.v1","request_id":request.request_id,"type":"record","value":value}),
                    )?;
                }
                write_record(
                    &mut output,
                    &json!({"schema":"glass.system.response.v1","request_id":request.request_id,"type":"complete","status":0,"records":count}),
                )?;
            }
            Err(error) => write_record(
                &mut output,
                &json!({"schema":"glass.system.response.v1","request_id":request.request_id,"type":"complete","status":1,"error":error.to_string()}),
            )?,
        }
        output.flush()?;
    }
    Ok(())
}
#[cfg(target_os = "android")]
fn configure_android() -> Result<()> {
    use std::path::PathBuf;
    use system_api::android_root::{configure, verify_packaged_binary, AndroidContext};

    let files_dir = std::env::var_os("GLASS_ANDROID_APP_FILES")
        .map(PathBuf::from)
        .context("Android Shell app files directory is unavailable")?;
    let executable = std::env::current_exe()?.canonicalize()?;
    let native_library_dir = executable
        .parent()
        .context("Android system bridge executable has no parent directory")?;
    verify_packaged_binary(&executable, native_library_dir)?;
    configure(AndroidContext {
        files_dir,
        native_library_dir: native_library_dir.to_path_buf(),
    })
}

fn run() -> Result<()> {
    #[cfg(target_os = "android")]
    configure_android()?;
    let mut args = std::env::args().skip(1);
    let name = args
        .next()
        .context("usage: glass-system-bridge sys.OPERATION [ARGS] | --catalog | --stdio")?;
    let args: Vec<String> = args.collect();
    if name == "--stdio" {
        ensure!(args.is_empty(), "--stdio accepts no arguments");
        return stdio();
    }
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    if name == "--complete" {
        let client = SystemClient::new()?;
        for record in operations::completion_records(&client, &args)? {
            write_record(&mut output, &record)?;
        }
        output.flush()?;
        return Ok(());
    }
    if name == "--catalog" || name == "sys.catalog" {
        ensure!(args.is_empty(), "catalog accepts no arguments");
        catalog(&mut output)?;
        output.flush()?;
        return Ok(());
    }
    if !operations::CATALOG
        .iter()
        .any(|definition| definition.name == name)
    {
        bail!("unknown system read operation {name}");
    }
    ensure!(!operations::is_broker_operation(&name), "Broker operations require the persistent ordinary --stdio session or the existing GUI gateway");
    let client = SystemClient::new()?;
    if name == "sys.subscribe" {
        #[cfg(unix)]
        let _output_guard = OutputCloseGuard::start()?;
        let options = operations::SubscriptionOptions::parse(&args)?;
        let mut subscription = operations::EventSubscription::start(&client, &options)?;
        write_record(&mut output, &subscription.ready())?;
        output.flush()?;
        let deadline = options
            .seconds
            .map(|seconds| std::time::Instant::now() + std::time::Duration::from_secs(seconds));
        let mut records = 0;
        while let Some(record) = subscription.next(deadline)? {
            write_record(&mut output, &record)?;
            output.flush()?;
            records += 1;
            if options.limit.is_some_and(|limit| records >= limit) {
                break;
            }
        }
        write_record(&mut output, &subscription.finished(records))?;
        output.flush()?;
        return Ok(());
    }
    for record in operations::records(&client, &name, &args)? {
        write_record(&mut output, &record)?;
    }
    output.flush()?;
    Ok(())
}
fn main() {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    if let Err(error) = run() {
        if error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::BrokenPipe)
        {
            #[cfg(unix)]
            std::process::exit(128 + libc::SIGPIPE);
            #[cfg(not(unix))]
            std::process::exit(1);
        }
        eprintln!("glass-system-bridge: {error:#}");
        std::process::exit(2);
    }
}

// FVPROV1 10445 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNmVhMzJjN2FiOGRhYmI1MTgzYjRkMGE2MzgxYTBkMWQ2YTI4OGFlZjYzYmI4NDg3NzQyN2NlM2EwYTM0MmJhMCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiOTUwZmIxODhkOTY0ODRkYjZmZDhhNWRlMjA5NTAyNDQ2YmM5MGMxMzQ4ZTIyYjBlYmJlMzU3NjMzNWFkNzA4YjExNDI4MTczZDdlOTM5NzA3M2JiYTUyZGU4M2Y5MmMxYjA0ZjZmZDFiODQxODE3Yzk3ZjQ3Zjk0NWRiNTJiMGQifV19XQ== FVPROV1-END
