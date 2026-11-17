// SPDX-License-Identifier: GPL-3.0-only
#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::{backend::bounded_command_with_directory, now_ms};
use crate::{PluginCapabilities, PluginRun};
use anyhow::{ensure, Result};
use std::path::Path;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::time::Duration;

pub fn run_plugin(
    workspace: &Path,
    program: &Path,
    args: &[String],
    capabilities: PluginCapabilities,
) -> Result<PluginRun> {
    ensure!(
        program.is_absolute() && program.is_file(),
        "plugin requires an absolute executable path"
    );
    ensure!(
        args.len() <= 32 && args.iter().all(|arg| arg.len() <= 4096),
        "plugin arguments exceed limits"
    );
    let workspace = workspace.canonicalize()?;
    let program = program.canonicalize()?;
    ensure!(
        !program.starts_with(workspace.join(".glass-journal")),
        "recovery journal files cannot be executed as plugins"
    );
    #[cfg(target_os = "macos")]
    {
        ensure!(
            Path::new("/usr/bin/sandbox-exec").is_file(),
            "macOS sandbox-exec is unavailable"
        );
        let quote = |value: &Path| -> Result<String> {
            let text = value
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("plugin path is not UTF-8"))?;
            ensure!(
                !text.chars().any(char::is_control),
                "plugin path contains controls"
            );
            Ok(format!(
                "\"{}\"",
                text.replace('\\', "\\\\").replace('"', "\\\"")
            ))
        };
        let mut profile = format!("(version 1)\n(deny default)\n(allow process-exec (literal {}))\n(allow file-read* (literal \"/\") (subpath \"/System\") (subpath \"/usr/lib\") (subpath \"/usr/share\") (subpath \"/private/var/db/dyld\") (literal {}) (subpath {}))\n(allow file-read-metadata)\n(allow sysctl-read)\n", quote(&program)?, quote(&program)?, quote(&workspace)?);
        profile.push_str(&format!(
            "(deny file-read* (subpath {}))\n(deny file-write* (subpath {}))\n",
            quote(&workspace.join(".glass-journal"))?,
            quote(&workspace.join(".glass-journal"))?
        ));
        if capabilities.write_workspace {
            profile.push_str(&format!(
                "(allow file-write* (subpath {}))\n",
                quote(&workspace)?
            ));
        }
        if capabilities.network {
            profile.push_str("(allow network-outbound)\n");
        }
        // No process-fork, Mach lookup, broker socket, or inherited broker token.
        let mut arguments = vec!["-p".into(), profile, program.to_string_lossy().into_owned()];
        arguments.extend_from_slice(args);
        let started_at_ms = now_ms();
        let output = bounded_command_with_directory(
            "/usr/bin/sandbox-exec",
            &arguments,
            Duration::from_secs(3),
            65_536,
            Some(&workspace),
        )?;
        Ok(PluginRun {
            started_at_ms,
            program: program.to_string_lossy().into_owned(),
            exit_code: output.code,
            output: format!("{}{}", output.stdout, output.stderr),
            sandbox: format!(
                "macOS Seatbelt deny-default; workspace_write={}; network={}; no fork; no recovery journal; no broker",
                capabilities.write_workspace, capabilities.network
            ),
            broker_access: false,
        })
    }
    #[cfg(target_os = "linux")]
    {
        ensure!(
            Path::new("/usr/bin/bwrap").is_file(),
            "Linux plugin sandbox requires /usr/bin/bwrap"
        );
        let workspace_path = workspace
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("plugin workspace is not UTF-8"))?;
        // Namespace and mount semantics follow the upstream bwrap manual.
        // https://github.com/containers/bubblewrap/blob/main/bwrap.xml
        let mut arguments: Vec<String> = [
            "--unshare-user",
            "--unshare-pid",
            "--unshare-net",
            "--unshare-ipc",
            "--unshare-uts",
            "--die-with-parent",
            "--new-session",
            "--cap-drop",
            "ALL",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--tmpfs",
            "/tmp",
            "--ro-bind",
            "/usr/bin",
            "/usr/bin",
            "--ro-bind",
            "/usr/lib",
            "/usr/lib",
            "--ro-bind-try",
            "/usr/lib64",
            "/usr/lib64",
            "--ro-bind-try",
            "/usr/share",
            "/usr/share",
        ]
        .iter()
        .map(|value| (*value).into())
        .collect();
        for path in ["/bin", "/lib", "/lib64", "/etc/ld.so.cache"] {
            arguments.extend(["--ro-bind-try".into(), path.into(), path.into()]);
        }
        arguments.extend([
            if capabilities.write_workspace {
                "--bind"
            } else {
                "--ro-bind"
            }
            .into(),
            workspace_path.into(),
            workspace_path.into(),
            "--ro-bind".into(),
            program.to_string_lossy().into_owned(),
            "/glass-plugin".into(),
        ]);
        let journal = workspace.join(".glass-journal");
        if journal.is_dir() {
            let journal = journal.to_string_lossy().into_owned();
            arguments.extend([
                "--tmpfs".into(),
                journal.clone(),
                "--remount-ro".into(),
                journal,
            ]);
        }
        if capabilities.network {
            arguments.push("--share-net".into());
            for path in ["/etc/resolv.conf", "/etc/ssl/certs"] {
                arguments.extend(["--ro-bind-try".into(), path.into(), path.into()]);
            }
        }
        arguments.extend([
            "--chdir".into(),
            workspace_path.into(),
            "--setenv".into(),
            "TMPDIR".into(),
            "/tmp".into(),
            "--".into(),
            "/glass-plugin".into(),
        ]);
        arguments.extend_from_slice(args);
        let started_at_ms = now_ms();
        let output = bounded_command_with_directory(
            "/usr/bin/bwrap",
            &arguments,
            Duration::from_secs(3),
            65_536,
            Some(&workspace),
        )?;
        Ok(PluginRun {
            started_at_ms,
            program: program.to_string_lossy().into_owned(),
            exit_code: output.code,
            output: format!("{}{}", output.stdout, output.stderr),
            sandbox: format!("Linux bubblewrap; private PID/mount/user namespaces; workspace_write={}; network={}; recovery journal masked; broker paths unavailable", capabilities.write_workspace, capabilities.network),
            broker_access: false,
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (workspace, program, capabilities);
        anyhow::bail!("native CLI sandbox is unavailable on this platform")
    }
}

// FVPROV1 7139 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNDcxZDlhMWE0YjZlNzRmYjc2ZjRlMzMwNTJmMWY3NDNmYzRiOGU0OWU5MzE0MjBmYzE3NGQyZjI5MTY4ZjFiNCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiNjVlMDdmOGFmZTQ2YjFhYjExYzBjOGZkNmJmNmU5MGZlYjc4MjBjNmJlMjRiYjNlNzVkOGQ0NjAwN2UzYjBkYzIyMzU1NTU2YjMxMTAxYzNhZTdkMzVjNGNmMjE2NGZmOWExMjQ3OTAwNTdjOGE1MmJkMGZhYjIzYjNkMWQ4MDQifV19XQ== FVPROV1-END
