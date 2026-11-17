// SPDX-License-Identifier: GPL-3.0-only
use std::path::PathBuf;

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").expect("Cargo target architecture");
    let filename = match arch.as_str() {
        "x86_64" => "windows-x86_64.obj",
        "aarch64" => "windows-aarch64.obj",
        _ => return,
    };
    let directory = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo package path"));
    let resource = directory.join("../../assets/app-icon").join(filename);
    println!("cargo:rerun-if-changed={}", resource.display());
    println!("cargo:rustc-link-arg-bin=glass-terminal={}", resource.display());
}

// FVPROV1 744 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiMjYxZDg5ZTNjMGZlNzdjZmQyMWY4OTI1YTYzZWU3NjU1MjU2ZTM5NDc5YTVlODhmMDNmZjE2N2EzOWEzZTM1OSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiY2I5ZTEwNTg4ZmNlMTM1OTdiYjdmNDRlN2VkYTZkMjFiMjMyZmFlNmY1MmZlMTVmYjZmZjA4NWI3MDMyNjRmM2VmZTE1YmNiZDYzYmFkNmE0YmFiZTIwOWRiNThjOTQzOTc4ZmIwMTkyYzAzNDQ1ZWQ0YTIwZDRhYWMxMDlmMGUifV19XQ== FVPROV1-END
