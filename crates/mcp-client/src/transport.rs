// SPDX-License-Identifier: GPL-3.0-only
use crate::{ServerConfig, TransportConfig};
use anyhow::{ensure, Context, Result};
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use rmcp::{
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::{async_rw::AsyncRwTransport, Transport},
    RoleClient,
};
use std::{
    future::Future,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::process::{ChildStdin, ChildStdout, Command};

type OwnedChild = Arc<tokio::sync::Mutex<Option<Box<dyn ChildWrapper>>>>;

#[derive(Clone, Default)]
pub(crate) struct StdioCleanup(Arc<Mutex<Vec<OwnedChild>>>);

impl StdioCleanup {
    pub async fn finish(&self, timeout: Duration) -> Result<()> {
        let children = std::mem::take(&mut *self.0.lock().unwrap());
        for child in children {
            let mut slot = tokio::time::timeout(timeout, child.lock())
                .await
                .context("MCP child cleanup lock exceeded its shutdown budget")?;
            if let Some(child) = slot.as_mut() {
                let _ = child.start_kill();
                tokio::time::timeout(timeout, child.wait())
                    .await
                    .context("MCP child did not retire within its shutdown budget")?
                    .context("MCP child could not be reaped")?;
            }
            slot.take();
        }
        Ok(())
    }
}

pub(crate) struct StdioTransport {
    io: AsyncRwTransport<RoleClient, ChildStdout, ChildStdin>,
    child: OwnedChild,
    shutdown_timeout: Duration,
    maximum_bytes: usize,
}

impl StdioTransport {
    pub fn spawn(config: &ServerConfig, cleanup: &StdioCleanup) -> Result<Self> {
        let TransportConfig::Stdio {
            program,
            args,
            env_refs,
            cwd,
        } = &config.transport
        else {
            anyhow::bail!("stdio configuration required");
        };
        ensure!(
            std::path::Path::new(program).is_file(),
            "configured MCP program is unavailable"
        );
        let mut command = Command::new(program);
        command
            .args(args)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for name in ["PATH", "HOME", "TMPDIR", "TEMP", "LANG", "SystemRoot"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        for (child_name, reference) in env_refs {
            let value = std::env::var_os(reference)
                .with_context(|| format!("MCP environment reference {reference} is unavailable"))?;
            command.env(child_name, value);
        }
        if let Some(cwd) = cwd {
            ensure!(
                std::path::Path::new(cwd).is_dir(),
                "configured MCP working directory is unavailable"
            );
            command.current_dir(cwd);
        }
        let mut command = CommandWrap::from(command);
        command.wrap(KillOnDrop);
        #[cfg(unix)]
        command.wrap(process_wrap::tokio::ProcessGroup::leader());
        #[cfg(windows)]
        command.wrap(process_wrap::tokio::JobObject);
        let mut child = command
            .spawn()
            .context("starting the explicitly selected MCP server")?;
        let read = child
            .stdout()
            .take()
            .context("MCP server stdout is unavailable")?;
        let write = child
            .stdin()
            .take()
            .context("MCP server stdin is unavailable")?;
        let child = Arc::new(tokio::sync::Mutex::new(Some(child)));
        cleanup.0.lock().unwrap().push(child.clone());
        Ok(Self {
            io: AsyncRwTransport::new_client(read, write)
                .with_max_line_length(config.max_message_bytes),
            child,
            shutdown_timeout: Duration::from_millis(config.shutdown_timeout_ms),
            maximum_bytes: config.max_message_bytes,
        })
    }
}

impl Transport<RoleClient> for StdioTransport {
    type Error = std::io::Error;
    fn send(
        &mut self,
        message: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = std::io::Result<()>> + Send + 'static {
        let within_limit =
            serde_json::to_vec(&message).is_ok_and(|bytes| bytes.len() <= self.maximum_bytes);
        let send = self.io.send(message);
        async move {
            if !within_limit {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "MCP outgoing message exceeds its configured byte limit",
                ));
            }
            send.await
        }
    }
    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
        self.io.receive()
    }
    async fn close(&mut self) -> std::io::Result<()> {
        let io_result = self.io.close().await;
        let mut slot = self.child.lock().await;
        let Some(child) = slot.as_mut() else {
            return Ok(());
        };
        if tokio::time::timeout(self.shutdown_timeout, child.wait())
            .await
            .is_ok_and(|result| result.is_ok())
        {
            slot.take();
            return io_result;
        }
        #[cfg(unix)]
        {
            let _ = child.signal(libc::SIGTERM);
        }
        if tokio::time::timeout(self.shutdown_timeout, child.wait())
            .await
            .is_ok_and(|result| result.is_ok())
        {
            slot.take();
            return io_result;
        }
        child.start_kill()?;
        if tokio::time::timeout(self.shutdown_timeout, child.wait())
            .await
            .is_ok_and(|result| result.is_ok())
        {
            slot.take();
        }
        io_result
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.child.try_lock() {
            if let Some(child) = slot.as_mut() {
                let _ = child.start_kill();
            }
        }
    }
}

// FVPROV1 6086 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiMGJmM2E1NTljNDFmNjZhZDI1ODRiNTliNjA2ZmE1OWUwYTdiMTliMzA0YTQ3MDMzNzgwZjBkNGQ0YmNjMjIyYSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiNjkwZTk4MGZjNGI1MGI1YzA0ODNlZWUyMTA5NjM3NWQ2YmM3Y2I1Y2UzN2RlZjdhYzdkNzI1NzM4NjEyMjE0MzIzMGQ5Y2YxNmNiZWZjMGZmNDQxNTIwYThmMjFjYjVhMmI1YzUyYzE4MDU2M2I1YWQ1NDRiYjE5YjU1NGVmMGQifV19XQ== FVPROV1-END
