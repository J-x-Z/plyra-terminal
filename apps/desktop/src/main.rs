#![cfg_attr(windows, windows_subsystem = "windows")]

use anyhow::{bail, Context, Result};
use glass_terminal_desktop::{run, Backend, Options, Wake};
use std::path::PathBuf;
use winit::event_loop::EventLoop;

#[cfg(windows)]
mod windows_probe;

fn main() -> Result<()> {
    #[cfg(windows)]
    if std::env::args().nth(1).as_deref() == Some("--system-probe-sleeper") {
        anyhow::ensure!(
            std::env::args_os().len() == 2,
            "the native sleeper accepts no additional arguments"
        );
        windows_probe::run_sleeper();
        return Ok(());
    }
    #[cfg(windows)]
    let mut system_probe = None;
    #[cfg(windows)]
    let mut administrator_probe = false;
    let mut options = Options::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            #[cfg(windows)]
            "--system-probe" => {
                system_probe = Some(PathBuf::from(
                    args.next()
                        .context("--system-probe requires a new absolute output directory")?,
                ));
            }
            #[cfg(windows)]
            "--administrator" => administrator_probe = true,
            "--backend" => {
                options.backend = match args.next().as_deref() {
                    Some("auto") => Backend::Auto,
                    Some("helper") => Backend::Helper,
                    Some("portable") => Backend::Portable,
                    _ => bail!("--backend requires auto, helper, or portable"),
                };
            }
            "--shell" => {
                options.shell = Some(args.next().context("--shell requires an executable path")?)
            }
            "--shell-integration" => {
                options.shell_integration = Some(match args.next().as_deref() {
                    Some("none") => session_manager::ShellIntegration::None,
                    Some("aether_v1") => session_manager::ShellIntegration::AetherV1,
                    _ => bail!("--shell-integration requires none or aether_v1"),
                });
            }
            "--config" => {
                options.config_path = Some(PathBuf::from(
                    args.next()
                        .context("--config requires an absolute file path")?,
                ));
            }
            "--probe" => {
                options.probe_dir = Some(PathBuf::from(
                    args.next()
                        .context("--probe requires an output directory")?,
                ))
            }
            "--help" | "-h" => {
                println!("glass-terminal [--backend auto|helper|portable] [--shell PATH] [--shell-integration none|aether_v1] [--config ABSOLUTE_FILE] [--probe NEW_DIRECTORY]");
                #[cfg(windows)]
                println!("glass-terminal --system-probe NEW_ABSOLUTE_DIRECTORY [--administrator]");
                return Ok(());
            }
            _ => bail!("unknown argument {arg}"),
        }
    }
    #[cfg(windows)]
    {
        if let Some(path) = system_probe {
            anyhow::ensure!(
                path.is_absolute() && options.probe_dir.is_none(),
                "--system-probe requires an absolute new directory and cannot be combined with --probe"
            );
            return windows_probe::run(&path, administrator_probe);
        }
        anyhow::ensure!(
            !administrator_probe,
            "--administrator is only valid with --system-probe"
        );
    }
    if let Some(dir) = &options.probe_dir {
        std::fs::create_dir(dir).context("probe output directory must be new")?;
        #[cfg(windows)]
        {
            let root = std::env::var_os("SystemRoot")
                .context("SystemRoot is required for the Windows probe shell")?;
            let shell = PathBuf::from(root).join("System32").join("cmd.exe");
            anyhow::ensure!(
                shell.is_absolute() && shell.is_file(),
                "Windows probe requires an absolute system cmd.exe path"
            );
            options.shell = Some(shell.to_string_lossy().into_owned());
        }
        #[cfg(not(windows))]
        {
            options.shell = Some("/bin/sh".into());
        }
    }
    run(EventLoop::<Wake>::with_user_event().build()?, options)
}

// FVPROV1 4330 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiZGMzZTZiNDNmZDYxMmRiMjg4ZWQ1OTEwYWEyZGVkMzJhOTFiOTU2MGVmZDk1NTVkOTkwMDExMmZkN2Y0NzUxOCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiNjU2NzBlYTA1NGYwOWU5MDQyNjhkY2U4YTFkOGM5NTc0MjYxYjEzN2Y2ZjRiZWRjNzU2MjVlY2VhN2Y2ZDczNWVlYzIzNTQxYTk5NTcwMDUyZmZmNWY2NjBjZWYzMzllYzhhYjUxMzQxZTEwZjViN2VkMGEwN2IyZDI0MjFhMGMifV19XQ== FVPROV1-END
