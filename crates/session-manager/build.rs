// SPDX-License-Identifier: GPL-3.0-only
use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-env-changed=CC");
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("../..");
    let source = root.join("tools/mobile/terminal");
    println!("cargo:rerun-if-changed={}", source.display());
    let target = env::var("TARGET").unwrap();
    let host = env::var("HOST").unwrap();
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();
    let unix_helper = target_os == "macos" || target_os == "linux";
    if !unix_helper || target != host {
        println!("cargo:rustc-env=PLYRA_BUILT_HELPER=");
        if unix_helper {
            println!("cargo:warning=Cross-target builds use the portable PTY backend. Build the C helper natively or provide PLYRA_TERMINAL_HELPER at runtime.");
        }
        return;
    }
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("plyra-terminal-helper");
    let vendor = source.join("vendor/libvterm");
    let mut command = Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()));
    command.args([
        "-std=c99",
        "-O2",
        "-D_DARWIN_C_SOURCE",
        "-D_DEFAULT_SOURCE",
        "-D_XOPEN_SOURCE=700",
    ]);
    if target_os == "macos" {
        let deployment = env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| "11.0".into());
        command.arg(format!("-mmacosx-version-min={deployment}"));
    }
    command.arg("-I").arg(vendor.join("include"));
    command.arg("-I").arg(vendor.join("src"));
    command.arg(source.join("plyra_terminal.c"));
    let mut sources: Vec<_> = fs::read_dir(vendor.join("src"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "c"))
        .collect();
    sources.sort();
    command.args(sources);
    if target.contains("linux") {
        command.arg("-lutil");
    }
    command.arg("-o").arg(&output);
    let status = command
        .status()
        .expect("failed to execute the configured C compiler");
    assert!(
        status.success(),
        "building the native Plyra PTY helper failed"
    );
    println!("cargo:rustc-env=PLYRA_BUILT_HELPER={}", output.display());
}

// FVPROV1 2353 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiZGFiNGI0YTY2ZjAzMTVmZmJiODAyNDlkZDczNTk5ZWI4YWMxZjM3Mjk4MzEzM2Q3NWNjMGU3OWQ5ZTkyZTI3NiIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiNDk5MDEzZWU1YzkyNGFjZDRhYTVhNWIyM2VlYTYyMzdjNmIyNjRjNGIzNzIxNTEwODlmZmU1Nzc4ZTYyZjE4OTYyZjRlMzM5OWVjZWQyNDdlNzJmMTA2MmI2OTIxNWRmNTFhMmEzOWEwYjA0MmViNzFmYzVlMzhkZWY1NDRiMGYifV19XQ== FVPROV1-END
